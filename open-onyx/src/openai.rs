//! Adaptador compatible con API OpenAI (`/v1/chat/completions`) síncrono y streaming SSE.
//!
//! Soporta endpoints OpenAI, OpenRouter, Ollama (`http://localhost:11434/v1`), vLLM y LM Studio,
//! reutilizando `ContextBudget`, invalidación `superseded`, cola secuencial de tool calls y
//! `StreamPreviewHandle` para proyección en vivo sin mutar el determinismo del grafo.

use std::collections::{BTreeMap, HashSet};
use std::io::BufReader;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use onyx_soul_agents::{
    AgentView, ConceptId, EventBus, GraphView, PlatformDriver, Reply, Responder, SurfaceBinding,
    Vocab, pending_requests,
};
use onyx_workspace::WorkspaceTools;
use serde_json::{Value as JsonValue, json};

use crate::context::{
    ProjectedTurn, ReasoningEffort, SessionTelemetryHandle, TieredContextConfig,
    extract_openai_usage, project_memory,
};
use crate::gemini::{
    InFlightGuard, PendingCallQueue, QueuedToolCall, StreamPreviewHandle, next_queued_call,
    push_queued_calls, system_instruction_text,
};

pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o";
pub const DEFAULT_OPENAI_API_BASE: &str = "https://api.openai.com/v1";

#[derive(Clone, Default)]
struct OpenAiSharedState {
    pending_calls: PendingCallQueue,
    project_instructions: Arc<String>,
    telemetry: Option<SessionTelemetryHandle>,
    context_config: TieredContextConfig,
    effort: Option<ReasoningEffort>,
}

fn tool_call_args_as_string(raw: &JsonValue) -> String {
    match raw {
        JsonValue::String(s) => {
            if s.trim().is_empty() {
                "{}".to_string()
            } else {
                s.clone()
            }
        }
        JsonValue::Null => "{}".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

pub(crate) fn build_openai_request_body(
    req: &AgentView,
    model: &str,
    budget: TieredContextConfig,
    project_instructions: &str,
    effort: Option<ReasoningEffort>,
    stream: bool,
) -> JsonValue {
    let projected = project_memory(&req.goal, &req.memory, &budget);
    let mut messages = Vec::with_capacity(projected.turns.len() + 1);
    messages.push(json!({
        "role": "system",
        "content": system_instruction_text(project_instructions)
    }));

    for turn in projected.turns {
        match turn {
            ProjectedTurn::User(text) => messages.push(json!({
                "role": "user",
                "content": text
            })),
            ProjectedTurn::AssistantText(text) => messages.push(json!({
                "role": "assistant",
                "content": text
            })),
            ProjectedTurn::ToolCall {
                call_id,
                name,
                args_json,
                ..
            } => messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": args_json
                    }
                }]
            })),
            ProjectedTurn::ToolObserve {
                call_id,
                name,
                content,
                ..
            } => {
                if let Some(cid) = call_id {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": cid,
                        "name": name,
                        "content": content
                    }));
                } else {
                    messages.push(json!({
                        "role": "user",
                        "content": content
                    }));
                }
            }
            ProjectedTurn::ToolError {
                call_id,
                name,
                error,
            } => {
                let msg_text = format!("Tool error: {error}");
                if let Some(cid) = call_id {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": cid,
                        "name": name,
                        "content": msg_text
                    }));
                } else {
                    messages.push(json!({
                        "role": "user",
                        "content": msg_text
                    }));
                }
            }
            ProjectedTurn::ToolDenied {
                call_id,
                name,
                reason,
            } => {
                let msg_text = format!("Tool call denied: {reason}. Adjust your plan.");
                if let Some(cid) = call_id {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": cid,
                        "name": name,
                        "content": msg_text
                    }));
                } else {
                    messages.push(json!({
                        "role": "user",
                        "content": msg_text
                    }));
                }
            }
        }
    }

    let tools: Vec<JsonValue> = WorkspaceTools::schemas_with_subagents(true)
        .into_iter()
        .filter(|s| req.tools.iter().any(|t| t == s.name))
        .map(|s| {
            let mut params = s.parameters;
            lowercase_json_schema_types(&mut params);
            json!({
                "type": "function",
                "function": {
                    "name": s.name,
                    "description": s.description,
                    "parameters": params
                }
            })
        })
        .collect();

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": stream
    });
    if stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if !tools.is_empty() {
        body["tools"] = JsonValue::Array(tools);
    }
    if let Some(eff) = effort {
        body["reasoning_effort"] = json!(eff.as_str());
    }
    body
}

