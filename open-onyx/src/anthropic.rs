//! Adaptador nativo para la API Anthropic Messages (`/v1/messages`) síncrono y streaming SSE.
//! (`SDD-04` `REQ-04.1`.)
//!
//! Cabeceras obligatorias: `x-api-key`, `anthropic-version: 2023-06-01` y
//! `content-type: application/json`. El streaming usa eventos `event:` + `data:`
//! (`message_start` / `content_block_delta` / `message_delta`), reutilizando
//! `ContextBudget`, cola secuencial de tool calls y `StreamPreviewHandle` sin
//! mutar el determinismo del grafo. Los reintentos ante `429` / `529`
//! (`overloaded_error`) respetan `Retry-After`.
//!
//! Nota de ids: la API Anthropic exige que cada `tool_result.tool_use_id` coincida
//! con el `tool_use.id` de la respuesta asistente **previa en la misma historia**.
//! Como el harness asigna ids propios (`call_N`), ambos campos se proyectan desde
//! la memoria de la sesión (coherente por construcción) y no desde el id `toolu_*`
//! emitido en el stream.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use onyx_soul_agents::{AgentView, ConceptId, Reply, Responder};
use onyx_workspace::WorkspaceTools;
use serde_json::{Value as JsonValue, json};

use crate::context::{
    ProjectedTurn, SessionTelemetryHandle, TieredContextConfig, TokenUsage, project_memory,
};
use crate::gemini::{
    PendingCallQueue, QueuedToolCall, StreamPreviewHandle, next_queued_call, push_queued_calls,
    system_instruction_text,
};
use crate::llm_profile::{api_base_host, is_loopback_host};
use crate::openai::{
    MAX_TRANSIENT_RETRIES, RETRY_BACKOFF_MS, is_retryable_stream_error, lowercase_json_schema_types,
};

pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5";
pub const DEFAULT_ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// `max_tokens` por defecto (obligatorio en la API Messages; sin override en Fase 1).
const DEFAULT_MAX_TOKENS: u64 = 8192;

/// Estado compartido por los hilos de despacho Anthropic (cola secuencial de tool
/// calls, telemetría). `pub(crate)` para que `DynamicLlmDriver` lo construya con la
/// cola de tool calls compartida con Gemini y OpenAI.
#[derive(Clone, Default)]
pub(crate) struct AnthropicSharedState {
    pub(crate) pending_calls: PendingCallQueue,
    pub(crate) project_instructions: Arc<String>,
    pub(crate) telemetry: Option<SessionTelemetryHandle>,
    pub(crate) context_config: TieredContextConfig,
    /// ids de drivers de harness CLI externos habilitados (`SDD-10` Fase 1): enriquecen el
    /// esquema dinámico de `spawn_subagent` (`driver` opcional).
    pub(crate) driver_ids: Vec<String>,
}

/// Bloque de contenido asistente en curso de un stream `content_block_*`.
enum BlockState {
    Text,
    Thinking,
    ToolUse { name: String, args: String },
}

/// Resultado normalizado de una respuesta Anthropic (SSE acumulado u objeto
/// `message` completo), agnóstico de transporte.
#[derive(Debug)]
pub(crate) struct AnthropicStreamResult {
    pub(crate) text: String,
    pub(crate) thought: String,
    pub(crate) tool_calls: Vec<QueuedToolCall>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) usage: Option<TokenUsage>,
}

fn tool_call_args_as_string(args: &str) -> String {
    if args.trim().is_empty() {
        "{}".to_string()
    } else {
        args.to_string()
    }
}

/// Extrae `TokenUsage` desde el `usage` de `message_start` (input/cache).
fn extract_anthropic_start_usage(usage: &JsonValue) -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cached_input_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        reasoning_tokens: 0,
    })
}

