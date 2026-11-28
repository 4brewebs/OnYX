//! Adaptador nativo para la API Anthropic Messages (`/v1/messages`) síncrono y streaming SSE.
//! (`SDD-04` `REQ-04.1` + `REQ-04.2`.)
//!
//! Cabeceras obligatorias: `x-api-key`, `anthropic-version: 2023-06-01` y
//! `content-type: application/json`. El streaming usa eventos `event:` + `data:`
//! (`message_start` / `content_block_delta` / `message_delta`), reutilizando
//! `ContextBudget`, cola secuencial de tool calls y `StreamPreviewHandle` sin
//! mutar el determinismo del grafo. Los reintentos ante `429` / `529`
//! (`overloaded_error`) respetan `Retry-After` (acotado a [`MAX_RETRY_AFTER_MS`]).
//!
//! `REQ-04.2` (Fase 2): prompt caching `cache_control: ephemeral` en el bloque de
//! `system` y la última definición de tool, y extended thinking
//! (`thinking.budget_tokens` vía `--thinking-budget` / `ANTHROPIC_THINKING_BUDGET`,
//! con mapeo de `--effort` si no hay presupuesto explícito). Los bloques de
//! `thinking` (con su `signature`) y `redacted_thinking` se capturan en el stream
//! y se cachean en [`AnthropicSharedState`] para reinyectarlos antes del
//! `tool_use` en el turno de continuación; si la firma no está en caché (p. ej.
//! tras `--resume` o hot-swap de proveedor) se omite el parámetro `thinking`
//! (fallback seguro, sin 400).
//!
//! Nota de ids: la API Anthropic exige que cada `tool_result.tool_use_id` coincida
//! con el `tool_use.id` de la respuesta asistente **previa en la misma historia**.
//! Como el harness asigna ids propios (`call_N`), ambos campos se proyectan desde
//! la memoria de la sesión (coherente por construcción) y no desde el id `toolu_*`
//! emitido en el stream.

use std::collections::{BTreeMap, HashMap};
use std::io::BufReader;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use onyx_soul_agents::{AgentView, ConceptId, Reply, Responder};
use onyx_workspace::{ToolSchema, WorkspaceTools};
use serde_json::{Value as JsonValue, json};

use crate::context::{
    ProjectedTurn, ReasoningEffort, SessionTelemetryHandle, TieredContextConfig, TokenUsage,
    project_memory,
};
use crate::gemini::{
    PendingCallQueue, QueuedToolCall, StreamPreviewHandle, next_queued_call, push_queued_calls,
    system_instruction_text,
};
use crate::llm_profile::{LlmTimeouts, api_base_host, is_loopback_host};
use crate::openai::{
    MAX_TRANSIENT_RETRIES, RETRY_BACKOFF_MS, is_retryable_stream_error, is_transport_read_timeout,
    lowercase_json_schema_types, transport_detail,
};

pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5";
pub const DEFAULT_ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// `max_tokens` por defecto (obligatorio en la API Messages).
pub(crate) const DEFAULT_ANTHROPIC_MAX_TOKENS: u64 = 8192;

/// Tope del `max_tokens` auto-ajustado cuando extended thinking está activo.
const MAX_THINKING_MAX_TOKENS: u64 = 64_000;

/// `thinking.budget_tokens` mínimo explícito (0 = desactivado).
pub(crate) const MIN_THINKING_BUDGET_TOKENS: u64 = 1024;

/// Tope del `thinking.budget_tokens` explícito: por encima, `max_tokens`
/// (acotado a `MAX_THINKING_MAX_TOKENS`) dejaría de ser `> budget_tokens`
/// y la API devolvería 400.
const MAX_THINKING_BUDGET_TOKENS: u64 = 60_000;

/// Tope del `Retry-After` sugerido por el servidor (ms): evita que un proxy
/// malicioso/lento bloquee el bucle de reintentos indefinidamente.
const MAX_RETRY_AFTER_MS: u64 = 60_000;

/// Bloques de `thinking`/`redacted_thinking` (con `signature`) capturados por
/// `(agente, call_index)`, para reinyectarlos en el turno de continuación del
/// tool loop (`REQ-04.2`). Mismo patrón que `GeminiSharedState::thought_signatures`.
pub(crate) type ThinkingBlockMap = Arc<Mutex<HashMap<(u64, usize), Vec<JsonValue>>>>;