fn lowercase_json_schema_types(val: &mut JsonValue) {
    match val {
        JsonValue::Object(map) => {
            if let Some(JsonValue::String(ty)) = map.get_mut("type") {
                *ty = ty.to_ascii_lowercase();
            }
            for v in map.values_mut() {
                lowercase_json_schema_types(v);
            }
        }
        JsonValue::Array(arr) => {
            for v in arr {
                lowercase_json_schema_types(v);
            }
        }
        _ => {}
    }
}

pub(crate) fn parse_openai_response(
    json: &JsonValue,
    agent: ConceptId,
    request: u64,
    pending_calls: &PendingCallQueue,
    telemetry: Option<&SessionTelemetryHandle>,
) -> Reply {
    if let Some(usage) = extract_openai_usage(json) {
        if let Some(tel) = telemetry {
            tel.record_turn(usage);
        }
    }
    let message = &json["choices"][0]["message"];
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        if !tool_calls.is_empty() {
            let mut parsed = Vec::with_capacity(tool_calls.len());
            for tc in tool_calls {
                let name = tc["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let args = tool_call_args_as_string(&tc["function"]["arguments"]);
                if !name.is_empty() {
                    parsed.push(QueuedToolCall { name, args });
                }
            }
            if !parsed.is_empty() {
                let mut iter = parsed.into_iter();
                let first = iter.next().expect("non-empty tool_calls");
                push_queued_calls(pending_calls, agent, iter);
                return Reply::call_structured(agent, request, first.name, first.args);
            }
        }
    }

    let text = message["content"].as_str().unwrap_or_default().to_string();
    if text.is_empty() {
        let reason = json["choices"][0]["finish_reason"]
            .as_str()
            .unwrap_or("unknown");
        Reply::fail(
            agent,
            request,
            format!("empty openai response (finish_reason={reason}): {json}"),
        )
    } else {
        Reply::done(agent, request, text)
    }
}

const MAX_TRANSIENT_RETRIES: usize = 2;
const RETRY_BACKOFF_MS: [u64; MAX_TRANSIENT_RETRIES] = [400, 1200];

fn is_retryable_stream_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("decoding chunks")
        || lower.contains("unexpected eof")
        || lower.contains("early eof")
        || lower.contains("connection reset")
        || lower.contains("connection aborted")
        || lower.contains("broken pipe")
        || lower.contains("timed out")
}

fn can_accept_partial_sse_eof(
    saw_sse_data: bool,
    saw_done_marker: bool,
    has_finish_reason: bool,
    text_acc: &str,
    tool_calls_map: &BTreeMap<usize, (String, String)>,
) -> bool {
    if !saw_sse_data {
        return false;
    }
    if !saw_done_marker && !has_finish_reason {
        return false;
    }
    if tool_calls_map.is_empty() {
        return !text_acc.is_empty();
    }
    tool_calls_map.values().all(|(name, args)| {
        !name.trim().is_empty()
            && (args.trim().is_empty() || serde_json::from_str::<JsonValue>(args).is_ok())
    })
}