/// Fusiona mensajes consecutivos del mismo rol (Anthropic los rechaza):
/// contenidos en texto se convierten en bloques `text` y se concatenan.
fn merge_consecutive_roles(messages: &mut Vec<JsonValue>) {
    let mut out: Vec<JsonValue> = Vec::with_capacity(messages.len());
    for msg in messages.drain(..) {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(last) = out.last_mut() {
            if last.get("role").and_then(|v| v.as_str()) == Some(role) {
                let last_blocks = match last.get("content") {
                    Some(JsonValue::Array(a)) => a.clone(),
                    Some(JsonValue::String(s)) => vec![json!({"type": "text", "text": s})],
                    _ => Vec::new(),
                };
                let blocks = match msg.get("content").cloned() {
                    Some(JsonValue::String(s)) => vec![json!({"type": "text", "text": s})],
                    Some(JsonValue::Array(a)) => a,
                    Some(other) => vec![json!({"type": "text", "text": other.to_string()})],
                    None => Vec::new(),
                };
                let mut merged = last_blocks;
                merged.extend(blocks);
                last["content"] = JsonValue::Array(merged);
                continue;
            }
        }
        out.push(msg);
    }
    *messages = out;
}

/// Añade un mensaje `user` con contenido texto o bloques (p. ej. `tool_result`).
fn push_user_message(messages: &mut Vec<JsonValue>, content: JsonValue) {
    messages.push(json!({"role": "user", "content": content}));
}

pub(crate) fn build_anthropic_request_body(
    req: &AgentView,
    model: &str,
    budget: TieredContextConfig,
    project_instructions: &str,
    stream: bool,
    driver_ids: &[String],
) -> JsonValue {
    let projected = project_memory(&req.goal, &req.memory, &budget);
    let mut messages: Vec<JsonValue> = Vec::with_capacity(projected.turns.len() + 1);

    for turn in projected.turns {
        match turn {
            ProjectedTurn::User(text) => push_user_message(&mut messages, JsonValue::String(text)),
            ProjectedTurn::AssistantText(text) => {
                messages.push(json!({"role": "assistant", "content": text}));
            }
            ProjectedTurn::ToolCall {
                call_id,
                name,
                args_json,
                ..
            } => {
                let input =
                    serde_json::from_str::<JsonValue>(&args_json).unwrap_or_else(|_| json!({}));
                messages.push(json!({
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": input
                    }]
                }));
            }
            ProjectedTurn::ToolObserve {
                call_id, content, ..
            } => match call_id {
                Some(cid) => push_user_message(
                    &mut messages,
                    json!([{ "type": "tool_result", "tool_use_id": cid, "content": content }]),
                ),
                None => push_user_message(&mut messages, JsonValue::String(content)),
            },
            ProjectedTurn::ToolError { call_id, error, .. } => {
                let msg_text = format!("Tool error: {error}");
                match call_id {
                    Some(cid) => push_user_message(
                        &mut messages,
                        json!([{ "type": "tool_result", "tool_use_id": cid, "content": msg_text, "is_error": true }]),
                    ),
                    None => push_user_message(&mut messages, JsonValue::String(msg_text)),
                }
            }
            ProjectedTurn::ToolDenied {
                call_id, reason, ..
            } => {
                let msg_text = format!("Tool call denied: {reason}. Adjust your plan.");
                match call_id {
                    Some(cid) => push_user_message(
                        &mut messages,
                        json!([{ "type": "tool_result", "tool_use_id": cid, "content": msg_text, "is_error": true }]),
                    ),
                    None => push_user_message(&mut messages, JsonValue::String(msg_text)),
                }
            }
        }
    }
    merge_consecutive_roles(&mut messages);

    let tools: Vec<JsonValue> =
        WorkspaceTools::schemas_with_subagents_and_drivers(true, driver_ids)
            .into_iter()
            .filter(|s| req.tools.iter().any(|t| t == s.name))
            .map(|s| {
                let mut params = s.parameters;
                lowercase_json_schema_types(&mut params);
                json!({
                    "name": s.name,
                    "description": s.description,
                    "input_schema": params
                })
            })
            .collect();

    let system = json!([{ "type": "text", "text": system_instruction_text(project_instructions) }]);

    let mut body = json!({
        "model": model,
        "max_tokens": DEFAULT_MAX_TOKENS,
        "system": system,
        "messages": messages,
        "stream": stream
    });
    if !tools.is_empty() {
        body["tools"] = JsonValue::Array(tools);
    }
    body
}

