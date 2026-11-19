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

use onyx_soul_agents::{
    AgentView, ConceptId, EventBus, GraphView, PlatformDriver, Reply, Responder, SurfaceBinding,
    Vocab, pending_requests,
};
use onyx_workspace::WorkspaceTools;
use serde_json::{Value as JsonValue, json};

use crate::gemini::{
    ContextBudget, InFlightGuard, PendingCallQueue, QueuedToolCall, StreamPreviewHandle,
    find_superseded_reads, next_queued_call, push_queued_calls, system_instruction_text,
};

pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o";
pub const DEFAULT_OPENAI_API_BASE: &str = "https://api.openai.com/v1";

#[derive(Clone, Default)]
struct OpenAiSharedState {
    pending_calls: PendingCallQueue,
    project_instructions: Arc<String>,
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
    budget: ContextBudget,
    project_instructions: &str,
    stream: bool,
) -> JsonValue {
    struct ProjectedEntry {
        msg: JsonValue,
        tool_name: String,
        tool_call_id: String,
        bytes: usize,
        is_observe: bool,
        is_toon: bool,
        is_superseded: bool,
    }

    let superseded = find_superseded_reads(&req.memory);
    let mut entries: Vec<ProjectedEntry> = Vec::with_capacity(req.memory.len() + 1);
    let goal_text = format!("Goal: {}", req.goal);
    entries.push(ProjectedEntry {
        bytes: goal_text.len(),
        msg: json!({
            "role": "user",
            "content": goal_text
        }),
        tool_name: String::new(),
        tool_call_id: String::new(),
        is_observe: false,
        is_toon: false,
        is_superseded: false,
    });

    let mut last_tool: Option<(String, String)> = None;
    let mut call_index = 0usize;
    for (mem_idx, (kind, text)) in req.memory.iter().enumerate() {
        match kind.as_str() {
            "user" | "system" => entries.push(ProjectedEntry {
                bytes: text.len(),
                msg: json!({
                    "role": "user",
                    "content": text
                }),
                tool_name: String::new(),
                tool_call_id: String::new(),
                is_observe: false,
                is_toon: false,
                is_superseded: false,
            }),
            "say" => entries.push(ProjectedEntry {
                bytes: text.len(),
                msg: json!({
                    "role": "assistant",
                    "content": text
                }),
                tool_name: String::new(),
                tool_call_id: String::new(),
                is_observe: false,
                is_toon: false,
                is_superseded: false,
            }),
            "call" => {
                let (tool, args) = match text.split_once(' ') {
                    Some((t, a)) => (t, a),
                    None => (text.as_str(), "{}"),
                };
                let args_norm = if args.trim().is_empty() { "{}" } else { args };
                let call_id = format!("call_{call_index}");
                call_index += 1;
                last_tool = Some((tool.to_string(), call_id.clone()));
                entries.push(ProjectedEntry {
                    bytes: tool.len() + args_norm.len(),
                    msg: json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": tool,
                                "arguments": args_norm
                            }
                        }]
                    }),
                    tool_name: tool.to_string(),
                    tool_call_id: String::new(),
                    is_observe: false,
                    is_toon: false,
                    is_superseded: false,
                });
            }
            "observe" => {
                if let Some((name, call_id)) = last_tool.take() {
                    if let Some(sup_path) = superseded.get(&mem_idx) {
                        let marker = format!(
                            "[superseded: prior fs_read of `{sup_path}` was invalidated by a subsequent edit]"
                        );
                        entries.push(ProjectedEntry {
                            bytes: marker.len(),
                            msg: json!({
                                "role": "tool",
                                "tool_call_id": call_id,
                                "name": name,
                                "content": marker
                            }),
                            tool_name: name,
                            tool_call_id: call_id,
                            is_observe: true,
                            is_toon: false,
                            is_superseded: true,
                        });
                    } else {
                        let is_toon = text.trim_start().starts_with("@TOON v1");
                        entries.push(ProjectedEntry {
                            bytes: text.len(),
                            msg: json!({
                                "role": "tool",
                                "tool_call_id": call_id,
                                "name": name,
                                "content": text
                            }),
                            tool_name: name,
                            tool_call_id: call_id,
                            is_observe: true,
                            is_toon,
                            is_superseded: false,
                        });
                    }
                } else {
                    entries.push(ProjectedEntry {
                        bytes: text.len(),
                        msg: json!({
                            "role": "user",
                            "content": text
                        }),
                        tool_name: String::new(),
                        tool_call_id: String::new(),
                        is_observe: false,
                        is_toon: false,
                        is_superseded: false,
                    });
                }
            }
            "tool_error" | "denied" => {
                let msg_text = if kind == "denied" {
                    format!("Tool call denied: {text}. Adjust your plan.")
                } else {
                    format!("Tool error: {text}")
                };
                if let Some((name, call_id)) = last_tool.take() {
                    entries.push(ProjectedEntry {
                        bytes: msg_text.len(),
                        msg: json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "name": name,
                            "content": msg_text
                        }),
                        tool_name: name,
                        tool_call_id: call_id,
                        is_observe: false,
                        is_toon: false,
                        is_superseded: false,
                    });
                } else {
                    entries.push(ProjectedEntry {
                        bytes: msg_text.len(),
                        msg: json!({
                            "role": "user",
                            "content": msg_text
                        }),
                        tool_name: String::new(),
                        tool_call_id: String::new(),
                        is_observe: false,
                        is_toon: false,
                        is_superseded: false,
                    });
                }
            }
            _ => {}
        }
    }

    let mut total_bytes: usize = entries.iter().map(|e| e.bytes).sum();
    let keep_tail_from = entries.len().saturating_sub(4);
    if total_bytes > budget.max_memory_bytes {
        for pass_toon in [false, true] {
            for entry in entries.iter_mut().take(keep_tail_from) {
                if total_bytes <= budget.max_memory_bytes {
                    break;
                }
                if !entry.is_observe || entry.is_superseded || entry.is_toon != pass_toon {
                    continue;
                }
                let marker = format!(
                    "[trimmed by ContextBudget: prior `{}` output ({} bytes) omitted]",
                    entry.tool_name, entry.bytes
                );
                if marker.len() < entry.bytes {
                    total_bytes = total_bytes - entry.bytes + marker.len();
                    entry.bytes = marker.len();
                    entry.msg = json!({
                        "role": "tool",
                        "tool_call_id": entry.tool_call_id,
                        "name": entry.tool_name,
                        "content": marker
                    });
                }
            }
        }
    }

    let mut messages = Vec::with_capacity(entries.len() + 1);
    messages.push(json!({
        "role": "system",
        "content": system_instruction_text(project_instructions)
    }));
    messages.extend(entries.into_iter().map(|e| e.msg));

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
    if !tools.is_empty() {
        body["tools"] = JsonValue::Array(tools);
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
) -> Reply {
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

pub(crate) fn accumulate_openai_sse_reader<R: std::io::BufRead>(
    reader: R,
    agent: ConceptId,
    request: u64,
    preview: Option<&StreamPreviewHandle>,
) -> Result<JsonValue, String> {
    let mut text_acc = String::new();
    let mut tool_calls_map: BTreeMap<usize, (String, String)> = BTreeMap::new();
    let mut last_finish_reason: Option<String> = None;
    let mut saw_sse_data = false;
    let mut raw_fallback = String::new();

    for line_res in reader.lines() {
        let line = line_res.map_err(|e| e.to_string())?;
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("data:") {
            let data = rest.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let chunk: JsonValue = serde_json::from_str(data).map_err(|e| e.to_string())?;
            saw_sse_data = true;
            if let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) {
                if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                    last_finish_reason = Some(fr.to_string());
                }
                if let Some(delta) = choice.get("delta") {
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
        Ok(json!({
            "choices": [{
                "message": msg,
                "finish_reason": last_finish_reason.unwrap_or_else(|| "stop".to_string())
            }]
        }))
    } else {
        serde_json::from_str(&raw_fallback).map_err(|e| e.to_string())
    }
}

fn post_openai_json(
    url: &str,
    api_key: &str,
    body: JsonValue,
    agent: ConceptId,
    request: u64,
) -> Result<ureq::Response, Reply> {
    let mut req = ureq::post(url);
    if !api_key.trim().is_empty() {
        req = req.set("Authorization", &format!("Bearer {api_key}"));
    }
    match req.send_json(body) {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, r)) => {
            let err_body = r.into_string().unwrap_or_default();
            Err(Reply::fail(
                agent,
                request,
                format!("openai HTTP {code}: {err_body}"),
            ))
        }
        Err(ureq::Error::Transport(t)) => Err(Reply::fail(
            agent,
            request,
            format!(
                "openai transport error ({:?}): {}",
                t.kind(),
                t.message().unwrap_or("request failed")
            ),
        )),
    }
}