pub(crate) fn accumulate_openai_sse_reader<R: std::io::BufRead>(
    reader: R,
    agent: ConceptId,
    request: u64,
    preview: Option<&StreamPreviewHandle>,
) -> Result<JsonValue, String> {
    let mut text_acc = String::new();
    let mut thought_acc = String::new();
    let mut tool_calls_map: BTreeMap<usize, (String, String)> = BTreeMap::new();
    let mut last_finish_reason: Option<String> = None;
    let mut last_usage: Option<JsonValue> = None;
    let mut saw_sse_data = false;
    let mut saw_done_marker = false;
    let mut raw_fallback = String::new();

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(err) => {
                if can_accept_partial_sse_eof(
                    saw_sse_data,
                    saw_done_marker,
                    last_finish_reason.is_some(),
                    &text_acc,
                    &tool_calls_map,
                ) {
                    break;
                }
                return Err(err.to_string());
            }
        };
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("data:") {
            let data = rest.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                saw_done_marker = true;
                continue;
            }
            let chunk: JsonValue = serde_json::from_str(data).map_err(|e| e.to_string())?;
            saw_sse_data = true;
            if let Some(usage) = chunk.get("usage") {
                if !usage.is_null() {
                    last_usage = Some(usage.clone());
                }
            }
            if let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) {
                if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                    last_finish_reason = Some(fr.to_string());
                }
                if let Some(delta) = choice.get("delta") {
                    if let Some(reasoning) = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .and_then(|v| v.as_str())
                    {
                        thought_acc.push_str(reasoning);
                        if let Some(prev) = preview {
                            prev.set_thought(agent.0, request, &thought_acc);
                        }
                    }
                    if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                        text_acc.push_str(content);
                        if let Some(prev) = preview {
                            prev.set(agent.0, request, &text_acc);
                        }
                    }
                    if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            let idx =
                                tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            let entry = tool_calls_map.entry(idx).or_default();
                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                    entry.0.push_str(name);
                                }
                                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                    entry.1.push_str(args);
                                }
                            }
                            if text_acc.is_empty() && !entry.0.is_empty() {
                                if let Some(prev) = preview {
                                    prev.set(
                                        agent.0,
                                        request,
                                        format!("[calling `{}`...]", entry.0),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        } else if !saw_sse_data {
            raw_fallback.push_str(&line);
            raw_fallback.push('\n');
        }
    }

    if saw_sse_data {
        let mut msg = json!({
            "role": "assistant",
            "content": if text_acc.is_empty() { JsonValue::Null } else { JsonValue::String(text_acc) }
        });
        if !tool_calls_map.is_empty() {
            let tool_calls: Vec<JsonValue> = tool_calls_map
                .into_iter()
                .map(|(idx, (name, args))| {
                    let norm_args = if args.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        args
                    };
                    json!({
                        "id": format!("call_sse_{idx}"),
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": norm_args
                        }
                    })
                })
                .collect();
            msg["tool_calls"] = JsonValue::Array(tool_calls);
        }
        let mut out = json!({
            "choices": [{
                "message": msg,
                "finish_reason": last_finish_reason.unwrap_or_else(|| "stop".to_string())
            }]
        });
        if let Some(usage) = last_usage {
            out["usage"] = usage;
        }
        Ok(out)
    } else {
        serde_json::from_str(&raw_fallback).map_err(|e| e.to_string())
    }
}

struct OpenAiHttpError {
    reply: Reply,
    retryable: bool,
}

fn post_openai_json(
    url: &str,
    api_key: &str,
    body: &JsonValue,
    agent: ConceptId,
    request: u64,
) -> Result<ureq::Response, OpenAiHttpError> {
    let mut req = ureq::post(url);
    if !api_key.trim().is_empty() {
        req = req.set("Authorization", &format!("Bearer {api_key}"));
    }
    match req.send_json(body.clone()) {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, r)) => {
            let err_body = r.into_string().unwrap_or_default();
            let retryable = matches!(code, 408 | 429 | 500 | 502 | 503 | 504);
            Err(OpenAiHttpError {
                reply: Reply::fail(agent, request, format!("openai HTTP {code}: {err_body}")),
                retryable,
            })
        }
        Err(ureq::Error::Transport(t)) => Err(OpenAiHttpError {
            reply: Reply::fail(
                agent,
                request,
                format!(
                    "openai transport error ({:?}): {}",
                    t.kind(),
                    t.message().unwrap_or("request failed")
                ),
            ),
            retryable: true,
        }),
    }
}

fn execute_openai_http(
    url: &str,
    api_key: &str,
    body: JsonValue,
    agent: ConceptId,
    request: u64,
    shared: &OpenAiSharedState,
) -> Reply {
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        let resp = match post_openai_json(url, api_key, &body, agent, request) {
            Ok(r) => r,
            Err(err) => {
                if err.retryable {
                    if let Some(&delay_ms) = RETRY_BACKOFF_MS.get(attempt) {
                        thread::sleep(Duration::from_millis(delay_ms));
                        continue;
                    }
                }
                return err.reply;
            }
        };
        let json: JsonValue = match resp.into_json() {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                if is_retryable_stream_error(&msg) {
                    if let Some(&delay_ms) = RETRY_BACKOFF_MS.get(attempt) {
                        thread::sleep(Duration::from_millis(delay_ms));
                        continue;
                    }
                }
                return Reply::fail(agent, request, format!("openai json error: {msg}"));
            }
        };
        return parse_openai_response(
            &json,
            agent,
            request,
            &shared.pending_calls,
            shared.telemetry.as_ref(),
        );
    }
    Reply::fail(agent, request, "openai request exhausted retries")
}