/// Parsea un objeto `message` completo (respuesta no-stream de `/v1/messages`)
/// a [`AnthropicStreamResult`].
pub(crate) fn parse_anthropic_message(json: &JsonValue) -> AnthropicStreamResult {
    let mut result = AnthropicStreamResult {
        text: String::new(),
        thought: String::new(),
        tool_calls: Vec::new(),
        stop_reason: None,
        usage: None,
    };
    if let Some(usage) = json.get("usage") {
        result.usage = extract_anthropic_start_usage(usage);
    }
    if let Some(reason) = json.get("stop_reason").and_then(|v| v.as_str()) {
        result.stop_reason = Some(reason.to_string());
    }
    if let Some(blocks) = json.get("content").and_then(|v| v.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        result.text.push_str(t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                        result.thought.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    let args = match block.get("input") {
                        Some(JsonValue::Object(_)) | Some(JsonValue::Array(_)) => {
                            serde_json::to_string(block.get("input").unwrap())
                                .unwrap_or_else(|_| "{}".to_string())
                        }
                        Some(JsonValue::String(s)) => s.clone(),
                        _ => "{}".to_string(),
                    };
                    result.tool_calls.push(QueuedToolCall {
                        name: name.to_string(),
                        args,
                    });
                }
                _ => {}
            }
        }
    }
    result
}