/// Estado compartido por los hilos de despacho Anthropic (cola secuencial de tool
/// calls, bloques de thinking caché, telemetría). `pub(crate)` para que
/// `DynamicLlmDriver` lo construya con la cola de tool calls compartida con Gemini
/// y OpenAI.
#[derive(Clone, Default)]
pub(crate) struct AnthropicSharedState {
    pub(crate) pending_calls: PendingCallQueue,
    /// Bloques de thinking (`signature` incluido) por `(agente, call_index)`.
    pub(crate) thinking_blocks: ThinkingBlockMap,
    /// Presupuesto explícito de `--thinking-budget` / `ANTHROPIC_THINKING_BUDGET`
    /// (`Some(0)` = desactivado explícitamente; `None` = se deriva de `--effort`).
    pub(crate) thinking_budget: Option<u64>,
    /// `--effort` activo: se mapea a `thinking.budget_tokens` si no hay
    /// presupuesto explícito (`REQ-04.2`).
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) project_instructions: Arc<String>,
    pub(crate) telemetry: Option<SessionTelemetryHandle>,
    pub(crate) context_config: TieredContextConfig,
    /// ids de drivers de harness CLI externos habilitados (`SDD-10` Fase 1): enriquecen el
    /// esquema dinámico de `spawn_subagent` (`driver` opcional).
    pub(crate) driver_ids: Vec<String>,
    /// Schemas MCP `mcp__*` del registro de arranque (`SDD-04 REQ-04.3`): se
    /// declaran en el body filtradas por los tools del spec del agente.
    pub(crate) mcp_schemas: Arc<Vec<ToolSchema>>,
    /// `FIX-02`: timeouts HTTP de las llamadas al proveedor (connect/read).
    pub(crate) timeouts: LlmTimeouts,
}

impl AnthropicSharedState {
    pub(crate) fn clear_thinking_blocks(&self) {
        if let Ok(mut guard) = self.thinking_blocks.lock() {
            guard.clear();
        }
    }
}

/// Bloque de contenido asistente en curso de un stream `content_block_*`.
enum BlockState {
    Text,
    Thinking {
        text: String,
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        name: String,
        args: String,
    },
}