struct OpenAiStreamTarget<'a> {
    url: &'a str,
    api_key: &'a str,
    agent: ConceptId,
    request: u64,
    shared: &'a OpenAiSharedState,
    preview: Option<&'a StreamPreviewHandle>,
}

fn execute_openai_stream_http(target: OpenAiStreamTarget<'_>, body: JsonValue) -> Reply {
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        let resp = match post_openai_json(
            target.url,
            target.api_key,
            &body,
            target.agent,
            target.request,
        ) {
            Ok(r) => r,
            Err(err) => {
                if let Some(prev) = target.preview {
                    prev.clear();
                }
                if err.retryable {
                    if let Some(&delay_ms) = RETRY_BACKOFF_MS.get(attempt) {
                        thread::sleep(Duration::from_millis(delay_ms));
                        continue;
                    }
                }
                return err.reply;
            }
        };
        let reader = BufReader::new(resp.into_reader());
        let json = match accumulate_openai_sse_reader(
            reader,
            target.agent,
            target.request,
            target.preview,
        ) {
            Ok(v) => v,
            Err(e) => {
                if let Some(prev) = target.preview {
                    prev.clear();
                }
                if is_retryable_stream_error(&e) {
                    if let Some(&delay_ms) = RETRY_BACKOFF_MS.get(attempt) {
                        thread::sleep(Duration::from_millis(delay_ms));
                        continue;
                    }
                }
                return Reply::fail(
                    target.agent,
                    target.request,
                    format!("openai stream error: {e}"),
                );
            }
        };
        if let Some(prev) = target.preview {
            prev.clear();
        }
        return parse_openai_response(
            &json,
            target.agent,
            target.request,
            &target.shared.pending_calls,
            target.shared.telemetry.as_ref(),
        );
    }
    Reply::fail(
        target.agent,
        target.request,
        "openai stream exhausted retries",
    )
}

/// Adaptador síncrono para endpoints compatibles con OpenAI `/v1/chat/completions`.
pub struct OpenAiResponder {
    api_key: String,
    model: String,
    api_base: String,
    shared: OpenAiSharedState,
}

impl OpenAiResponder {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            api_base: DEFAULT_OPENAI_API_BASE.to_string(),
            shared: OpenAiSharedState::default(),
        }
    }

    pub fn with_api_base(mut self, api_base: Option<String>) -> Self {
        if let Some(base) = api_base {
            let trimmed = base.trim().trim_end_matches('/');
            if !trimmed.is_empty() {
                self.api_base = trimmed.to_string();
            }
        }
        self
    }

    pub fn with_project_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.shared.project_instructions = Arc::new(instructions.into());
        self
    }

    pub fn with_telemetry(mut self, telemetry: SessionTelemetryHandle) -> Self {
        self.shared.telemetry = Some(telemetry);
        self
    }

    pub fn with_context_config(mut self, cfg: TieredContextConfig) -> Self {
        self.shared.context_config = cfg;
        self
    }

    pub fn with_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.shared.effort = effort;
        self
    }
}

impl Responder for OpenAiResponder {
    fn respond(&mut self, req: &AgentView) -> Option<Reply> {
        if let Some(queued_reply) = next_queued_call(&self.shared.pending_calls, req) {
            return Some(queued_reply);
        }
        let url = format!("{}/chat/completions", self.api_base);
        let body = build_openai_request_body(
            req,
            &self.model,
            self.shared.context_config,
            &self.shared.project_instructions,
            self.shared.effort,
            false,
        );
        Some(execute_openai_http(
            &url,
            &self.api_key,
            body,
            req.agent,
            req.request,
            &self.shared,
        ))
    }
}

/// Driver asíncrono con streaming SSE para endpoints compatibles con OpenAI en `Mode::Chat`.
pub struct AsyncOpenAiDriver {
    vocab: Vocab,
    api_key: String,
    model: String,
    api_base: String,
    shared: OpenAiSharedState,
    dispatched: HashSet<(u64, u64)>,
    in_flight_count: Arc<AtomicUsize>,
    has_ready: Arc<AtomicBool>,
    stream_preview: Option<StreamPreviewHandle>,
    rx: mpsc::Receiver<Reply>,
    tx: mpsc::Sender<Reply>,
    inner: Option<Box<dyn PlatformDriver>>,
}