/// Acumula un stream SSE de `/v1/messages` (eventos `event:` + `data:`) a
/// [`AnthropicStreamResult`], proyectando `text_delta` / `thinking_delta` en vivo
/// a `preview`.
pub(crate) fn accumulate_anthropic_sse_reader<R: std::io::BufRead>(
    reader: R,
    agent: ConceptId,
    request: u64,
    preview: Option<&StreamPreviewHandle>,
) -> Result<AnthropicStreamResult, String> {
    let mut result = AnthropicStreamResult {
        text: String::new(),
        thought: String::new(),
        tool_calls: Vec::new(),
        stop_reason: None,
        usage: None,
    };
    let mut blocks: BTreeMap<usize, BlockState> = BTreeMap::new();
    let mut event_name: Option<String> = None;
    let mut saw_data = false;
    let mut raw_fallback = String::new();

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(err) => {
                // Un stream que termina tras haber entregado texto, razonamiento
                // o tool calls completos (y `message_delta`) se acepta como parcial.
                if result.text.is_empty()
                    && result.thought.is_empty()
                    && result.tool_calls.is_empty()
                {
                    return Err(err.to_string());
                }
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("event:") {
            event_name = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("data:") {
            let data = rest.trim();
            if data.is_empty() {
                continue;
            }
            let payload: JsonValue = serde_json::from_str(data).map_err(|e| e.to_string())?;
            saw_data = true;
            let typ = payload
                .get("type")
                .and_then(|v| v.as_str())
                .or(event_name.as_deref())
                .unwrap_or_default();
            match typ {
                "message_start" => {
                    if let Some(usage) = payload.pointer("/message/usage") {
                        result.usage = extract_anthropic_start_usage(usage);
                    }
                }
                "content_block_start" => {
                    let index = payload.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let block = &payload["content_block"];
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            blocks.insert(
                                index,
                                BlockState::ToolUse {
                                    name: name.to_string(),
                                    args: String::new(),
                                },
                            );
                            if !name.is_empty() && result.text.is_empty() {
                                if let Some(prev) = preview {
                                    prev.set(agent.0, request, format!("[calling `{name}`...]"));
                                }
                            }
                        }
                        Some("thinking") => {
                            blocks.insert(index, BlockState::Thinking);
                        }
                        _ => {
                            blocks.insert(index, BlockState::Text);
                        }
                    }
                }
                "content_block_delta" => {
                    let index = payload.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let delta = &payload["delta"];
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => {
                            let t = delta
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            result.text.push_str(t);
                            blocks.insert(index, BlockState::Text);
                            if let Some(prev) = preview {
                                prev.set(agent.0, request, &result.text);
                            }
                        }
                        Some("thinking_delta") => {
                            let t = delta
                                .get("thinking")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            result.thought.push_str(t);
                            blocks.insert(index, BlockState::Thinking);
                            if let Some(prev) = preview {
                                prev.set_thought(agent.0, request, &result.thought);
                            }
                        }
                        Some("input_json_delta") => {
                            let partial = delta
                                .get("partial_json")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            if let Some(BlockState::ToolUse { args, .. }) = blocks.get_mut(&index) {
                                args.push_str(partial);
                            }
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    if let Some(reason) = payload
                        .pointer("/delta/stop_reason")
                        .and_then(|v| v.as_str())
                    {
                        result.stop_reason = Some(reason.to_string());
                    }
                    if let Some(usage) = payload.get("usage") {
                        if let Some(u) = extract_anthropic_start_usage(usage) {
                            result.usage = Some(match result.usage.take() {
                                Some(mut acc) => {
                                    acc.output_tokens = u.output_tokens;
                                    acc
                                }
                                None => u,
                            });
                        }
                    }
                }
                "error" => {
                    let msg = payload
                        .pointer("/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(format!("anthropic stream error: {msg}"));
                }
                _ => {} // `ping`, `content_block_stop`, `message_stop`, `signature_delta`…
            }
            event_name = None;
            continue;
        }
        if !saw_data {
            raw_fallback.push_str(&line);
            raw_fallback.push('\n');
        }
    }

    if !saw_data {
        // Respuesta sin SSE: objeto `message` completo (p. ej. proxies sin
        // streaming) o error JSON.
        let raw = raw_fallback.trim();
        if !raw.is_empty() {
            if let Ok(v) = serde_json::from_str::<JsonValue>(raw) {
                return Ok(parse_anthropic_message(&v));
            }
        }
        return Err("anthropic: empty or non-SSE response".to_string());
    }

    for block in blocks.into_values() {
        if let BlockState::ToolUse { name, args } = block {
            if !name.trim().is_empty() {
                result.tool_calls.push(QueuedToolCall {
                    name,
                    args: tool_call_args_as_string(&args),
                });
            }
        }
    }
    Ok(result)
}

/// Normaliza [`AnthropicStreamResult`] a `Reply` y registra telemetría por la
/// llamada real (modelo/endpoint despachados, no los del handle).
pub(crate) fn parse_anthropic_result(
    result: AnthropicStreamResult,
    agent: ConceptId,
    request: u64,
    pending_calls: &PendingCallQueue,
    telemetry: Option<&SessionTelemetryHandle>,
    model: &str,
    is_local_endpoint: bool,
) -> Reply {
    if let Some(usage) = result.usage {
        if let Some(tel) = telemetry {
            tel.record_turn_for(model, is_local_endpoint, usage);
        }
    }
    if !result.tool_calls.is_empty() {
        let mut iter = result.tool_calls.into_iter();
        let first = iter.next().expect("non-empty tool_calls");
        push_queued_calls(pending_calls, agent, iter);
        return Reply::call_structured(agent, request, first.name, first.args);
    }
    if result.text.trim().is_empty() {
        let reason = result.stop_reason.unwrap_or_else(|| "unknown".to_string());
        return Reply::fail(
            agent,
            request,
            format!("empty anthropic response (stop_reason={reason})"),
        );
    }
    Reply::done(agent, request, result.text)
}

struct AnthropicHttpError {
    reply: Reply,
    retryable: bool,
    /// Delay mínimo sugerido por el servidor (`Retry-After`, segundos).
    retry_after_ms: Option<u64>,
}

fn parse_retry_after_ms(header: Option<&str>) -> Option<u64> {
    header
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000))
}

fn post_anthropic_json(
    url: &str,
    api_key: &str,
    body: &JsonValue,
    stream: bool,
    agent: ConceptId,
    request: u64,
) -> Result<ureq::Response, AnthropicHttpError> {
    let mut req = ureq::post(url)
        .set("x-api-key", api_key)
        .set("anthropic-version", ANTHROPIC_API_VERSION)
        .set("content-type", "application/json");
    if stream {
        req = req.set("accept", "text/event-stream");
    }
    match req.send_json(body.clone()) {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, r)) => {
            let retry_after_ms = parse_retry_after_ms(r.header("retry-after"));
            let err_body = r.into_string().unwrap_or_default();
            let retryable = matches!(code, 408 | 429 | 500 | 502 | 503 | 504 | 529);
            Err(AnthropicHttpError {
                reply: Reply::fail(agent, request, format!("anthropic HTTP {code}: {err_body}")),
                retryable,
                retry_after_ms,
            })
        }
        Err(ureq::Error::Transport(t)) => Err(AnthropicHttpError {
            reply: Reply::fail(
                agent,
                request,
                format!(
                    "anthropic transport error ({:?}): {}",
                    t.kind(),
                    t.message().unwrap_or("request failed")
                ),
            ),
            retryable: true,
            retry_after_ms: None,
        }),
    }
}