fn execute_openai_http(
    url: &str,
    api_key: &str,
    body: JsonValue,
    agent: ConceptId,
    request: u64,
    pending_calls: &PendingCallQueue,
) -> Reply {
    let resp = match post_openai_json(url, api_key, body, agent, request) {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    let json: JsonValue = match resp.into_json() {
        Ok(v) => v,
        Err(e) => return Reply::fail(agent, request, format!("openai json error: {e}")),
    };
    parse_openai_response(&json, agent, request, pending_calls)
}

fn execute_openai_stream_http(
    url: &str,
    api_key: &str,
    body: JsonValue,
    agent: ConceptId,
    request: u64,
    pending_calls: &PendingCallQueue,
    preview: Option<&StreamPreviewHandle>,
) -> Reply {
    let resp = match post_openai_json(url, api_key, body, agent, request) {
        Ok(r) => r,
        Err(reply) => {
            if let Some(prev) = preview {
                prev.clear();
            }
            return reply;
        }
    };
    let reader = BufReader::new(resp.into_reader());
    let json = match accumulate_openai_sse_reader(reader, agent, request, preview) {
        Ok(v) => v,
        Err(e) => {
            if let Some(prev) = preview {
                prev.clear();
            }
            return Reply::fail(agent, request, format!("openai stream error: {e}"));
        }
    };
    if let Some(prev) = preview {
        prev.clear();
    }
    parse_openai_response(&json, agent, request, pending_calls)
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
            ContextBudget::default(),
            &self.shared.project_instructions,
            false,
        );
        Some(execute_openai_http(
            &url,
            &self.api_key,
            body,
            req.agent,
            req.request,
            &self.shared.pending_calls,
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
                ContextBudget::default(),
                &self.shared.project_instructions,
                true,
            );
            let api_key = self.api_key.clone();
            let pending = Arc::clone(&self.shared.pending_calls);
            let ready_flag = Arc::clone(&self.has_ready);
            let tx = self.tx.clone();
            let preview = self.stream_preview.clone();
            let agent = req.agent;
            let request = req.request;

            thread::spawn(move || {
                let _guard = guard;
                let reply = execute_openai_stream_http(
                    &url,
                    &api_key,
                    body,
                    agent,
                    request,
                    &pending,
                    preview.as_ref(),
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
            ContextBudget::default(),
            "Project notes.",
            true,
        );
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
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
        let text_sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hola \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"OpenAI!\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
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
        assert_eq!(preview.get().unwrap().text, "Hola OpenAI!");
        let reply = parse_openai_response(&json, ConceptId(2), 5, &pending);
        assert_eq!(reply, Reply::done(ConceptId(2), 5, "Hola OpenAI!"));

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
        let r1 = parse_openai_response(&tool_json, ConceptId(2), 6, &pending);
        assert!(format!("{r1:?}").contains("fs_read"), "{r1:?}");
        assert!(format!("{r1:?}").contains("lib.rs"), "{r1:?}");

        let next = next_queued_call(&pending, &dummy_agent_view(2, 7)).unwrap();
        assert!(format!("{next:?}").contains("git_status"), "{next:?}");
    }
}