impl AsyncOpenAiDriver {
    pub fn new(
        vocab: Vocab,
        api_key: impl Into<String>,
        model: impl Into<String>,
        in_flight_count: Arc<AtomicUsize>,
        has_ready: Arc<AtomicBool>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            vocab,
            api_key: api_key.into(),
            model: model.into(),
            api_base: DEFAULT_OPENAI_API_BASE.to_string(),
            shared: OpenAiSharedState::default(),
            dispatched: HashSet::new(),
            in_flight_count,
            has_ready,
            stream_preview: None,
            rx,
            tx,
            inner: None,
        }
    }

    pub fn wrapping(mut self, inner: impl PlatformDriver + 'static) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

    pub fn with_api_base(mut self, api_base: Option<String>) -> Self {
        if let Some(base) = api_base {
            let trimmed = base.trim().trim_end_matches('/');
            if !trimmed.is_empty() {
                self.api_base = trimmed.to_string();
            }
        }
        self
    }

    pub fn with_project_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.shared.project_instructions = Arc::new(instructions.into());
        self
    }

    pub fn with_stream_preview(mut self, preview: StreamPreviewHandle) -> Self {
        self.stream_preview = Some(preview);
        self
    }

    pub fn with_telemetry(mut self, telemetry: SessionTelemetryHandle) -> Self {
        self.shared.telemetry = Some(telemetry);
        self
    }

    pub fn with_context_config(mut self, cfg: TieredContextConfig) -> Self {
        self.shared.context_config = cfg;
        self
    }

    pub fn with_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.shared.effort = effort;
        self
    }
}