fn retry_delay_ms(attempt: usize, retry_after_ms: Option<u64>) -> Option<Duration> {
    let backoff_ms = RETRY_BACKOFF_MS.get(attempt).copied();
    let ms = backoff_ms
        .zip(retry_after_ms)
        .map(|(b, r)| b.max(r))
        .or_else(|| backoff_ms.or(retry_after_ms));
    ms.map(Duration::from_millis)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_anthropic_http(
    url: &str,
    api_key: &str,
    body: JsonValue,
    agent: ConceptId,
    request: u64,
    shared: &AnthropicSharedState,
    model: &str,
    is_local_endpoint: bool,
) -> Reply {
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        let resp = match post_anthropic_json(url, api_key, &body, false, agent, request) {
            Ok(r) => r,
            Err(err) => {
                if err.retryable {
                    if let Some(delay) = retry_delay_ms(attempt, err.retry_after_ms) {
                        thread::sleep(delay);
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
                    if let Some(delay) = retry_delay_ms(attempt, None) {
                        thread::sleep(delay);
                        continue;
                    }
                }
                return Reply::fail(agent, request, format!("anthropic json error: {msg}"));
            }
        };
        return parse_anthropic_result(
            parse_anthropic_message(&json),
            agent,
            request,
            &shared.pending_calls,
            shared.telemetry.as_ref(),
            model,
            is_local_endpoint,
        );
    }
    Reply::fail(agent, request, "anthropic request exhausted retries")
}

/// Parámetros inmutables de una llamada SSE `POST /v1/messages` (`stream: true`),
/// con el modelo/endpoint reales de la llamada para telemetría.
pub(crate) struct AnthropicStreamTarget<'a> {
    pub(crate) url: &'a str,
    pub(crate) api_key: &'a str,
    pub(crate) agent: ConceptId,
    pub(crate) request: u64,
    pub(crate) shared: &'a AnthropicSharedState,
    pub(crate) preview: Option<&'a StreamPreviewHandle>,
    pub(crate) model: &'a str,
    pub(crate) is_local_endpoint: bool,
}

pub(crate) fn execute_anthropic_stream_http(
    target: AnthropicStreamTarget<'_>,
    body: JsonValue,
) -> Reply {
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        let resp = match post_anthropic_json(
            target.url,
            target.api_key,
            &body,
            true,
            target.agent,
            target.request,
        ) {
            Ok(r) => r,
            Err(err) => {
                if let Some(prev) = target.preview {
                    prev.clear();
                }
                if err.retryable {
                    if let Some(delay) = retry_delay_ms(attempt, err.retry_after_ms) {
                        thread::sleep(delay);
                        continue;
                    }
                }
                return err.reply;
            }
        };
        let reader = BufReader::new(resp.into_reader());
        let result = match accumulate_anthropic_sse_reader(
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
                    if let Some(delay) = retry_delay_ms(attempt, None) {
                        thread::sleep(delay);
                        continue;
                    }
                }
                return Reply::fail(
                    target.agent,
                    target.request,
                    format!("anthropic stream error: {e}"),
                );
            }
        };
        if let Some(prev) = target.preview {
            prev.clear();
        }
        return parse_anthropic_result(
            result,
            target.agent,
            target.request,
            &target.shared.pending_calls,
            target.shared.telemetry.as_ref(),
            target.model,
            target.is_local_endpoint,
        );
    }
    Reply::fail(
        target.agent,
        target.request,
        "anthropic stream exhausted retries",
    )
}

/// Adaptador síncrono para la API nativa Anthropic `/v1/messages`.
pub struct AnthropicResponder {
    api_key: String,
    model: String,
    api_base: String,
    shared: AnthropicSharedState,
}

impl AnthropicResponder {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            api_base: DEFAULT_ANTHROPIC_API_BASE.to_string(),
            shared: AnthropicSharedState::default(),
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

    pub fn with_external_driver_ids(mut self, driver_ids: Vec<String>) -> Self {
        self.shared.driver_ids = driver_ids;
        self
    }
}