/// Resultado normalizado de una respuesta Anthropic (SSE acumulado u objeto
/// `message` completo), agnóstico de transporte.
#[derive(Debug)]
pub(crate) struct AnthropicStreamResult {
    pub(crate) text: String,
    pub(crate) thought: String,
    pub(crate) tool_calls: Vec<QueuedToolCall>,
    /// Bloques `thinking` (con `signature`) / `redacted_thinking` en orden de
    /// índice, listos para reinyectar en el turno de continuación.
    pub(crate) thinking_blocks: Vec<JsonValue>,
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
///
/// La API Anthropic reporta `input_tokens` **excluyendo** los tokens de caché
/// (`cache_creation_input_tokens` / `cache_read_input_tokens`): se normaliza al
/// total para que `estimate_usage_cost_usd` (que resta `cached_input_tokens`) y
/// el `ctx %` no doble-cuenten ni colapsen.
fn extract_anthropic_start_usage(usage: &JsonValue) -> Option<TokenUsage> {
    let raw_input = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(TokenUsage {
        input_tokens: raw_input
            .saturating_add(cache_creation)
            .saturating_add(cache_read),
        cached_input_tokens: cache_read,
        output_tokens: usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        reasoning_tokens: 0,
    })
}

/// Valida un presupuesto de extended thinking: `0` (desactivado) o >= 1024 tokens.
pub(crate) fn parse_thinking_budget(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    let value: u64 = trimmed.parse().map_err(|_| {
        format!("--thinking-budget must be a non-negative integer number of tokens, got `{raw}`")
    })?;
    if value != 0 && value < MIN_THINKING_BUDGET_TOKENS {
        return Err(format!(
            "--thinking-budget must be 0 (disabled) or at least {MIN_THINKING_BUDGET_TOKENS} tokens, got {value}"
        ));
    }
    if value > MAX_THINKING_BUDGET_TOKENS {
        return Err(format!(
            "--thinking-budget cannot exceed {MAX_THINKING_BUDGET_TOKENS} tokens (the API requires `max_tokens` above the budget), got {value}"
        ));
    }
    Ok(value)
}

/// Presupuesto efectivo de thinking para una llamada Anthropic (`REQ-04.2`):
/// el valor explícito (`--thinking-budget` / `ANTHROPIC_THINKING_BUDGET`, donde
/// `0` = desactivado explícito) tiene prioridad; sin él, `--effort` mapea a
/// low → 2048, medium → 8192, high → 16384.
pub(crate) fn effective_thinking_budget(
    explicit: Option<u64>,
    effort: Option<ReasoningEffort>,
) -> Option<u64> {
    if let Some(value) = explicit {
        return Some(value);
    }
    match effort {
        Some(ReasoningEffort::Low) => Some(2048),
        Some(ReasoningEffort::Medium) => Some(8192),
        Some(ReasoningEffort::High) => Some(16384),
        _ => None,
    }
}

/// URL `{base}/v1/messages` normalizada: una `api_base` que ya termina en
/// `/v1` no genera `/v1/v1/messages`.
pub(crate) fn anthropic_messages_url(api_base: &str) -> String {
    let base = api_base.trim().trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
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

/// `true` si el segmento de historia **tras el último mensaje de usuario**
/// contiene un `tool_use` sin bloques de thinking en caché (con
/// `signature`) — p. ej. tras `--resume` o un hot-swap de proveedor. En ese
/// caso se omite el parámetro `thinking` de esta petición (fallback seguro)
/// para que la API no devuelva 400 por bloques de thinking ausentes.
///
/// Un nuevo mensaje de usuario tras el último tool loop reactiva el thinking
/// (el segmento antiguo queda «respondido» y no se inspecciona): por eso solo
/// se escanea desde el último `ProjectedTurn::User` en adelante.
fn history_missing_thinking(
    turns: &[ProjectedTurn],
    agent: u64,
    cache: &HashMap<(u64, usize), Vec<JsonValue>>,
) -> bool {
    let last_user = turns
        .iter()
        .rposition(|t| matches!(t, ProjectedTurn::User(_)))
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut prev_was_tool_call = false;
    for turn in &turns[last_user..] {
        let is_tool_call = matches!(turn, ProjectedTurn::ToolCall { .. });
        if is_tool_call && !prev_was_tool_call {
            let call_index = match turn {
                ProjectedTurn::ToolCall { call_index, .. } => *call_index,
                _ => unreachable!("matched as ToolCall"),
            };
            if !cache.contains_key(&(agent, call_index)) {
                return true;
            }
        }
        prev_was_tool_call = is_tool_call;
    }
    false
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_anthropic_request_body(
    req: &AgentView,
    model: &str,
    budget: TieredContextConfig,
    project_instructions: &str,
    stream: bool,
    driver_ids: &[String],
    thinking_blocks: &HashMap<(u64, usize), Vec<JsonValue>>,
    thinking_budget_tokens: Option<u64>,
    mcp_schemas: &[ToolSchema],
) -> JsonValue {
    let projected = project_memory(&req.goal, &req.memory, &budget);
    let mut messages: Vec<JsonValue> = Vec::with_capacity(projected.turns.len() + 1);

    // `REQ-04.2`: extended thinking. El presupuesto explícito (`0` = off) manda;
    // si no hay, `effective_thinking_budget` ya mapeó `--effort`. Se omite el
    // parámetro en un tool loop con firmas ausentes (fallback seguro, sin 400).
    // Se calcula **antes** del loop de turns: `projected.turns` se mueve allí.
    let thinking_budget = thinking_budget_tokens.filter(|b| *b > 0);
    let enable_thinking = thinking_budget
        .is_some_and(|_| !history_missing_thinking(&projected.turns, req.agent.0, thinking_blocks));
    // La API exige `max_tokens > budget_tokens` SOLO cuando `thinking` se envía:
    // se auto-ajusta hacia arriba únicamente en ese caso (sin thinking, el
    // `max_tokens` por defecto vale y evita sobre-cobro de ventana).
    let max_tokens = if enable_thinking {
        thinking_budget
            .map(|b| {
                DEFAULT_ANTHROPIC_MAX_TOKENS
                    .max(b.saturating_add(4096))
                    .min(MAX_THINKING_MAX_TOKENS)
            })
            .unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS)
    } else {
        DEFAULT_ANTHROPIC_MAX_TOKENS
    };

    for turn in projected.turns {
        match turn {
            ProjectedTurn::User(text) => push_user_message(&mut messages, JsonValue::String(text)),
            ProjectedTurn::AssistantText(text) => {
                messages.push(json!({"role": "assistant", "content": text}));
            }
            ProjectedTurn::ToolCall {
                call_index,
                call_id,
                name,
                args_json,
                ..
            } => {
                // La API exige `tool_use.input` como objeto JSON: cualquier
                // args no-objecto cae a `{}`.
                let input = serde_json::from_str::<JsonValue>(&args_json)
                    .ok()
                    .filter(|v| v.is_object())
                    .unwrap_or_else(|| json!({}));
                // `REQ-04.2`: se reinyectan los bloques de thinking (con
                // `signature`) cacheados de la respuesta original al inicio de
                // la secuencia de tool calls. Fallback seguro: sin caché → sin
                // bloques (solo se cachea el primer call de cada respuesta, así
                // la fusión de roles los coloca una sola vez).
                let mut blocks = thinking_blocks
                    .get(&(req.agent.0, call_index))
                    .cloned()
                    .unwrap_or_default();
                blocks.push(json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": input
                }));
                messages.push(json!({
                    "role": "assistant",
                    "content": blocks
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

    let mut tools: Vec<JsonValue> =
        WorkspaceTools::schemas_with_subagents_and_drivers(true, driver_ids)
            .into_iter()
            .filter(|s| req.tools.iter().any(|t| t.as_str() == s.name.as_ref()))
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
    // `SDD-04 REQ-04.3`: tools MCP `mcp__*` (schemas del registro, filtradas por
    // los tools del spec; `type` normalizadas a minúsculas, que exige la API).
    for s in mcp_schemas
        .iter()
        .filter(|s| req.tools.iter().any(|t| t.as_str() == s.name.as_ref()))
    {
        let mut params = s.parameters.clone();
        lowercase_json_schema_types(&mut params);
        tools.push(json!({
            "name": s.name,
            "description": s.description,
            "input_schema": params
        }));
    }
    // `REQ-04.2`: prompt caching `ephemeral` en la última definición de tool
    // (el prefijo anterior queda cubierto por la ventana de 20 bloques).
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }

    // `REQ-04.2`: prompt caching `ephemeral` en el bloque de `system`.
    let system = json!([{
        "type": "text",
        "text": system_instruction_text(project_instructions),
        "cache_control": {"type": "ephemeral"}
    }]);

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": messages,
        "stream": stream
    });
    if enable_thinking {
        if let Some(budget_tokens) = thinking_budget {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget_tokens});
        }
    }
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
        thinking_blocks: Vec::new(),
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
                    let text = block
                        .get("thinking")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    result.thought.push_str(&text);
                    // Solo se puede reenviar un bloque de thinking si trae su
                    // `signature` cifrada (sin ella la API rechazaría la historia).
                    if !text.is_empty() {
                        if let Some(sig) = block.get("signature").and_then(|v| v.as_str()) {
                            result.thinking_blocks.push(json!({
                                "type": "thinking",
                                "thinking": text,
                                "signature": sig
                            }));
                        }
                    }
                }
                Some("redacted_thinking") => {
                    if let Some(data) = block.get("data").and_then(|v| v.as_str()) {
                        if !data.is_empty() {
                            result.thinking_blocks.push(json!({
                                "type": "redacted_thinking",
                                "data": data
                            }));
                        }
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
        thinking_blocks: Vec::new(),
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
                // Un stream que se corta tras un `message_delta` completo
                // (`stop_reason`) **y** contenido entregado se acepta como
                // parcial; sin `stop_reason` se devuelve `Err` para que la capa
                // HTTP reintente (no hay garantía de entrega completa).
                let delivered = !result.text.is_empty()
                    || !result.thought.is_empty()
                    || !result.tool_calls.is_empty();
                if result.stop_reason.is_some() && delivered {
                    break;
                }
                return Err(err.to_string());
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
                            blocks.insert(
                                index,
                                BlockState::Thinking {
                                    text: String::new(),
                                    signature: None,
                                },
                            );
                        }
                        Some("redacted_thinking") => {
                            let data = block
                                .get("data")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            blocks.insert(index, BlockState::RedactedThinking { data });
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
                            match blocks.get_mut(&index) {
                                Some(BlockState::Thinking { text, .. }) => text.push_str(t),
                                _ => {
                                    blocks.insert(
                                        index,
                                        BlockState::Thinking {
                                            text: t.to_string(),
                                            signature: None,
                                        },
                                    );
                                }
                            }
                            if let Some(prev) = preview {
                                prev.set_thought(agent.0, request, &result.thought);
                            }
                        }
                        Some("signature_delta") => {
                            // Firma cifrada del bloque de thinking (formato
                            // oficial: `{"type":"signature_delta","signature":…}`):
                            // sin ella el bloque no puede reenviarse en el tool
                            // loop. Puede llegar en varios chunks: se acumula.
                            let sig = delta
                                .get("signature")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            if !sig.is_empty() {
                                if let Some(BlockState::Thinking { signature, .. }) =
                                    blocks.get_mut(&index)
                                {
                                    signature.get_or_insert_with(String::new).push_str(sig);
                                }
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
                    let etype = payload
                        .pointer("/error/type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown_error");
                    let msg = payload
                        .pointer("/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    // El `type` va en el mensaje: `overloaded_error` (529) es
                    // reintenable para la capa HTTP vía `is_retryable_stream_error`.
                    return Err(format!("anthropic stream error ({etype}): {msg}"));
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

    // `BTreeMap`: los bloques se recogen en orden de índice (thinking antes
    // que `tool_use`, igual que la API los emitió).
    for block in blocks.into_values() {
        match block {
            BlockState::ToolUse { name, args } => {
                if !name.trim().is_empty() {
                    result.tool_calls.push(QueuedToolCall {
                        name,
                        args: tool_call_args_as_string(&args),
                    });
                }
            }
            BlockState::Thinking { text, signature } => {
                // Sin `signature` el bloque no puede reenviarse: se descarta
                // (aparece en `thought` pero no en `thinking_blocks`).
                if !text.is_empty() {
                    if let Some(sig) = signature {
                        result.thinking_blocks.push(json!({
                            "type": "thinking",
                            "thinking": text,
                            "signature": sig
                        }));
                    }
                }
            }
            BlockState::RedactedThinking { data } => {
                if !data.is_empty() {
                    result.thinking_blocks.push(json!({
                        "type": "redacted_thinking",
                        "data": data
                    }));
                }
            }
            BlockState::Text => {}
        }
    }
    Ok(result)
}

/// Normaliza [`AnthropicStreamResult`] a `Reply`, registra telemetría por la
/// llamada real (modelo/endpoint despachados, no los del handle) y, si la
/// respuesta trae bloques de thinking (con `signature`), los cachea bajo el
/// `call_index` del **primer** tool call de la respuesta para reinyectarlos en
/// el turno de continuación (`REQ-04.2`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn parse_anthropic_result(
    result: AnthropicStreamResult,
    agent: ConceptId,
    request: u64,
    shared: &AnthropicSharedState,
    call_index: usize,
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
        if !result.thinking_blocks.is_empty() {
            if let Ok(mut cache) = shared.thinking_blocks.lock() {
                cache.insert((agent.0, call_index), result.thinking_blocks.clone());
            }
        }
        let mut iter = result.tool_calls.into_iter();
        let first = iter.next().expect("non-empty tool_calls");
        push_queued_calls(&shared.pending_calls, agent, iter);
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
    timeouts: LlmTimeouts,
) -> Result<ureq::Response, AnthropicHttpError> {
    // `FIX-02`: timeouts explícitos por petición (connect corto + read por
    // chunk SSE). Sin `timeout_read`, un proveedor mudo bloquea el read
    // eternamente (ureq por defecto: "requests may block forever on reads").
    let http = ureq::AgentBuilder::new()
        .timeout_connect(timeouts.connect)
        .timeout_read(timeouts.read)
        .build();
    let mut req = http
        .post(url)
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
                    transport_detail(&t)
                ),
            ),
            // `FIX-02` (cierre de auditoría): el timeout de read NO es
            // reintenable (`is_transport_read_timeout`); connect/DNS/reset
            // siguen siéndolo.
            retryable: !is_transport_read_timeout(&t),
            retry_after_ms: None,
        }),
    }
}