impl PlatformDriver for AsyncOpenAiDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Some(inner) = self.inner.as_mut() {
            inner.poll(bus);
        }
        self.has_ready.store(false, Ordering::Release);
        while let Ok(reply) = self.rx.try_recv() {
            if let Some(prev) = &self.stream_preview {
                prev.clear();
            }
            bus.push(reply.to_event());
        }
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
        for req in pending_requests(graph, &self.vocab) {
            let key = (req.agent.0, req.request);
            if !self.dispatched.insert(key) {
                continue;
            }
            if let Some(queued_reply) = next_queued_call(&self.shared.pending_calls, &req) {
                if self.tx.send(queued_reply).is_ok() {
                    self.has_ready.store(true, Ordering::Release);
                }
                continue;
            }

            self.in_flight_count.fetch_add(1, Ordering::Release);
            let guard = InFlightGuard(Arc::clone(&self.in_flight_count));
            let url = format!("{}/chat/completions", self.api_base);
            let body = build_openai_request_body(
                &req,
                &self.model,
                self.shared.context_config,
                &self.shared.project_instructions,
                self.shared.effort,
                true,
            );
            let api_key = self.api_key.clone();
            let shared = self.shared.clone();
            let ready_flag = Arc::clone(&self.has_ready);
            let tx = self.tx.clone();
            let preview = self.stream_preview.clone();
            let agent = req.agent;
            let request = req.request;

            thread::spawn(move || {
                let _guard = guard;
                let reply = execute_openai_stream_http(
                    OpenAiStreamTarget {
                        url: &url,
                        api_key: &api_key,
                        agent,
                        request,
                        shared: &shared,
                        preview: preview.as_ref(),
                    },
                    body,
                );
                if tx.send(reply).is_ok() {
                    ready_flag.store(true, Ordering::Release);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_agent_view(agent_id: u64, request: u64) -> AgentView {
        AgentView {
            agent: ConceptId(agent_id),
            request,
            name: "test".to_string(),
            goal: "inspect crate".to_string(),
            tools: vec!["fs_read".to_string(), "fs_edit".to_string()],
            memory: vec![],
            pending_approval: None,
            props: vec![],
        }
    }

    #[test]
    fn openai_request_body_maps_tools_and_memory_with_lowercase_schemas() {
        let mut req = dummy_agent_view(1, 3);
        req.memory = vec![
            (
                "call".to_string(),
                r#"fs_read {"path":"src/lib.rs"}"#.to_string(),
            ),
            ("observe".to_string(), "OLD_READ".to_string()),
            (
                "call".to_string(),
                r#"fs_edit {"path":"src/lib.rs","old_text":"a","new_text":"b"}"#.to_string(),
            ),
            ("observe".to_string(), "edited".to_string()),
            ("denied".to_string(), "user declined write".to_string()),
        ];

        let body = build_openai_request_body(
            &req,
            "gpt-4o",
            TieredContextConfig::default(),
            "Project notes.",
            Some(ReasoningEffort::Medium),
            true,
        );
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["stream_options"]["include_usage"], true);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "fs_read");
        assert_eq!(msgs[3]["role"], "tool");
        assert!(msgs[3]["content"].as_str().unwrap().contains("superseded"));
        // Unpaired denial falls back safely to `user` role rather than orphan `tool` role.
        assert_eq!(msgs.last().unwrap()["role"], "user");

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn openai_sse_reader_accumulates_content_deltas_and_chunked_tool_calls() {
        let preview = StreamPreviewHandle::new();
        let telemetry = SessionTelemetryHandle::new("gpt-4o", false);
        let text_sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"plan first \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hola \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"OpenAI!\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":6400,\"completion_tokens\":210,\"prompt_tokens_details\":{\"cached_tokens\":3200}}}\n\n",
            "data: [DONE]\n"
        );
        let pending = PendingCallQueue::default();
        let json = accumulate_openai_sse_reader(
            BufReader::new(text_sse.as_bytes()),
            ConceptId(2),
            5,
            Some(&preview),
        )
        .unwrap();
        let prev_snap = preview.get().unwrap();
        assert_eq!(prev_snap.text, "Hola OpenAI!");
        assert_eq!(prev_snap.thought, "plan first ");
        let reply = parse_openai_response(&json, ConceptId(2), 5, &pending, Some(&telemetry));
        assert_eq!(reply, Reply::done(ConceptId(2), 5, "Hola OpenAI!"));
        let snap = telemetry.snapshot();
        assert_eq!(snap.last_turn.input_tokens, 6_400);
        assert_eq!(snap.last_turn.cached_input_tokens, 3_200);
        assert_eq!(snap.last_turn.output_tokens, 210);
        assert!(snap.format_status_bar().contains("cache 3.2k"));

        preview.clear();
        let tool_sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"fs_read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"lib.rs\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"name\":\"git_status\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: [DONE]\n"
        );
        let tool_json = accumulate_openai_sse_reader(
            BufReader::new(tool_sse.as_bytes()),
            ConceptId(2),
            6,
            Some(&preview),
        )
        .unwrap();
        let r1 = parse_openai_response(&tool_json, ConceptId(2), 6, &pending, None);
        assert!(format!("{r1:?}").contains("fs_read"), "{r1:?}");
        assert!(format!("{r1:?}").contains("lib.rs"), "{r1:?}");

        let next = next_queued_call(&pending, &dummy_agent_view(2, 7)).unwrap();
        assert!(format!("{next:?}").contains("git_status"), "{next:?}");
    }

    #[test]
    fn openai_sse_tolerates_chunk_decode_error_after_finish_reason_and_flags_truncated_streams() {
        struct ChunkErrorReader {
            data: Vec<u8>,
            pos: usize,
        }
        impl std::io::Read for ChunkErrorReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.pos < self.data.len() {
                    let n = (self.data.len() - self.pos).min(buf.len());
                    buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                    self.pos += n;
                    Ok(n)
                } else {
                    Err(std::io::Error::other("Error while decoding chunks"))
                }
            }
        }

        // Complete response followed by abrupt proxy chunk termination must succeed cleanly.
        let complete = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"42 tildes\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        );
        let reader_ok = BufReader::new(ChunkErrorReader {
            data: complete.as_bytes().to_vec(),
            pos: 0,
        });
        let json = accumulate_openai_sse_reader(reader_ok, ConceptId(2), 1, None).unwrap();
        let reply =
            parse_openai_response(&json, ConceptId(2), 1, &PendingCallQueue::default(), None);
        assert_eq!(reply, Reply::done(ConceptId(2), 1, "42 tildes"));

        // Mid-stream truncation before `finish_reason` must return a retryable error.
        let truncated = "data: {\"choices\":[{\"delta\":{\"content\":\"incompl\"}}]}\n\n";
        let reader_err = BufReader::new(ChunkErrorReader {
            data: truncated.as_bytes().to_vec(),
            pos: 0,
        });
        let err = accumulate_openai_sse_reader(reader_err, ConceptId(2), 2, None).unwrap_err();
        assert!(is_retryable_stream_error(&err), "{err}");
    }
}