impl Responder for AnthropicResponder {
    fn respond(&mut self, req: &AgentView) -> Option<Reply> {
        if let Some(queued_reply) = next_queued_call(&self.shared.pending_calls, req) {
            return Some(queued_reply);
        }
        let is_local = is_loopback_host(api_base_host(&self.api_base));
        let url = format!("{}/v1/messages", self.api_base);
        let body = build_anthropic_request_body(
            req,
            &self.model,
            self.shared.context_config,
            &self.shared.project_instructions,
            false,
            &self.shared.driver_ids,
        );
        Some(execute_anthropic_http(
            &url,
            &self.api_key,
            body,
            req.agent,
            req.request,
            &self.shared,
            &self.model,
            is_local,
        ))
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
    fn anthropic_request_body_maps_tools_memory_and_system_blocks() {
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

        let body = build_anthropic_request_body(
            &req,
            "claude-sonnet-4-5",
            TieredContextConfig::default(),
            "Project notes.",
            true,
            &[],
        );
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["stream"], true);
        // `system` es un array de bloques (soporte futuro de `cache_control`).
        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0]["type"], "text");
        assert!(
            system[0]["text"]
                .as_str()
                .unwrap()
                .contains("Project notes.")
        );

        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"].as_str().unwrap(), "Goal: inspect crate");
        // Tool call: bloque `tool_use` con id de harness e `input` parseado.
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "call_0");
        assert_eq!(msgs[1]["content"][0]["name"], "fs_read");
        assert_eq!(msgs[1]["content"][0]["input"]["path"], "src/lib.rs");
        // Tool observe: bloque `tool_result` con `tool_use_id` coherente.
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_0");
        // El denied sin tool_use emparejado cae a texto `user` y se funde con el
        // `tool_result` previo (misma rol consecutiva: un solo mensaje con bloques).
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "user");
        let last_blocks = last["content"].as_array().unwrap();
        assert_eq!(last_blocks.len(), 2);
        assert_eq!(last_blocks[0]["type"], "tool_result");
        assert_eq!(last_blocks[0]["tool_use_id"], "call_1");
        assert!(last_blocks[1]["text"].as_str().unwrap().contains("denied"));

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["input_schema"]["type"], "object");
        assert!(
            tools[0].get("function").is_none(),
            "Anthropic usa `input_schema`"
        );
    }

    #[test]
    fn anthropic_sse_reader_accumulates_text_thinking_and_tool_use() {
        let preview = StreamPreviewHandle::new();
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":25,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":12}}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"plan first \"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hola \"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Anthropic!\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":18}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n"
        );
        let result = accumulate_anthropic_sse_reader(
            BufReader::new(sse.as_bytes()),
            ConceptId(2),
            5,
            Some(&preview),
        )
        .unwrap();
        let prev_snap = preview.get().unwrap();
        assert_eq!(prev_snap.text, "Hola Anthropic!");
        assert_eq!(prev_snap.thought, "plan first ");

        assert_eq!(result.text, "Hola Anthropic!");
        assert_eq!(result.thought, "plan first ");
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
        let usage = result.usage.unwrap();
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.cached_input_tokens, 12);
        assert_eq!(
            usage.output_tokens, 18,
            "output_tokens llega en `message_delta`"
        );

        let reply = parse_anthropic_result(
            result,
            ConceptId(2),
            5,
            &PendingCallQueue::default(),
            None,
            "claude-sonnet-4-5",
            false,
        );
        assert_eq!(reply, Reply::done(ConceptId(2), 5, "Hola Anthropic!"));
    }

    #[test]
    fn anthropic_sse_reader_queues_multiple_tool_use_and_reports_cache_usage() {
        let pending = PendingCallQueue::default();
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":40,\"cache_read_input_tokens\":8}}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"fs_read\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"src/lib.rs\\\"}\"}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_02\",\"name\":\"git_status\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":30}}\n"
        );
        let result =
            accumulate_anthropic_sse_reader(BufReader::new(sse.as_bytes()), ConceptId(2), 6, None)
                .unwrap();
        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(
            result.tool_calls.len(),
            2,
            "varios `tool_use` en un mismo turno"
        );
        assert_eq!(result.tool_calls[0].name, "fs_read");
        assert!(result.tool_calls[0].args.contains("src/lib.rs"));
        assert_eq!(result.tool_calls[1].name, "git_status");
        assert_eq!(result.tool_calls[1].args, "{}");
        let usage = result.usage.unwrap();
        assert_eq!(usage.input_tokens, 40);
        assert_eq!(
            usage.cached_input_tokens, 8,
            "TEST-04.1: `cache_read_input_tokens`"
        );
        assert_eq!(usage.output_tokens, 30);

        let reply = parse_anthropic_result(
            result,
            ConceptId(2),
            6,
            &pending,
            None,
            "claude-sonnet-4-5",
            false,
        );
        let first = format!("{reply:?}");
        assert!(first.contains("fs_read"), "{first}");
        // El segundo tool call queda en cola para el siguiente tick.
        let next = next_queued_call(&pending, &dummy_agent_view(2, 7)).unwrap();
        let second = format!("{next:?}");
        assert!(second.contains("git_status"), "{second}");
    }

    #[test]
    fn anthropic_sse_reader_tolerates_truncated_stream_and_errors_on_garbage() {
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

        // Texto completo + `message_delta` y corte brutal del proxy: se acepta como parcial.
        let complete = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n"
        );
        let reader_ok = BufReader::new(ChunkErrorReader {
            data: complete.as_bytes().to_vec(),
            pos: 0,
        });
        let result = accumulate_anthropic_sse_reader(reader_ok, ConceptId(2), 1, None).unwrap();
        assert_eq!(result.text, "ok");

        // Corte sin contenido entregado: error reintento.
        let truncated =
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n";
        let reader_err = BufReader::new(ChunkErrorReader {
            data: truncated.as_bytes().to_vec(),
            pos: 0,
        });
        let err = accumulate_anthropic_sse_reader(reader_err, ConceptId(2), 2, None).unwrap_err();
        assert!(is_retryable_stream_error(&err), "{err}");

        // Evento `error` de la API: fallo inmediato.
        let err_event = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n"
        );
        let err = accumulate_anthropic_sse_reader(
            BufReader::new(err_event.as_bytes()),
            ConceptId(2),
            3,
            None,
        )
        .unwrap_err();
        assert!(err.contains("Overloaded"), "{err}");
    }

    #[test]
    fn anthropic_full_message_parses_text_thinking_and_tool_use() {
        let json: JsonValue = serde_json::from_str(
            r#"{"id":"msg_2","role":"assistant","stop_reason":"tool_use",
                "usage":{"input_tokens":10,"cache_read_input_tokens":2,"output_tokens":7},
                "content":[
                  {"type":"thinking","thinking":"reasoning..."},
                  {"type":"text","text":"Let me read it."},
                  {"type":"tool_use","id":"toolu_09","name":"fs_read","input":{"path":"main.rs"}}
                ]}"#,
        )
        .unwrap();
        let result = parse_anthropic_message(&json);
        assert_eq!(result.thought, "reasoning...");
        assert_eq!(result.text, "Let me read it.");
        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
        let usage = result.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cached_input_tokens, 2);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "fs_read");
        assert!(result.tool_calls[0].args.contains("main.rs"));
    }

    #[test]
    fn anthropic_consecutive_user_turns_merge_into_blocks() {
        let mut req = dummy_agent_view(1, 3);
        // `user` + `denied` sin tool_use emparejado: dos mensajes `user` consecutivos.
        req.memory = vec![
            ("user".to_string(), "follow-up question".to_string()),
            ("denied".to_string(), "user declined write".to_string()),
        ];
        let body =
            build_anthropic_request_body(&req, "m", TieredContextConfig::default(), "", false, &[]);
        let msgs = body["messages"].as_array().unwrap();
        // `Goal:` (user) + `follow-up` (user) + `denied` sin emparejar (user) se funden
        // en un solo mensaje con bloques de texto (Anthropic rechaza roles repetidos).
        assert_eq!(msgs.len(), 1, "{body}");
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["text"], "Goal: inspect crate");
        assert_eq!(blocks[1]["text"], "follow-up question");
        assert_eq!(
            blocks[2]["text"],
            "Tool call denied: user declined write. Adjust your plan."
        );
    }
}