fn retry_delay_ms(attempt: usize, retry_after_ms: Option<u64>) -> Option<Duration> {
    // Sin backoff definido para el intento (el último): se devuelve `None`
    // para no dormir y que el caller devuelva directamente el `Reply` de error.
    let backoff_ms = RETRY_BACKOFF_MS.get(attempt).copied()?;
    // El `Retry-After` sugerido por el servidor se acota: un proxy lento no
    // puede bloquear el bucle de reintentos indefinidamente.
    let retry_after_ms = retry_after_ms.map(|ms| ms.min(MAX_RETRY_AFTER_MS));
    let ms = match retry_after_ms {
        Some(r) => backoff_ms.max(r),
        None => backoff_ms,
    };
    Some(Duration::from_millis(ms))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_anthropic_http(
    url: &str,
    api_key: &str,
    body: JsonValue,
    agent: ConceptId,
    request: u64,
    call_index: usize,
    shared: &AnthropicSharedState,
    model: &str,
    is_local_endpoint: bool,
) -> Reply {
    let timeouts = shared.timeouts;
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        let resp = match post_anthropic_json(url, api_key, &body, false, agent, request, timeouts) {
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
            shared,
            call_index,
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
    pub(crate) call_index: usize,
    pub(crate) shared: &'a AnthropicSharedState,
    pub(crate) preview: Option<&'a StreamPreviewHandle>,
    pub(crate) model: &'a str,
    pub(crate) is_local_endpoint: bool,
}

pub(crate) fn execute_anthropic_stream_http(
    target: AnthropicStreamTarget<'_>,
    body: JsonValue,
) -> Reply {
    let timeouts = target.shared.timeouts;
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        let resp = match post_anthropic_json(
            target.url,
            target.api_key,
            &body,
            true,
            target.agent,
            target.request,
            timeouts,
        ) {
            Ok(r) => r,
            Err(err) => {
                if let Some(prev) = target.preview {
                    prev.clear_for(target.agent.0, target.request);
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
                    prev.clear_for(target.agent.0, target.request);
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
            prev.clear_for(target.agent.0, target.request);
        }
        return parse_anthropic_result(
            result,
            target.agent,
            target.request,
            target.shared,
            target.call_index,
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

    pub fn with_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.shared.effort = effort;
        self
    }

    pub fn with_thinking_budget(mut self, thinking_budget: Option<u64>) -> Self {
        self.shared.thinking_budget = thinking_budget;
        self
    }

    /// Schemas MCP `mcp__*` a declarar en el body (`SDD-04 REQ-04.3`).
    pub fn with_mcp_schemas(mut self, mcp_schemas: Arc<Vec<ToolSchema>>) -> Self {
        self.shared.mcp_schemas = mcp_schemas;
        self
    }

    /// `FIX-02`: timeouts HTTP de las llamadas al proveedor (connect/read).
    pub fn with_llm_timeouts(mut self, timeouts: LlmTimeouts) -> Self {
        self.shared.timeouts = timeouts;
        self
    }
}

impl Responder for AnthropicResponder {
    fn respond(&mut self, req: &AgentView) -> Option<Reply> {
        if let Some(queued_reply) = next_queued_call(&self.shared.pending_calls, req) {
            return Some(queued_reply);
        }
        let is_local = is_loopback_host(api_base_host(&self.api_base));
        let url = anthropic_messages_url(&self.api_base);
        let body = {
            let thinking_blocks = self
                .shared
                .thinking_blocks
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            let thinking_budget =
                effective_thinking_budget(self.shared.thinking_budget, self.shared.effort);
            build_anthropic_request_body(
                req,
                &self.model,
                self.shared.context_config,
                &self.shared.project_instructions,
                false,
                &self.shared.driver_ids,
                &thinking_blocks,
                thinking_budget,
                &self.shared.mcp_schemas,
            )
        };
        let call_index = req.memory.iter().filter(|(k, _)| k == "call").count();
        Some(execute_anthropic_http(
            &url,
            &self.api_key,
            body,
            req.agent,
            req.request,
            call_index,
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
            &HashMap::new(),
            None,
            &[],
        );
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], DEFAULT_ANTHROPIC_MAX_TOKENS);
        assert_eq!(body["stream"], true);
        // `system` es un array de bloques con prompt caching `ephemeral`
        // (`REQ-04.2`) en el último bloque.
        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0]["type"], "text");
        assert!(
            system[0]["text"]
                .as_str()
                .unwrap()
                .contains("Project notes.")
        );
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

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
        // `REQ-04.2`: `cache_control` solo en la última definición de tool.
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
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
        // `REQ-04.2`: `input_tokens` total (25 + 0 creación + 12 lectura).
        assert_eq!(usage.input_tokens, 37);
        assert_eq!(usage.cached_input_tokens, 12);
        assert_eq!(
            usage.output_tokens, 18,
            "output_tokens llega en `message_delta`"
        );

        let shared = AnthropicSharedState::default();
        let reply = parse_anthropic_result(
            result,
            ConceptId(2),
            5,
            &shared,
            0,
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
        // `REQ-04.2`: `input_tokens` total (40 + 8 lectura de caché).
        assert_eq!(usage.input_tokens, 48);
        assert_eq!(
            usage.cached_input_tokens, 8,
            "TEST-04.1: `cache_read_input_tokens`"
        );
        assert_eq!(usage.output_tokens, 30);

        let shared = AnthropicSharedState {
            pending_calls: Arc::clone(&pending),
            ..AnthropicSharedState::default()
        };
        let reply = parse_anthropic_result(
            result,
            ConceptId(2),
            6,
            &shared,
            0,
            None,
            "claude-sonnet-4-5",
            false,
        );
        let first = format!("{reply:?}");
        assert!(first.contains("fs_read"), "{first}");
        // El segundo tool call queda en cola para el siguiente tick.
        let mut follow_req = dummy_agent_view(2, 7);
        follow_req.memory = vec![
            ("call".to_string(), "fs_read {}".to_string()),
            ("observe".to_string(), "ok".to_string()),
        ];
        let next = next_queued_call(&pending, &follow_req).unwrap();
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

        // Texto entregado pero **sin** `message_delta` (`stop_reason` ausente):
        // no hay garantía de entrega completa → `Err` reintenable.
        let text_no_stop = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n"
        );
        let reader_no_stop = BufReader::new(ChunkErrorReader {
            data: text_no_stop.as_bytes().to_vec(),
            pos: 0,
        });
        let err =
            accumulate_anthropic_sse_reader(reader_no_stop, ConceptId(2), 4, None).unwrap_err();
        assert!(is_retryable_stream_error(&err), "{err}");

        // Evento `error` de la API con `overloaded_error`: reintenable (529).
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
        assert!(err.contains("overloaded_error"), "{err}");
        assert!(is_retryable_stream_error(&err), "{err}");
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
        // `REQ-04.2`: `input_tokens` total (10 + 2 lectura de caché).
        assert_eq!(usage.input_tokens, 12);
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
        let body = build_anthropic_request_body(
            &req,
            "m",
            TieredContextConfig::default(),
            "",
            false,
            &[],
            &HashMap::new(),
            None,
            &[],
        );
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

    #[test]
    fn anthropic_thinking_budget_parse_and_effort_mapping() {
        // `REQ-04.2`: validación 0 (off) o >= 1024.
        assert_eq!(parse_thinking_budget("0").unwrap(), 0);
        assert_eq!(parse_thinking_budget("1024").unwrap(), 1024);
        assert_eq!(parse_thinking_budget(" 2048 ").unwrap(), 2048);
        assert!(parse_thinking_budget("512").is_err());
        assert!(parse_thinking_budget("abc").is_err());
        assert!(parse_thinking_budget("-1").is_err());
        // Tope: por encima, `max_tokens` no podría superar `budget_tokens`.
        assert_eq!(parse_thinking_budget("60000").unwrap(), 60_000);
        assert!(parse_thinking_budget("60001").is_err());

        // El explícito manda (incluso `0` = off contra `--effort`).
        assert_eq!(
            effective_thinking_budget(Some(0), Some(ReasoningEffort::High)),
            Some(0)
        );
        assert_eq!(
            effective_thinking_budget(Some(4096), Some(ReasoningEffort::Low)),
            Some(4096)
        );
        // Sin explícito: `--effort` mapea low → 2048, medium → 8192, high → 16384.
        assert_eq!(
            effective_thinking_budget(None, Some(ReasoningEffort::Low)),
            Some(2048)
        );
        assert_eq!(
            effective_thinking_budget(None, Some(ReasoningEffort::Medium)),
            Some(8192)
        );
        assert_eq!(
            effective_thinking_budget(None, Some(ReasoningEffort::High)),
            Some(16384)
        );
        assert_eq!(effective_thinking_budget(None, None), None);
    }

    #[test]
    fn anthropic_request_body_thinking_budget_adjusts_max_tokens() {
        let req = dummy_agent_view(1, 3);
        // high → 16384: `max_tokens` se auto-ajusta a > `budget_tokens`.
        let budget = effective_thinking_budget(None, Some(ReasoningEffort::High)).unwrap();
        let body = build_anthropic_request_body(
            &req,
            "claude-sonnet-4-5",
            TieredContextConfig::default(),
            "",
            false,
            &[],
            &HashMap::new(),
            Some(budget),
            &[],
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 16384);
        assert!(
            body["max_tokens"].as_u64().unwrap() > 16384,
            "`max_tokens` debe superar `budget_tokens`"
        );
        assert_eq!(body["max_tokens"], 20480);

        // `--thinking-budget 0` = off explícito: sin `thinking` ni ajuste.
        let body = build_anthropic_request_body(
            &req,
            "claude-sonnet-4-5",
            TieredContextConfig::default(),
            "",
            false,
            &[],
            &HashMap::new(),
            Some(0),
            &[],
        );
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], DEFAULT_ANTHROPIC_MAX_TOKENS);

        // Sin presupuesto ni esfuerzo: sin `thinking`.
        let body = build_anthropic_request_body(
            &req,
            "claude-sonnet-4-5",
            TieredContextConfig::default(),
            "",
            false,
            &[],
            &HashMap::new(),
            None,
            &[],
        );
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], DEFAULT_ANTHROPIC_MAX_TOKENS);
    }

    #[test]
    fn history_missing_thinking_only_scans_segment_after_last_user_turn() {
        let tool_call = |call_index: usize| ProjectedTurn::ToolCall {
            call_index,
            call_id: format!("call_{call_index}"),
            name: "fs_read".to_string(),
            args_json: "{}".to_string(),
        };
        let empty = HashMap::new();

        // Tool loop sin firmas en caché → `true` (fallback: sin `thinking`).
        let turns = vec![
            ProjectedTurn::User("goal".to_string()),
            tool_call(0),
            ProjectedTurn::ToolObserve {
                call_id: Some("call_0".to_string()),
                name: "fs_read".to_string(),
                content: "ok".to_string(),
                tier: crate::context::ToolOutputTier::Tier0Full,
                is_toon: false,
                is_superseded: false,
            },
        ];
        assert!(history_missing_thinking(&turns, 1, &empty));

        // Mismo tool loop pero con un **nuevo mensaje de usuario** al final
        // (p. ej. tras `--resume`): el segmento antiguo no se inspecciona →
        // el thinking vuelve a activarse.
        let turns = vec![
            ProjectedTurn::User("goal".to_string()),
            tool_call(0),
            ProjectedTurn::ToolObserve {
                call_id: Some("call_0".to_string()),
                name: "fs_read".to_string(),
                content: "ok".to_string(),
                tier: crate::context::ToolOutputTier::Tier0Full,
                is_toon: false,
                is_superseded: false,
            },
            ProjectedTurn::User("otra pregunta".to_string()),
        ];
        assert!(!history_missing_thinking(&turns, 1, &empty));

        // Firma en caché para el primer `call` de la corrida → `false`.
        let mut cache = HashMap::new();
        cache.insert(
            (1u64, 0usize),
            vec![json!({"type": "thinking", "signature": "s"})],
        );
        let turns = vec![ProjectedTurn::User("goal".to_string()), tool_call(0)];
        assert!(!history_missing_thinking(&turns, 1, &cache));
    }

    #[test]
    fn anthropic_sse_reader_captures_thinking_signature_and_reinjected_in_tool_loop() {
        // `REQ-04.2`: thinking (con `signature_delta`) + `redacted_thinking` +
        // `tool_use` → bloques cacheados para el turno de continuación.
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"think \"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_a\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"bc\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"red_1\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"fs_read\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n"
        );
        let result =
            accumulate_anthropic_sse_reader(BufReader::new(sse.as_bytes()), ConceptId(7), 1, None)
                .unwrap();
        assert_eq!(result.thought, "think ");
        assert_eq!(result.thinking_blocks.len(), 2);
        assert_eq!(result.thinking_blocks[0]["type"], "thinking");
        assert_eq!(result.thinking_blocks[0]["thinking"], "think ");
        assert_eq!(result.thinking_blocks[0]["signature"], "sig_abc");
        assert_eq!(result.thinking_blocks[1]["type"], "redacted_thinking");
        assert_eq!(result.thinking_blocks[1]["data"], "red_1");
        assert_eq!(result.tool_calls.len(), 1);

        let shared = AnthropicSharedState::default();
        let reply = parse_anthropic_result(
            result,
            ConceptId(7),
            1,
            &shared,
            0,
            None,
            "claude-sonnet-4-5",
            false,
        );
        let _ = reply; // `call_structured` (primer tool call)
        let cache = shared.thinking_blocks.lock().unwrap();
        let cached = cache
            .get(&(7u64, 0usize))
            .expect("caché bajo (agente, call_index 0)");
        assert_eq!(cached.len(), 2);
        drop(cache);

        // Turno de continuación: la historia tiene el `tool_use` `call_0` y su
        // resultado; los bloques cacheados se reinyectan **antes** del tool_use
        // y `thinking` se habilita (firma presente en caché).
        let mut cont = dummy_agent_view(7, 2);
        cont.memory = vec![
            ("call".to_string(), r#"fs_read {"path":"a.rs"}"#.to_string()),
            ("observe".to_string(), "1: fn main() {}".to_string()),
        ];
        let cache = shared.thinking_blocks.lock().unwrap().clone();
        let body = build_anthropic_request_body(
            &cont,
            "claude-sonnet-4-5",
            TieredContextConfig::default(),
            "",
            false,
            &[],
            &cache,
            Some(2048),
            &[],
        );
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
        let msgs = body["messages"].as_array().unwrap();
        let assistant = &msgs[1];
        assert_eq!(assistant["role"], "assistant");
        let content = assistant["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "sig_abc");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[2]["type"], "tool_use");
        assert_eq!(content[2]["id"], "call_0");

        // Fallback seguro: sin firmas en caché (p. ej. `--resume`) se omite el
        // parámetro `thinking` — sin bloques inyectados, solo el `tool_use`.
        let body = build_anthropic_request_body(
            &cont,
            "claude-sonnet-4-5",
            TieredContextConfig::default(),
            "",
            false,
            &[],
            &HashMap::new(),
            Some(2048),
            &[],
        );
        assert!(
            body.get("thinking").is_none(),
            "fallback: sin firmas → sin `thinking`"
        );
        let msgs = body["messages"].as_array().unwrap();
        let content = msgs[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
    }

    #[test]
    fn anthropic_start_usage_normalizes_cache_tokens() {
        // La API reporta `input_tokens` sin los tokens de caché: se suma el
        // total y `cached_input_tokens` refleja solo los leídos (coste reducido).
        let usage: JsonValue = json!({
            "input_tokens": 10,
            "cache_creation_input_tokens": 5,
            "cache_read_input_tokens": 3,
            "output_tokens": 4
        });
        let u = extract_anthropic_start_usage(&usage).unwrap();
        assert_eq!(u.input_tokens, 18);
        assert_eq!(u.cached_input_tokens, 3);
        assert_eq!(u.output_tokens, 4);
    }

    #[test]
    fn anthropic_retry_delay_caps_retry_after_and_stops_on_last_attempt() {
        // Backoff base; `Retry-After` mayor manda (acotado a 60s).
        assert_eq!(retry_delay_ms(0, None), Some(Duration::from_millis(400)));
        assert_eq!(
            retry_delay_ms(0, Some(90_000)),
            Some(Duration::from_millis(MAX_RETRY_AFTER_MS))
        );
        assert_eq!(
            retry_delay_ms(1, Some(2_000)),
            Some(Duration::from_millis(2_000))
        );
        // Último intento: sin backoff → `None` (no se duerme; se devuelve el error).
        assert_eq!(retry_delay_ms(2, Some(90_000)), None);
        assert_eq!(retry_delay_ms(2, None), None);
    }

    #[test]
    fn anthropic_messages_url_avoids_double_v1() {
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://localhost:8080/v1"),
            "https://localhost:8080/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://localhost:8080/v1/"),
            "https://localhost:8080/v1/messages"
        );
    }
}
