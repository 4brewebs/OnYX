use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use onyx_soul_agents::{AgentView, ConceptId, Reply, Responder};

#[cfg(test)]
use onyx_soul_agents::{
    EventBus, GraphView, PlatformDriver, SurfaceBinding, Vocab, pending_requests,
};
use onyx_workspace::tools::{discover_skills_summary, scrub_secrets};
use onyx_workspace::{ToolSchema, WorkspaceTools};
use serde_json::{Value as JsonValue, json};
#[cfg(test)]
use std::collections::HashSet;
#[cfg(test)]
use std::sync::mpsc;
#[cfg(test)]
use std::thread;

use crate::context::{
    ContextBudget, ProjectedTurn, ReasoningEffort, SessionTelemetryHandle, TieredContextConfig,
    extract_gemini_usage, project_memory,
};
use crate::llm_profile::{LlmTimeouts, api_base_host, is_loopback_host};
use crate::openai::{
    MAX_TRANSIENT_RETRIES, RETRY_BACKOFF_MS, is_retryable_stream_error, is_transport_read_timeout,
    transport_detail,
};

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-4-hydrogen";
pub const DEFAULT_GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const MAX_PROJECT_INSTRUCTIONS_BYTES: usize = 16 * 1024;

pub(crate) type SignatureMap = Arc<Mutex<HashMap<(u64, usize), JsonValue>>>;
pub(crate) type PendingCallQueue = Arc<Mutex<HashMap<u64, VecDeque<(String, String)>>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamPreview {
    pub agent: u64,
    pub request: u64,
    pub text: String,
    pub thought: String,
}

#[derive(Clone, Default)]
pub struct StreamPreviewHandle {
    inner: Arc<Mutex<Option<StreamPreview>>>,
    dirty: Arc<AtomicBool>,
}

impl StreamPreviewHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, agent: u64, request: u64, text: impl Into<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            let prev_thought = guard
                .as_ref()
                .filter(|p| p.agent == agent && p.request == request)
                .map(|p| p.thought.clone())
                .unwrap_or_default();
            *guard = Some(StreamPreview {
                agent,
                request,
                text: text.into(),
                thought: prev_thought,
            });
            self.dirty.store(true, Ordering::Release);
        }
    }

    pub fn set_thought(&self, agent: u64, request: u64, thought: impl Into<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            let prev_text = guard
                .as_ref()
                .filter(|p| p.agent == agent && p.request == request)
                .map(|p| p.text.clone())
                .unwrap_or_default();
            *guard = Some(StreamPreview {
                agent,
                request,
                text: prev_text,
                thought: thought.into(),
            });
            self.dirty.store(true, Ordering::Release);
        }
    }

    #[cfg(test)]
    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            if guard.take().is_some() {
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    pub fn clear_for(&self, agent: u64, request: u64) {
        if let Ok(mut guard) = self.inner.lock() {
            if guard
                .as_ref()
                .is_some_and(|p| p.agent == agent && p.request == request)
            {
                guard.take();
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    pub fn get(&self) -> Option<StreamPreview> {
        self.inner.lock().ok().and_then(|g| g.clone())
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}

/// Estado compartido por los hilos de despacho Gemini (firmas de pensamiento, cola
/// secuencial de tool calls, telemetría). `pub(crate)` para que `DynamicLlmDriver`
/// (`SDD-03` Fase 2) lo construya con la cola de tool calls compartida con OpenAI.
#[derive(Clone, Default)]
pub(crate) struct GeminiSharedState {
    pub(crate) thought_signatures: SignatureMap,
    pub(crate) pending_calls: PendingCallQueue,
    pub(crate) project_instructions: Arc<String>,
    pub(crate) telemetry: Option<SessionTelemetryHandle>,
    pub(crate) context_config: TieredContextConfig,
    pub(crate) effort: Option<ReasoningEffort>,
    /// ids de drivers de harness CLI externos habilitados (`SDD-10` Fase 1): enriquecen el
    /// esquema dinámico de `spawn_subagent` (`driver` opcional).
    pub(crate) driver_ids: Vec<String>,
    /// Schemas MCP `mcp__*` del registro de arranque (`SDD-04 REQ-04.3`): se
    /// declaran en el body filtradas por los tools del spec del agente.
    pub(crate) mcp_schemas: Arc<Vec<ToolSchema>>,
    /// `FIX-02`: timeouts HTTP de las llamadas al proveedor (connect/read).
    pub(crate) timeouts: LlmTimeouts,
}

impl GeminiSharedState {
    pub(crate) fn clear_thought_signatures(&self) {
        if let Ok(mut guard) = self.thought_signatures.lock() {
            guard.clear();
        }
    }

    pub(crate) fn prune_thought_signatures_from(&self, agent: u64, from_call_index: usize) {
        if let Ok(mut guard) = self.thought_signatures.lock() {
            guard.retain(|&(a, idx), _| a != agent || idx < from_call_index);
        }
    }
}

/// Carga instrucciones de proyecto desde `~/.onyx/{AGENTS,ONYX}.md` y `<workspace>/{AGENTS,ONYX}.md`,
/// aplicando `scrub_secrets` y limitando el conjunto a 16 KiB.
pub fn load_project_instructions(workspace_root: &Path) -> String {
    let mut candidates: Vec<(String, std::path::PathBuf)> = Vec::new();
    if let Some(home) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
        let onyx_dir = Path::new(&home).join(".onyx");
        candidates.push(("~/.onyx/AGENTS.md".to_string(), onyx_dir.join("AGENTS.md")));
        candidates.push(("~/.onyx/ONYX.md".to_string(), onyx_dir.join("ONYX.md")));
    }
    candidates.push(("AGENTS.md".to_string(), workspace_root.join("AGENTS.md")));
    candidates.push(("ONYX.md".to_string(), workspace_root.join("ONYX.md")));

    let mut combined = String::new();
    for (label, path) in candidates {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let cleaned = scrub_secrets(raw.trim());
        if cleaned.is_empty() {
            continue;
        }
        let section = format!("## {label}\n{cleaned}\n");
        if combined.len() + section.len() > MAX_PROJECT_INSTRUCTIONS_BYTES {
            let remaining = MAX_PROJECT_INSTRUCTIONS_BYTES.saturating_sub(combined.len());
            if remaining > label.len() + 16 {
                let mut cut = remaining;
                while cut > 0 && !section.is_char_boundary(cut) {
                    cut -= 1;
                }
                combined.push_str(&section[..cut]);
                combined.push_str("\n...[truncated]\n");
            }
            break;
        }
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&section);
    }

    let skills_summary = discover_skills_summary(workspace_root);
    if !skills_summary.is_empty()
        && combined.len() + skills_summary.len() + 2 <= MAX_PROJECT_INSTRUCTIONS_BYTES
    {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(skills_summary.trim_end());
        combined.push('\n');
    }
    combined
}

#[derive(Debug)]
pub(crate) struct QueuedToolCall {
    pub name: String,
    pub args: String,
}

pub(crate) fn next_queued_call(pending_calls: &PendingCallQueue, req: &AgentView) -> Option<Reply> {
    let mut guard = pending_calls.lock().ok()?;
    if req.memory.last().map(|(k, _)| k.as_str()) != Some("observe") {
        guard.remove(&req.agent.0);
        return None;
    }
    let queue = guard.get_mut(&req.agent.0)?;
    let (name, args) = queue.pop_front()?;
    if queue.is_empty() {
        guard.remove(&req.agent.0);
    }
    Some(Reply::call_structured(req.agent, req.request, name, args))
}

pub(crate) fn push_queued_calls(
    pending_calls: &PendingCallQueue,
    agent: ConceptId,
    calls: impl IntoIterator<Item = QueuedToolCall>,
) {
    if let Ok(mut guard) = pending_calls.lock() {
        let queue = guard.entry(agent.0).or_default();
        for c in calls {
            queue.push_back((c.name, c.args));
        }
        if queue.is_empty() {
            guard.remove(&agent.0);
        }
    }
}

pub(crate) struct InFlightGuard(pub Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

#[allow(dead_code)]
fn build_request_body(
    req: &AgentView,
    thought_signatures: &HashMap<(u64, usize), JsonValue>,
) -> JsonValue {
    build_request_body_with_budget(req, thought_signatures, ContextBudget::default())
}

fn build_request_body_with_budget(
    req: &AgentView,
    thought_signatures: &HashMap<(u64, usize), JsonValue>,
    budget: ContextBudget,
) -> JsonValue {
    build_request_body_with_context(
        req,
        thought_signatures,
        budget.into(),
        "",
        None,
        &[],
        &[],
        DEFAULT_GEMINI_MODEL,
    )
}

const BASE_INSTRUCTION: &str = "You are openOnyx, a precise software engineering agent. When mapping architecture or inspecting unfamiliar crates, call `scent_outline` first to get compact @TOON v1 signatures and line ranges, call `scent_impact` before editing shared symbols, and use `fs_read` with bounded `start_line`/`end_line` ranges rather than reading whole files unnecessarily. Do not re-read unchanged files or repeat `scent_outline` queries already summarized in your history unless the file was modified. Never create temporary helper scripts or scratch directories inside the workspace repository; use built-in tools (`glob_files`, `code_search`, `scent_outline`) for counting and inspection. Never use `shell_exec` to read, list, search files (such as `nl`, `cat`, `grep`, `find`), or inspect git status/diff; always use `fs_read`, `fs_list`, `code_search`, `scent_outline`, `scent_impact`, `scent_references`, `git_status`, `git_diff`, and `load_skill` (which are read-only and require no approval), reserving `shell_exec` strictly for builds, tests, or mutating commands. When handling multi-step workspace tasks, maintain progress visibly with `plan_update` (`pending`, `in_progress`, `completed`), but never call `plan_update` multiple times in a row without executing real workspace tools in between; for purely conceptual questions or single-step answers that do not inspect or modify the workspace, omit `plan_update` (or call it at most once). Call `load_skill` whenever a specialized skill (e.g. `scent`, `rust`, or a listed workspace skill) applies. Verify exact dependency and enum names with `fs_read` before citing them. Never invent or paraphrase symbol, struct, trait, enum, or function names that did not literally appear in `scent_outline`, `scent_impact`, or `fs_read` output; if `scent_outline` returns `# next_cursor=...`, paginate with `cursor` before summarizing unread symbols. For multi-region updates to a single file, prefer multi-hunk `fs_edit` (`edits: [{ old_text, new_text }]`). You can delegate bounded subtasks with `spawn_agent` when useful. Read files before editing them, keep changes minimal, and summarize your final answer clearly when done.";

pub(crate) fn host_os_instruction_hint() -> &'static str {
    if cfg!(windows) {
        "Host environment: Windows (PowerShell). Never use Unix commands (`find`, `xargs`, `wc`, `ls`, `grep`, `cat`), `/dev/null` (use `$null`), or `.ps1` script files (ExecutionPolicy blocks them)."
    } else {
        "Host environment: Unix/POSIX (`sh`)."
    }
}

pub(crate) fn system_instruction_text(project_instructions: &str) -> String {
    let os_hint = host_os_instruction_hint();
    if project_instructions.trim().is_empty() {
        format!("{BASE_INSTRUCTION}\n\n{os_hint}")
    } else {
        format!(
            "{BASE_INSTRUCTION}\n\n{os_hint}\n\n# Project Instructions (ONYX.md / AGENTS.md)\n{project_instructions}"
        )
    }
}

/// Construye el body `generateContent` con proyección jerárquica de contexto,
/// firmas de pensamiento y esfuerzo de razonamiento. `pub(crate)` para
/// `DynamicLlmDriver` (`SDD-03` Fase 2), que construye el body síncrono en el
/// hilo principal antes de `thread::spawn`.
fn push_turn_parts(contents: &mut Vec<JsonValue>, role: &str, mut parts: Vec<JsonValue>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            if let Some(existing) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                existing.append(&mut parts);
                return;
            }
        }
    }
    contents.push(json!({
        "role": role,
        "parts": parts
    }));
}

fn gemini_thinking_config(model: &str, effort: ReasoningEffort) -> Option<JsonValue> {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("gemini-1.") {
        return None;
    }
    if lower.starts_with("gemini-2.0") {
        if lower.contains("thinking") {
            return Some(json!({ "includeThoughts": true }));
        }
        return None;
    }
    if lower.starts_with("gemini-2.5") {
        let budget = match effort {
            ReasoningEffort::Low => 2048,
            ReasoningEffort::Medium => 8192,
            ReasoningEffort::High => 24576,
        };
        return Some(json!({
            "thinkingBudget": budget,
            "includeThoughts": true
        }));
    }
    let level = if lower.starts_with("gemini-3-pro") && effort == ReasoningEffort::Medium {
        "HIGH"
    } else {
        effort.as_gemini_level()
    };
    Some(json!({
        "thinkingLevel": level,
        "includeThoughts": true
    }))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_request_body_with_context(
    req: &AgentView,
    thought_signatures: &HashMap<(u64, usize), JsonValue>,
    budget: TieredContextConfig,
    project_instructions: &str,
    effort: Option<ReasoningEffort>,
    driver_ids: &[String],
    mcp_schemas: &[ToolSchema],
    model: &str,
) -> JsonValue {
    let projected = project_memory(&req.goal, &req.memory, &budget);
    let mut contents: Vec<JsonValue> = Vec::with_capacity(projected.turns.len());
    for turn in projected.turns {
        match turn {
            ProjectedTurn::User(text) => {
                push_turn_parts(&mut contents, "user", vec![json!({ "text": text })]);
            }
            ProjectedTurn::AssistantText(text) => {
                push_turn_parts(&mut contents, "model", vec![json!({ "text": text })]);
            }
            ProjectedTurn::ToolCall {
                call_index,
                name,
                args_json,
                ..
            } => {
                let parsed_args = match serde_json::from_str::<JsonValue>(&args_json) {
                    Ok(JsonValue::Object(map)) => JsonValue::Object(map),
                    Ok(JsonValue::Null) => json!({}),
                    _ => json!({ "raw": args_json }),
                };
                let mut part = json!({
                    "functionCall": { "name": name, "args": parsed_args }
                });
                if let Some(sig) = thought_signatures.get(&(req.agent.0, call_index)) {
                    part["thoughtSignature"] = sig.clone();
                }
                push_turn_parts(&mut contents, "model", vec![part]);
            }
            ProjectedTurn::ToolObserve { name, content, .. } => {
                push_turn_parts(
                    &mut contents,
                    "user",
                    vec![json!({
                        "functionResponse": {
                            "name": name,
                            "response": { "result": content }
                        }
                    })],
                );
            }
            ProjectedTurn::ToolError { name, error, .. } => {
                push_turn_parts(
                    &mut contents,
                    "user",
                    vec![json!({
                        "functionResponse": {
                            "name": name,
                            "response": { "error": error }
                        }
                    })],
                );
            }
            ProjectedTurn::ToolDenied { name, reason, .. } => {
                let msg_text = format!("Tool call denied: {reason}. Adjust your plan.");
                push_turn_parts(
                    &mut contents,
                    "user",
                    vec![json!({
                        "functionResponse": {
                            "name": name,
                            "response": { "error": msg_text }
                        }
                    })],
                );
            }
        }
    }

    let mut declarations: Vec<JsonValue> =
        WorkspaceTools::schemas_with_subagents_and_drivers(true, driver_ids)
            .into_iter()
            .filter(|s| req.tools.iter().any(|t| t.as_str() == s.name.as_ref()))
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "parameters": s.parameters
                })
            })
            .collect();
    // `SDD-04 REQ-04.3`: declaraciones MCP `mcp__*` (schemas del registro,
    // filtradas por los tools del spec y normalizados al formato
    // `functionDeclarations` de Gemini: ver `sanitize_gemini_mcp_schema`).
    for s in mcp_schemas
        .iter()
        .filter(|s| req.tools.iter().any(|t| t.as_str() == s.name.as_ref()))
    {
        let mut params = s.parameters.clone();
        sanitize_gemini_mcp_schema(&mut params);
        declarations.push(json!({
            "name": s.name,
            "description": s.description,
            "parameters": params
        }));
    }

    let system_text = system_instruction_text(project_instructions);

    let mut body = json!({
        "systemInstruction": {
            "parts": [{
                "text": system_text
            }]
        },
        "contents": contents
    });

    if !declarations.is_empty() {
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    }
    if let Some(eff) = effort {
        if let Some(thinking_cfg) = gemini_thinking_config(model, eff) {
            body["generationConfig"] = json!({
                "thinkingConfig": thinking_cfg
            });
        }
    }
    body
}

/// `SDD-04 REQ-04.3`: normaliza un `inputSchema` de tool MCP al formato de
/// `functionDeclarations` de Gemini:
/// - `type` → MAYÚSCULAS (`OBJECT`, `STRING`, `INTEGER`, `NUMBER`, `BOOLEAN`, `ARRAY`);
/// - `["string", "null"]` → `"STRING"` + `"nullable": true`;
/// - elimina recursivamente los metadatos que Gemini rechaza: `$schema`,
///   `additionalProperties`, `$id`, `$defs`, `definitions`, `default`, `examples`,
///   `title`.
fn sanitize_gemini_mcp_schema(val: &mut JsonValue) {
    const STRIP_KEYS: [&str; 25] = [
        "$schema",
        "additionalProperties",
        "$id",
        "$defs",
        "definitions",
        "default",
        "examples",
        "title",
        "$ref",
        "$comment",
        "$anchor",
        "$dynamicRef",
        "$vocabulary",
        "oneOf",
        "allOf",
        "not",
        "const",
        "patternProperties",
        "unevaluatedProperties",
        "propertyNames",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "dependentRequired",
        "dependentSchemas",
    ];
    const EXTRA_STRIP_KEYS: [&str; 9] = [
        "if",
        "then",
        "else",
        "contentMediaType",
        "contentEncoding",
        "readOnly",
        "writeOnly",
        "deprecated",
        "discriminator",
    ];
    match val {
        JsonValue::Object(map) => {
            if let Some(JsonValue::String(s)) = map.get("const").cloned() {
                if !map.contains_key("enum") {
                    map.insert(
                        "enum".to_string(),
                        JsonValue::Array(vec![JsonValue::String(s)]),
                    );
                    if !map.contains_key("type") {
                        map.insert("type".to_string(), JsonValue::String("STRING".to_string()));
                    }
                }
            }
            if let Some(ty) = map.get("type").and_then(|v| v.as_str()).map(str::to_owned) {
                if ty.eq_ignore_ascii_case("null") {
                    map.remove("type");
                    map.insert("nullable".to_string(), JsonValue::Bool(true));
                } else {
                    map.insert(
                        "type".to_string(),
                        JsonValue::String(ty.to_ascii_uppercase()),
                    );
                }
            } else if let Some(JsonValue::Array(tys)) = map.get("type") {
                // Clonamos el array: mutar `map` con `tys` vivo no compila
                // (préstamo inmutable pendiente del `map.get`).
                let tys: Vec<JsonValue> = tys.to_vec();
                if tys
                    .iter()
                    .any(|t| t.as_str().is_some_and(|s| s.eq_ignore_ascii_case("null")))
                {
                    map.insert("nullable".to_string(), JsonValue::Bool(true));
                }
                let kept: Vec<JsonValue> = tys
                    .into_iter()
                    .filter(|t| t.as_str().is_none_or(|s| !s.eq_ignore_ascii_case("null")))
                    .filter_map(|t| {
                        t.as_str()
                            .map(|s| JsonValue::String(s.to_ascii_uppercase()))
                    })
                    .collect();
                match kept.len() {
                    0 => {
                        map.remove("type");
                    }
                    1 => {
                        map.insert("type".to_string(), kept.into_iter().next().unwrap());
                    }
                    _ => {
                        map.remove("type");
                        if !map.contains_key("anyOf") {
                            let union: Vec<JsonValue> =
                                kept.into_iter().map(|t| json!({ "type": t })).collect();
                            map.insert("anyOf".to_string(), JsonValue::Array(union));
                        }
                    }
                }
            }
            for key in STRIP_KEYS.iter().chain(EXTRA_STRIP_KEYS.iter()) {
                map.remove(*key);
            }
            for (k, v) in map.iter_mut() {
                if k == "properties" {
                    // Las claves de `"properties"` son nombres de argumentos de la tool
                    // (p. ej. `title`, `default`, `const`, `readOnly`, `deprecated`),
                    // no keywords JSON Schema: sanitizar solo el esquema de cada propiedad.
                    if let JsonValue::Object(props) = v {
                        for prop_schema in props.values_mut() {
                            sanitize_gemini_mcp_schema(prop_schema);
                        }
                    }
                } else {
                    sanitize_gemini_mcp_schema(v);
                }
            }
        }
        JsonValue::Array(arr) => {
            for v in arr {
                sanitize_gemini_mcp_schema(v);
            }
        }
        _ => {}
    }
}

/// Parsea una respuesta `generateContent` (REST o SSE acumulada) y registra
/// telemetría por la **llamada real** (`SDD-03` Fase 2: `record_turn_for` con el
/// modelo/endpoint despachados, no los del handle, que pueden haber cambiado
/// en caliente entre el despacho y la respuesta).
pub(crate) fn parse_gemini_response(
    json: &JsonValue,
    agent: ConceptId,
    request: u64,
    call_index: usize,
    shared: &GeminiSharedState,
    model: &str,
    is_local_endpoint: bool,
) -> Reply {
    if let Some(usage) = extract_gemini_usage(json) {
        if let Some(telemetry) = &shared.telemetry {
            telemetry.record_turn_for(model, is_local_endpoint, usage);
        }
    }
    if let Some(err) = json.get("error") {
        let code = err
            .get("code")
            .and_then(|v| v.as_i64())
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Reply::fail(agent, request, format!("gemini API error ({code}): {msg}"));
    }
    let parts = json["candidates"][0]["content"]["parts"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut function_calls = Vec::new();
    for part in &parts {
        if let Some(fc) = part.get("functionCall") {
            function_calls.push((part, fc));
        }
    }

    if !function_calls.is_empty() {
        let mut parsed = Vec::with_capacity(function_calls.len());
        let mut batch_sig: Option<JsonValue> = None;
        for (part, fc) in function_calls {
            if let Some(sig) = part.get("thoughtSignature") {
                batch_sig = Some(sig.clone());
            }
            let name = fc["name"].as_str().unwrap_or_default().trim().to_string();
            if name.is_empty() {
                continue;
            }
            if let Some(ref sig) = batch_sig {
                if let Ok(mut guard) = shared.thought_signatures.lock() {
                    guard.insert((agent.0, call_index + parsed.len()), sig.clone());
                }
            }
            let args = match fc.get("args") {
                Some(JsonValue::Object(map)) => {
                    serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string())
                }
                _ => "{}".to_string(),
            };
            parsed.push(QueuedToolCall { name, args });
        }

        if parsed.is_empty() {
            return Reply::fail(
                agent,
                request,
                "gemini: malformed functionCall with empty name",
            );
        }

        let mut iter = parsed.into_iter();
        let first = iter.next().expect("non-empty parsed function_calls");
        push_queued_calls(&shared.pending_calls, agent, iter);
        return Reply::call_structured(agent, request, first.name, first.args);
    }

    let text: String = parts
        .iter()
        .filter_map(|p| {
            if p.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                return None;
            }
            p.get("text").and_then(|t| t.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");

    if text.is_empty() {
        if let Some(block_reason) = json
            .pointer("/promptFeedback/blockReason")
            .and_then(|v| v.as_str())
        {
            return Reply::fail(
                agent,
                request,
                format!("gemini: prompt blocked ({block_reason})"),
            );
        }
        let reason = json["candidates"][0]["finishReason"]
            .as_str()
            .unwrap_or("unknown");
        Reply::fail(
            agent,
            request,
            format!("empty gemini response (finishReason={reason}): {json}"),
        )
    } else {
        Reply::done(agent, request, text)
    }
}

const MAX_RETRY_AFTER_MS: u64 = 10_000;

fn is_retryable_http_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
}

fn parse_retry_after_ms(resp: &ureq::Response) -> Option<u64> {
    let raw = resp.header("retry-after")?.trim();
    let secs: u64 = raw.parse().ok()?;
    Some(secs.saturating_mul(1_000).min(MAX_RETRY_AFTER_MS))
}

fn is_retryable_gemini_stream_error(msg: &str) -> bool {
    is_retryable_stream_error(msg)
        || msg.contains("truncated SSE stream")
        || msg.contains("retryable SSE error")
}

fn is_retryable_sse_error_obj(err: &JsonValue) -> bool {
    let code = err.get("code").and_then(|v| v.as_u64()).unwrap_or(0);
    if matches!(code, 408 | 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    matches!(
        err.get("status").and_then(|v| v.as_str()),
        Some("UNAVAILABLE" | "RESOURCE_EXHAUSTED" | "DEADLINE_EXCEEDED" | "INTERNAL")
    )
}

struct GeminiHttpError {
    reply: Reply,
    retryable: bool,
    retry_after_ms: Option<u64>,
}

fn gemini_retry_delay_ms(attempt: usize, retry_after_ms: Option<u64>) -> Option<u64> {
    let &base = RETRY_BACKOFF_MS.get(attempt)?;
    let delay = match retry_after_ms {
        Some(r) => base.max(r.min(MAX_RETRY_AFTER_MS)),
        None => base,
    };
    Some(delay)
}

fn post_gemini_json(
    url: &str,
    api_key: &str,
    body: &JsonValue,
    agent: ConceptId,
    request: u64,
    timeouts: LlmTimeouts,
) -> Result<ureq::Response, GeminiHttpError> {
    // `FIX-02`: timeouts explícitos por petición (connect corto + read por
    // chunk SSE). Sin `timeout_read`, un proveedor mudo bloquea el read
    // eternamente (ureq por defecto: "requests may block forever on reads").
    let http = ureq::AgentBuilder::new()
        .timeout_connect(timeouts.connect)
        .timeout_read(timeouts.read)
        .build();
    match http
        .post(url)
        .set("x-goog-api-key", api_key)
        .send_json(body)
    {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, r)) => {
            let retry_after_ms = parse_retry_after_ms(&r);
            let err_body = r.into_string().unwrap_or_default();
            Err(GeminiHttpError {
                reply: Reply::fail(agent, request, format!("gemini HTTP {code}: {err_body}")),
                retryable: is_retryable_http_status(code),
                retry_after_ms,
            })
        }
        Err(ureq::Error::Transport(t)) => Err(GeminiHttpError {
            reply: Reply::fail(
                agent,
                request,
                format!(
                    "gemini transport error ({:?}): {}",
                    t.kind(),
                    transport_detail(&t)
                ),
            ),
            // `FIX-02` (cierre de auditoría): read timeout NO es reintenable.
            retryable: !is_transport_read_timeout(&t),
            retry_after_ms: None,
        }),
    }
}

pub(crate) fn accumulate_gemini_sse_reader<R: std::io::BufRead>(
    reader: R,
    agent: ConceptId,
    request: u64,
    preview: Option<&StreamPreviewHandle>,
) -> Result<JsonValue, String> {
    let mut text_acc = String::new();
    let mut thought_acc = String::new();
    let mut call_parts: Vec<JsonValue> = Vec::new();
    let mut last_finish_reason: Option<String> = None;
    let mut last_usage: Option<JsonValue> = None;
    let mut last_error: Option<JsonValue> = None;
    let mut last_prompt_feedback: Option<JsonValue> = None;
    let mut last_non_call_sig: Option<JsonValue> = None;
    let mut saw_sse_data = false;
    let mut saw_done = false;
    let mut raw_fallback = String::new();

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(e) => {
                if (saw_done || last_finish_reason.is_some())
                    && (!text_acc.is_empty() || !call_parts.is_empty())
                {
                    break;
                }
                return Err(e.to_string());
            }
        };
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("data:") {
            let data = rest.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                saw_done = true;
                continue;
            }
            let chunk: JsonValue = serde_json::from_str(data).map_err(|e| e.to_string())?;
            saw_sse_data = true;
            if let Some(err) = chunk.get("error") {
                if !err.is_null() {
                    if is_retryable_sse_error_obj(err) {
                        let code = err.get("code").and_then(|v| v.as_u64()).unwrap_or(0);
                        let msg = err
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        return Err(format!("retryable SSE error ({code}): {msg}"));
                    }
                    last_error = Some(err.clone());
                }
            }
            if let Some(pf) = chunk.get("promptFeedback") {
                if !pf.is_null() {
                    last_prompt_feedback = Some(pf.clone());
                }
            }
            if let Some(usage) = chunk.get("usageMetadata") {
                if !usage.is_null() {
                    last_usage = Some(usage.clone());
                }
            }
            if let Some(candidate) = chunk.get("candidates").and_then(|c| c.get(0)) {
                if let Some(fr) = candidate.get("finishReason").and_then(|v| v.as_str()) {
                    last_finish_reason = Some(fr.to_string());
                }
                if let Some(parts) = candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    for part in parts {
                        if let Some(fc) = part.get("functionCall") {
                            call_parts.push(part.clone());
                            if text_acc.is_empty() {
                                if let Some(prev) = preview {
                                    let name =
                                        fc.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                                    prev.set(agent.0, request, format!("[calling `{name}`...]"));
                                }
                            }
                        } else {
                            if let Some(sig) = part.get("thoughtSignature") {
                                last_non_call_sig = Some(sig.clone());
                            }
                            if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                    thought_acc.push_str(t);
                                    if let Some(prev) = preview {
                                        prev.set_thought(agent.0, request, &thought_acc);
                                    }
                                }
                                continue;
                            } else if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                text_acc.push_str(t);
                                if let Some(prev) = preview {
                                    prev.set(agent.0, request, &text_acc);
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
        if let Some(err) = last_error {
            let mut out = json!({ "error": err });
            if let Some(usage) = last_usage {
                out["usageMetadata"] = usage;
            }
            return Ok(out);
        }

        let mut parts = Vec::new();
        if !text_acc.is_empty() {
            let mut text_part = json!({ "text": text_acc });
            if call_parts.is_empty() {
                if let Some(sig) = last_non_call_sig.take() {
                    text_part["thoughtSignature"] = sig;
                }
            }
            parts.push(text_part);
        }
        if let (Some(first_call), Some(sig)) = (call_parts.first_mut(), last_non_call_sig.take()) {
            if first_call.get("thoughtSignature").is_none() {
                first_call["thoughtSignature"] = sig;
            }
        }
        parts.extend(call_parts);

        if parts.is_empty() {
            if let Some(pf) = last_prompt_feedback {
                let mut out = json!({ "promptFeedback": pf, "candidates": [] });
                if let Some(usage) = last_usage {
                    out["usageMetadata"] = usage;
                }
                return Ok(out);
            }
        }

        if !saw_done && last_finish_reason.is_none() {
            return Err("truncated SSE stream: missing finishReason".to_string());
        }

        let mut cand_obj = serde_json::Map::new();
        cand_obj.insert("content".to_string(), json!({ "parts": parts }));
        if let Some(fr) = last_finish_reason {
            cand_obj.insert("finishReason".to_string(), JsonValue::String(fr));
        }
        let mut out = json!({
            "candidates": [JsonValue::Object(cand_obj)]
        });
        if let Some(usage) = last_usage {
            out["usageMetadata"] = usage;
        }
        if let Some(pf) = last_prompt_feedback {
            out["promptFeedback"] = pf;
        }
        Ok(out)
    } else {
        serde_json::from_str(&raw_fallback).map_err(|e| e.to_string())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_gemini_http(
    url: &str,
    api_key: &str,
    body: JsonValue,
    agent: ConceptId,
    request: u64,
    call_index: usize,
    shared: &GeminiSharedState,
    model: &str,
    is_local_endpoint: bool,
) -> Reply {
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        let resp = match post_gemini_json(url, api_key, &body, agent, request, shared.timeouts) {
            Ok(r) => r,
            Err(err) => {
                if err.retryable {
                    if let Some(delay_ms) = gemini_retry_delay_ms(attempt, err.retry_after_ms) {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
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
                if is_retryable_gemini_stream_error(&msg) {
                    if let Some(delay_ms) = gemini_retry_delay_ms(attempt, None) {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        continue;
                    }
                }
                return Reply::fail(agent, request, format!("gemini json error: {e}"));
            }
        };

        return parse_gemini_response(
            &json,
            agent,
            request,
            call_index,
            shared,
            model,
            is_local_endpoint,
        );
    }
    unreachable!("retry loop always returns on final attempt")
}

/// Parámetros inmutables de una llamada SSE `streamGenerateContent`
/// (`SDD-03` Fase 2: incluye el modelo/endpoint reales de la llamada para
/// telemetría por llamada). `pub(crate)` para `DynamicLlmDriver`.
pub(crate) struct GeminiStreamTarget<'a> {
    pub(crate) url: &'a str,
    pub(crate) api_key: &'a str,
    pub(crate) agent: ConceptId,
    pub(crate) request: u64,
    pub(crate) call_index: usize,
    pub(crate) shared: &'a GeminiSharedState,
    pub(crate) preview: Option<&'a StreamPreviewHandle>,
    pub(crate) model: &'a str,
    pub(crate) is_local_endpoint: bool,
}

pub(crate) fn execute_gemini_stream_http(target: GeminiStreamTarget<'_>, body: JsonValue) -> Reply {
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        let resp = match post_gemini_json(
            target.url,
            target.api_key,
            &body,
            target.agent,
            target.request,
            target.shared.timeouts,
        ) {
            Ok(r) => r,
            Err(err) => {
                if let Some(prev) = target.preview {
                    prev.clear_for(target.agent.0, target.request);
                }
                if err.retryable {
                    if let Some(delay_ms) = gemini_retry_delay_ms(attempt, err.retry_after_ms) {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        continue;
                    }
                }
                return err.reply;
            }
        };
        let reader = std::io::BufReader::new(resp.into_reader());
        match accumulate_gemini_sse_reader(reader, target.agent, target.request, target.preview) {
            Ok(json) => {
                if let Some(prev) = target.preview {
                    prev.clear_for(target.agent.0, target.request);
                }
                return parse_gemini_response(
                    &json,
                    target.agent,
                    target.request,
                    target.call_index,
                    target.shared,
                    target.model,
                    target.is_local_endpoint,
                );
            }
            Err(e) => {
                if let Some(prev) = target.preview {
                    prev.clear_for(target.agent.0, target.request);
                }
                if is_retryable_gemini_stream_error(&e) {
                    if let Some(delay_ms) = gemini_retry_delay_ms(attempt, None) {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        continue;
                    }
                }
                return Reply::fail(
                    target.agent,
                    target.request,
                    format!("gemini stream error: {e}"),
                );
            }
        }
    }
    unreachable!("stream retry loop always returns on final attempt")
}

/// Adaptador síncrono para Google AI Studio (Gemini `generateContent` REST API).
pub struct GeminiResponder {
    api_key: String,
    model: String,
    api_base: String,
    shared: GeminiSharedState,
}

impl GeminiResponder {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            api_base: DEFAULT_GEMINI_API_BASE.to_string(),
            shared: GeminiSharedState::default(),
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

    pub fn with_external_driver_ids(mut self, driver_ids: Vec<String>) -> Self {
        self.shared.driver_ids = driver_ids;
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

impl Responder for GeminiResponder {
    fn respond(&mut self, req: &AgentView) -> Option<Reply> {
        if let Some(queued_reply) = next_queued_call(&self.shared.pending_calls, req) {
            return Some(queued_reply);
        }
        let is_local = is_loopback_host(api_base_host(&self.api_base));
        let url = format!("{}/models/{}:generateContent", self.api_base, self.model);
        let call_index = req.memory.iter().filter(|(k, _)| k == "call").count();
        self.shared
            .prune_thought_signatures_from(req.agent.0, call_index);
        let body = {
            let sigs = self
                .shared
                .thought_signatures
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            build_request_body_with_context(
                req,
                &sigs,
                self.shared.context_config,
                &self.shared.project_instructions,
                self.shared.effort,
                &self.shared.driver_ids,
                &self.shared.mcp_schemas,
                &self.model,
            )
        };
        Some(execute_gemini_http(
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

/// Driver asíncrono no bloqueante para la TUI de `Mode::Chat` (solo tests).
///
/// En producción (`SDD-03` Fase 2) `DynamicLlmDriver` sustituye a este driver en
/// `chat`/`serve`: el objetivo LLM se congela por petición en el hilo principal,
/// mientras que `AsyncGeminiDriver` se mantenía fijo al modelo de arranque.
/// Sigue existiendo bajo `#[cfg(test)]` como fixture de despacho SSE.
#[cfg(test)]
pub struct AsyncGeminiDriver {
    vocab: Vocab,
    api_key: String,
    model: String,
    api_base: String,
    shared: GeminiSharedState,
    dispatched: HashSet<(u64, u64)>,
    in_flight_count: Arc<AtomicUsize>,
    has_ready: Arc<AtomicBool>,
    stream_preview: Option<StreamPreviewHandle>,
    rx: mpsc::Receiver<Reply>,
    tx: mpsc::Sender<Reply>,
    inner: Option<Box<dyn PlatformDriver>>,
}

#[cfg(test)]
impl AsyncGeminiDriver {
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
            api_base: DEFAULT_GEMINI_API_BASE.to_string(),
            shared: GeminiSharedState::default(),
            dispatched: HashSet::new(),
            in_flight_count,
            has_ready,
            stream_preview: None,
            rx,
            tx,
            inner: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_with<F>(&mut self, req: &AgentView, spawn_fn: F)
    where
        F: FnOnce(
            String,
            String,
            JsonValue,
            ConceptId,
            u64,
            usize,
            GeminiSharedState,
            InFlightGuard,
            Arc<AtomicBool>,
            mpsc::Sender<Reply>,
        ),
    {
        let key = (req.agent.0, req.request);
        if !self.dispatched.insert(key) {
            return;
        }

        if let Some(queued_reply) = next_queued_call(&self.shared.pending_calls, req) {
            if self.tx.send(queued_reply).is_ok() {
                self.has_ready.store(true, Ordering::Release);
            }
            return;
        }

        self.in_flight_count.fetch_add(1, Ordering::Release);
        let guard = InFlightGuard(Arc::clone(&self.in_flight_count));
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.api_base, self.model
        );
        let call_index = req.memory.iter().filter(|(k, _)| k == "call").count();
        self.shared
            .prune_thought_signatures_from(req.agent.0, call_index);
        let body = {
            let sigs = self
                .shared
                .thought_signatures
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            build_request_body_with_context(
                req,
                &sigs,
                self.shared.context_config,
                &self.shared.project_instructions,
                self.shared.effort,
                &self.shared.driver_ids,
                &self.shared.mcp_schemas,
                &self.model,
            )
        };

        spawn_fn(
            url,
            self.api_key.clone(),
            body,
            req.agent,
            req.request,
            call_index,
            self.shared.clone(),
            guard,
            Arc::clone(&self.has_ready),
            self.tx.clone(),
        );
    }
}

#[cfg(test)]
impl PlatformDriver for AsyncGeminiDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Some(inner) = self.inner.as_mut() {
            inner.poll(bus);
        }
        self.has_ready.store(false, Ordering::Release);
        while let Ok(reply) = self.rx.try_recv() {
            if let Some(prev) = &self.stream_preview {
                prev.clear_for(reply.agent.0, reply.request);
            }
            bus.push(reply.to_event());
        }
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
        let preview = self.stream_preview.clone();
        for req in pending_requests(graph, &self.vocab) {
            let thread_preview = preview.clone();
            let model = self.model.clone();
            let is_local = is_loopback_host(api_base_host(&self.api_base));
            self.dispatch_with(
                &req,
                |url, api_key, body, agent, request, call_index, shared, guard, ready_flag, tx| {
                    thread::spawn(move || {
                        let _guard = guard;
                        let reply = execute_gemini_stream_http(
                            GeminiStreamTarget {
                                url: &url,
                                api_key: &api_key,
                                agent,
                                request,
                                call_index,
                                shared: &shared,
                                preview: thread_preview.as_ref(),
                                model: &model,
                                is_local_endpoint: is_local,
                            },
                            body,
                        );
                        if tx.send(reply).is_ok() {
                            ready_flag.store(true, Ordering::Release);
                        }
                    });
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onyx_soul_agents::{AgentGraph, OnyxEvent};

    struct VecBus(Vec<OnyxEvent>);

    impl EventBus for VecBus {
        fn push(&mut self, event: OnyxEvent) {
            self.0.push(event);
        }
        fn drain(&mut self) -> Vec<OnyxEvent> {
            std::mem::take(&mut self.0)
        }
        fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }

    fn dummy_agent_view(agent_id: u64, request: u64) -> AgentView {
        AgentView {
            agent: ConceptId(agent_id),
            request,
            name: "test".to_string(),
            goal: "check async".to_string(),
            tools: vec![],
            memory: vec![],
            pending_approval: None,
            props: vec![],
        }
    }

    #[test]
    fn async_driver_dispatches_once_and_delivers_on_poll() {
        let mut graph = AgentGraph::new();
        let vocab = Vocab::resolve(&mut graph);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let has_ready = Arc::new(AtomicBool::new(false));
        let mut driver = AsyncGeminiDriver::new(
            vocab,
            "fake-key",
            DEFAULT_GEMINI_MODEL,
            Arc::clone(&in_flight),
            Arc::clone(&has_ready),
        );
        let req = dummy_agent_view(7, 42);

        let mut captured_tx = None;
        let mut captured_guard = None;
        let mut captured_ready = None;
        driver.dispatch_with(
            &req,
            |_url, _key_str, _body, _agent, _request, _idx, _shared, guard, ready_flag, tx| {
                captured_guard = Some(guard);
                captured_ready = Some(ready_flag);
                captured_tx = Some(tx);
            },
        );
        assert_eq!(in_flight.load(Ordering::Acquire), 1);
        assert!(!has_ready.load(Ordering::Acquire));

        // Repeated dispatch for the same (agent, request) is ignored.
        let mut spawned_again = false;
        driver.dispatch_with(&req, |_, _, _, _, _, _, _, _, _, _| {
            spawned_again = true;
        });
        assert!(!spawned_again);
        assert_eq!(in_flight.load(Ordering::Acquire), 1);

        // Simulate worker finishing: send reply, mark ready, then drop guard.
        captured_tx
            .unwrap()
            .send(Reply::done(ConceptId(7), 42, "all good"))
            .unwrap();
        captured_ready.unwrap().store(true, Ordering::Release);
        drop(captured_guard);
        assert_eq!(in_flight.load(Ordering::Acquire), 0);
        assert!(has_ready.load(Ordering::Acquire));

        let mut bus = VecBus(Vec::new());
        driver.poll(&mut bus);
        assert_eq!(bus.0.len(), 1);
        assert!(!has_ready.load(Ordering::Acquire));
    }

    #[test]
    fn multi_function_call_queues_and_dispatches_sequentially() {
        let shared = GeminiSharedState::default();
        let json = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {
                            "functionCall": { "name": "fs_read", "args": { "path": "a.rs" } },
                            "thoughtSignature": "sig-0"
                        },
                        {
                            "functionCall": { "name": "fs_read", "args": { "path": "b.rs" } },
                            "thoughtSignature": "sig-1"
                        }
                    ]
                }
            }]
        });

        let first = parse_gemini_response(&json, ConceptId(1), 10, 0, &shared, "test-model", false);
        let first_dbg = format!("{first:?}");
        assert!(first_dbg.contains("a.rs"), "{first_dbg}");

        let mut req2 = dummy_agent_view(1, 11);
        req2.memory = vec![
            ("call".to_string(), r#"fs_read {"path":"a.rs"}"#.to_string()),
            ("observe".to_string(), "ok".to_string()),
        ];
        let second = next_queued_call(&shared.pending_calls, &req2).expect("second call queued");
        let second_dbg = format!("{second:?}");
        assert!(second_dbg.contains("b.rs"), "{second_dbg}");

        let mut req3 = dummy_agent_view(1, 12);
        req3.memory = vec![
            ("call".to_string(), r#"fs_read {"path":"a.rs"}"#.to_string()),
            ("observe".to_string(), "ok".to_string()),
            ("call".to_string(), r#"fs_read {"path":"b.rs"}"#.to_string()),
            ("observe".to_string(), "ok".to_string()),
        ];
        assert!(next_queued_call(&shared.pending_calls, &req3).is_none());
        let sig_guard = shared.thought_signatures.lock().unwrap();
        assert_eq!(sig_guard.get(&(1, 0)), Some(&json!("sig-0")));
        assert_eq!(sig_guard.get(&(1, 1)), Some(&json!("sig-1")));
    }

    #[test]
    fn sanitize_gemini_mcp_schema_normalizes_types_and_strips_metadata() {
        // `inputSchema` MCP realista (draft 2020-12, tipos con `null`, metadatos).
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Echo params",
            "type": "object",
            "properties": {
                "message": {
                    "type": ["string", "null"],
                    "description": "Text to echo back",
                    "default": "hello",
                    "examples": ["a", "b"]
                },
                "count": { "type": "integer", "minimum": 1 },
                "ratio": { "type": "number" },
                "dry_run": { "type": "boolean" },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "additionalProperties": true
                },
                "meta": {
                    "type": "object",
                    "properties": {
                        "score": { "type": ["number", "null"] }
                    }
                }
            },
            "$id": "urn:echo:1",
            "definitions": { "x": { "type": "string" } },
            "$defs": { "y": { "type": "boolean" } }
        });
        sanitize_gemini_mcp_schema(&mut schema);

        // Tipos en MAYÚSCULAS.
        assert_eq!(schema["type"], "OBJECT");
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(props["count"]["type"], "INTEGER");
        assert_eq!(props["ratio"]["type"], "NUMBER");
        assert_eq!(props["dry_run"]["type"], "BOOLEAN");
        assert_eq!(props["tags"]["type"], "ARRAY");
        assert_eq!(props["tags"]["items"]["type"], "STRING");

        // `["string","null"]` → `"STRING"` + `nullable: true` (también anidado).
        assert_eq!(props["message"]["type"], "STRING");
        assert_eq!(props["message"]["nullable"], true);
        assert_eq!(props["meta"]["properties"]["score"]["type"], "NUMBER");
        assert_eq!(props["meta"]["properties"]["score"]["nullable"], true);

        // Metadatos prohibidos eliminados (recursivo y en raíz).
        for key in [
            "$schema",
            "title",
            "$id",
            "definitions",
            "$defs",
            "default",
            "examples",
        ] {
            assert!(schema.get(key).is_none(), "root kept {key}");
        }
        assert!(props["message"].get("default").is_none());
        assert!(props["message"].get("examples").is_none());
        assert!(props["tags"].get("additionalProperties").is_none());

        // Lo legítimo se conserva.
        assert_eq!(props["message"]["description"], "Text to echo back");
        assert_eq!(props["count"]["minimum"], 1);
    }

    #[test]
    fn queued_calls_are_cleared_after_tool_error_or_denial() {
        let shared = GeminiSharedState::default();
        push_queued_calls(
            &shared.pending_calls,
            ConceptId(2),
            [QueuedToolCall {
                name: "fs_write".to_string(),
                args: r#"{"path":"x.rs","content":"hi"}"#.to_string(),
            }],
        );

        let mut denied_req = dummy_agent_view(2, 5);
        denied_req.memory = vec![
            ("call".to_string(), "shell_exec {}".to_string()),
            ("denied".to_string(), "user declined".to_string()),
        ];
        assert!(next_queued_call(&shared.pending_calls, &denied_req).is_none());

        // Even if memory is later clean, the aborted batch must remain empty.
        let clean_req = dummy_agent_view(2, 6);
        assert!(next_queued_call(&shared.pending_calls, &clean_req).is_none());
    }

    #[test]
    fn batch_thought_signature_is_inherited_by_subsequent_parallel_calls() {
        let shared = GeminiSharedState::default();
        let json = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {
                            "functionCall": { "name": "fs_list", "args": {} },
                            "thoughtSignature": "sig-batch"
                        },
                        {
                            "functionCall": { "name": "fs_read", "args": { "path": "Cargo.toml" } }
                        }
                    ]
                }
            }]
        });

        let _ = parse_gemini_response(&json, ConceptId(1), 1, 0, &shared, "test-model", false);
        let sig_guard = shared.thought_signatures.lock().unwrap();
        assert_eq!(sig_guard.get(&(1, 0)), Some(&json!("sig-batch")));
        assert_eq!(sig_guard.get(&(1, 1)), Some(&json!("sig-batch")));
    }

    #[test]
    fn build_request_body_supersedes_prior_reads_after_edit() {
        let mut req = dummy_agent_view(1, 3);
        req.memory = vec![
            (
                "call".to_string(),
                r#"fs_read {"path":"./src/lib.rs"}"#.to_string(),
            ),
            (
                "observe".to_string(),
                "OLD_STALE_CONTENT_SHOULD_BE_SUPERSEDED".to_string(),
            ),
            (
                "call".to_string(),
                r#"fs_edit {"path":"src/lib.rs","old_text":"a","new_text":"b"}"#.to_string(),
            ),
            (
                "observe".to_string(),
                "edited `src/lib.rs` (1 replacement)".to_string(),
            ),
        ];

        let body = build_request_body(&req, &HashMap::new());
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(
            !serialized.contains("OLD_STALE_CONTENT_SHOULD_BE_SUPERSEDED"),
            "{serialized}"
        );
        assert!(serialized.contains("superseded"), "{serialized}");
    }

    #[test]
    fn build_request_body_respects_context_budget_preserving_errors_and_toon() {
        let mut req = dummy_agent_view(1, 5);
        req.memory = vec![
            (
                "call".to_string(),
                r#"fs_read {"path":"huge.rs"}"#.to_string(),
            ),
            ("observe".to_string(), "X".repeat(600)),
            (
                "call".to_string(),
                r#"scent_impact {"target":"foo"}"#.to_string(),
            ),
            (
                "observe".to_string(),
                "@TOON v1\n# cols: subject | predicate | object | meta\nfoo | epicenter | src/a.rs::foo | temp_bp:10000\n# total: 1\n"
                    .to_string(),
            ),
            (
                "call".to_string(),
                r#"shell_exec {"command":"cargo test"}"#.to_string(),
            ),
            ("tool_error".to_string(), "RECENT_ERROR_KEEP".to_string()),
        ];

        let body = build_request_body_with_budget(
            &req,
            &HashMap::new(),
            ContextBudget {
                max_memory_bytes: 350,
            },
        );
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(!serialized.contains(&"X".repeat(200)), "{serialized}");
        assert!(
            serialized.contains("[summary fs_read huge.rs"),
            "{serialized}"
        );
        assert!(serialized.contains("@TOON v1"), "{serialized}");
        assert!(serialized.contains("RECENT_ERROR_KEEP"), "{serialized}");
    }

    #[test]
    fn load_project_instructions_reads_markdown_and_scrubs_secrets() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-inst-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("AGENTS.md"), "Always run cargo test.\n").unwrap();
        fs::write(
            dir.join("ONYX.md"),
            "Secret token AIzaSyTestSecretKey1234567890 must be scrubbed.\n",
        )
        .unwrap();

        let loaded = load_project_instructions(&dir);
        assert!(loaded.contains("## AGENTS.md"), "{loaded}");
        assert!(loaded.contains("Always run cargo test."), "{loaded}");
        assert!(loaded.contains("## ONYX.md"), "{loaded}");
        assert!(
            !loaded.contains("AIzaSyTestSecretKey1234567890"),
            "{loaded}"
        );
        assert!(loaded.contains("[REDACTED]"), "{loaded}");

        let mut req = dummy_agent_view(1, 2);
        req.memory = vec![
            ("say".to_string(), "First answer.".to_string()),
            ("user".to_string(), "Follow-up question.".to_string()),
            (
                "system".to_string(),
                "[/undo] Restored git checkpoint.".to_string(),
            ),
        ];
        let body = build_request_body_with_context(
            &req,
            &HashMap::new(),
            TieredContextConfig::default(),
            &loaded,
            Some(ReasoningEffort::High),
            &[],
            &[],
            DEFAULT_GEMINI_MODEL,
        );
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(serialized.contains("Follow-up question."), "{serialized}");
        assert!(serialized.contains("First answer."), "{serialized}");
        assert!(
            serialized.contains("[/undo] Restored git checkpoint."),
            "{serialized}"
        );
        assert!(serialized.contains("Project Instructions"), "{serialized}");
        assert!(serialized.contains("git_status"), "{serialized}");
        assert!(serialized.contains("Host environment:"), "{serialized}");
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gemini_sse_reader_accumulates_text_deltas_and_tool_calls_updating_preview() {
        let preview = StreamPreviewHandle::new();
        let telemetry = SessionTelemetryHandle::new("gemini-4-hydrogen", false);
        let sse = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"hidden\"},{\"text\":\"Hola \"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"OnYX!\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12000,\"cachedContentTokenCount\":8000,\"candidatesTokenCount\":350,\"thoughtsTokenCount\":120}}\n\n",
            "data: [DONE]\n"
        );
        let shared = GeminiSharedState {
            telemetry: Some(telemetry.clone()),
            ..GeminiSharedState::default()
        };
        let json = accumulate_gemini_sse_reader(
            std::io::BufReader::new(sse.as_bytes()),
            ConceptId(3),
            9,
            Some(&preview),
        )
        .unwrap();
        let snap = preview.get().expect("preview should hold accumulated text");
        assert_eq!(snap.agent, 3);
        assert_eq!(snap.request, 9);
        assert_eq!(snap.text, "Hola OnYX!");
        assert_eq!(snap.thought, "hidden");
        assert!(preview.take_dirty());
        assert!(!preview.take_dirty());

        let reply = parse_gemini_response(&json, ConceptId(3), 9, 0, &shared, "test-model", false);
        assert_eq!(reply, Reply::done(ConceptId(3), 9, "Hola OnYX!"));
        let tel_snap = telemetry.snapshot();
        assert_eq!(tel_snap.last_turn.input_tokens, 12_000);
        assert_eq!(tel_snap.last_turn.cached_input_tokens, 8_000);
        assert_eq!(tel_snap.last_turn.output_tokens, 350);
        assert_eq!(tel_snap.last_turn.reasoning_tokens, 120);
        assert!(tel_snap.format_status_bar().contains("cache 8.0k"));

        // Tool call chunks over SSE also accumulate cleanly.
        let tool_sse = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"fs_read\",\"args\":{\"path\":\"Cargo.toml\"}},\"thoughtSignature\":\"sig-sse\"}]}}]}\n\n",
            "data: [DONE]\n"
        );
        preview.clear();
        let tool_json = accumulate_gemini_sse_reader(
            std::io::BufReader::new(tool_sse.as_bytes()),
            ConceptId(3),
            10,
            Some(&preview),
        )
        .unwrap();
        assert_eq!(preview.get().unwrap().text, "[calling `fs_read`...]");
        let tool_reply = parse_gemini_response(
            &tool_json,
            ConceptId(3),
            10,
            0,
            &shared,
            "test-model",
            false,
        );
        assert!(
            matches!(tool_reply.verb, onyx_soul_agents::Verb::Call),
            "{tool_reply:?}"
        );
        assert!(tool_reply.payload.contains("fs_read"), "{tool_reply:?}");
        assert_eq!(
            shared.thought_signatures.lock().unwrap().get(&(3, 0)),
            Some(&json!("sig-sse"))
        );
    }

    #[test]
    fn build_request_body_merges_consecutive_same_roles_and_selects_thinking_config() {
        let mut req = dummy_agent_view(1, 2);
        req.memory = vec![
            ("say".to_string(), "Ans".to_string()),
            ("user".to_string(), "Follow-up".to_string()),
            ("system".to_string(), "[/undo]".to_string()),
        ];
        let body_25 = build_request_body_with_context(
            &req,
            &HashMap::new(),
            TieredContextConfig::default(),
            "",
            Some(ReasoningEffort::Medium),
            &[],
            &[],
            "gemini-2.5-pro",
        );
        let contents = body_25["contents"].as_array().unwrap();
        let roles: Vec<&str> = contents
            .iter()
            .map(|c| c["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["user", "model", "user"]);
        assert_eq!(contents[2]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(
            body_25["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            8192
        );
        assert_eq!(
            body_25["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
        assert!(
            body_25["generationConfig"]["thinkingConfig"]
                .get("thinkingLevel")
                .is_none()
        );

        let body_3pro = build_request_body_with_context(
            &req,
            &HashMap::new(),
            TieredContextConfig::default(),
            "",
            Some(ReasoningEffort::Medium),
            &[],
            &[],
            "gemini-3-pro-preview",
        );
        assert_eq!(
            body_3pro["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
        assert_eq!(
            body_3pro["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );

        let body_20_thinking = build_request_body_with_context(
            &req,
            &HashMap::new(),
            TieredContextConfig::default(),
            "",
            Some(ReasoningEffort::High),
            &[],
            &[],
            "gemini-2.0-flash-thinking-exp",
        );
        assert!(
            body_20_thinking["generationConfig"]["thinkingConfig"]
                .get("thinkingLevel")
                .is_none()
        );
        assert_eq!(
            body_20_thinking["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );

        let body_20_plain = build_request_body_with_context(
            &req,
            &HashMap::new(),
            TieredContextConfig::default(),
            "",
            Some(ReasoningEffort::High),
            &[],
            &[],
            "gemini-2.0-flash",
        );
        assert!(body_20_plain.get("generationConfig").is_none());
    }

    #[test]
    fn gemini_sse_reader_propagates_errors_prompt_feedback_and_fix03_truncation() {
        let err_sse = "data: {\"error\":{\"code\":400,\"message\":\"Invalid arg\"}}\n\n";
        let err_json = accumulate_gemini_sse_reader(
            std::io::BufReader::new(err_sse.as_bytes()),
            ConceptId(1),
            1,
            None,
        )
        .unwrap();
        let shared = GeminiSharedState::default();
        let reply = parse_gemini_response(&err_json, ConceptId(1), 1, 0, &shared, "m", false);
        assert_eq!(reply.verb, onyx_soul_agents::Verb::Fail);
        assert!(reply.payload.contains("400"), "{reply:?}");
        assert!(reply.payload.contains("Invalid arg"), "{reply:?}");

        let retryable_sse =
            "data: {\"error\":{\"code\":503,\"status\":\"UNAVAILABLE\",\"message\":\"busy\"}}\n\n";
        let retry_err = accumulate_gemini_sse_reader(
            std::io::BufReader::new(retryable_sse.as_bytes()),
            ConceptId(1),
            1,
            None,
        )
        .unwrap_err();
        assert!(is_retryable_gemini_stream_error(&retry_err), "{retry_err}");

        let blocked_sse = "data: {\"promptFeedback\":{\"blockReason\":\"SAFETY\"}}\n\n";
        let blocked_json = accumulate_gemini_sse_reader(
            std::io::BufReader::new(blocked_sse.as_bytes()),
            ConceptId(1),
            1,
            None,
        )
        .unwrap();
        let blocked_reply =
            parse_gemini_response(&blocked_json, ConceptId(1), 1, 0, &shared, "m", false);
        assert!(
            blocked_reply.payload.contains("SAFETY"),
            "{blocked_reply:?}"
        );

        // FIX-03: truncated SSE without finishReason or [DONE] must error (not fake STOP).
        let truncated_sse =
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"half\"}]}}]}\n\n";
        let trunc_err = accumulate_gemini_sse_reader(
            std::io::BufReader::new(truncated_sse.as_bytes()),
            ConceptId(1),
            1,
            None,
        )
        .unwrap_err();
        assert!(trunc_err.contains("truncated SSE stream"), "{trunc_err}");
        assert!(is_retryable_gemini_stream_error(&trunc_err));

        // ThoughtSignature on non-call part preceding functionCall is inherited.
        let sig_sse = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"\",\"thoughtSignature\":\"sig-prev\"},{\"functionCall\":{\"name\":\"git_status\"}}]},\"finishReason\":\"STOP\"}]}\n\n"
        );
        let sig_json = accumulate_gemini_sse_reader(
            std::io::BufReader::new(sig_sse.as_bytes()),
            ConceptId(1),
            2,
            None,
        )
        .unwrap();
        let call_reply = parse_gemini_response(&sig_json, ConceptId(1), 2, 0, &shared, "m", false);
        assert_eq!(call_reply.verb, onyx_soul_agents::Verb::Call);
        assert!(call_reply.payload.ends_with("{}"), "{call_reply:?}");
        assert_eq!(
            shared.thought_signatures.lock().unwrap().get(&(1, 0)),
            Some(&json!("sig-prev"))
        );
    }

    #[test]
    fn sanitize_mcp_schema_handles_unions_null_const_and_strip_keys() {
        let mut schema = json!({
            "type": "object",
            "additionalProperties": false,
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "properties": {
                "id": { "type": ["string", "integer", "null"] },
                "mode": { "const": "fast" },
                "only_null": { "type": ["null"] },
                "count": { "type": "integer", "exclusiveMinimum": 0, "$ref": "#/$defs/n" }
            }
        });
        sanitize_gemini_mcp_schema(&mut schema);
        assert_eq!(schema["type"], "OBJECT");
        assert!(schema.get("additionalProperties").is_none());
        assert!(schema.get("$schema").is_none());

        let id = &schema["properties"]["id"];
        assert!(id.get("type").is_none());
        assert_eq!(id["nullable"], true);
        assert_eq!(
            id["anyOf"],
            json!([{ "type": "STRING" }, { "type": "INTEGER" }])
        );

        let mode = &schema["properties"]["mode"];
        assert_eq!(mode["type"], "STRING");
        assert_eq!(mode["enum"], json!(["fast"]));
        assert!(mode.get("const").is_none());

        let only_null = &schema["properties"]["only_null"];
        assert!(only_null.get("type").is_none());
        assert_eq!(only_null["nullable"], true);

        let count = &schema["properties"]["count"];
        assert_eq!(count["type"], "INTEGER");
        assert!(count.get("exclusiveMinimum").is_none());
        assert!(count.get("$ref").is_none());
    }

    #[test]
    fn next_queued_call_requires_observe_and_clears_on_user_or_system_and_prunes_signatures() {
        let shared = GeminiSharedState::default();
        push_queued_calls(
            &shared.pending_calls,
            ConceptId(4),
            [QueuedToolCall {
                name: "fs_read".to_string(),
                args: "{}".to_string(),
            }],
        );

        let mut interrupted = dummy_agent_view(4, 1);
        interrupted.memory = vec![("user".to_string(), "stop and do X".to_string())];
        assert!(next_queued_call(&shared.pending_calls, &interrupted).is_none());
        assert!(shared.pending_calls.lock().unwrap().get(&4).is_none());

        {
            let mut sigs = shared.thought_signatures.lock().unwrap();
            sigs.insert((4, 0), json!("sig-0"));
            sigs.insert((4, 1), json!("sig-1"));
            sigs.insert((4, 2), json!("sig-2"));
            sigs.insert((9, 2), json!("other-agent"));
        }
        shared.prune_thought_signatures_from(4, 1);
        let sigs = shared.thought_signatures.lock().unwrap();
        assert_eq!(sigs.get(&(4, 0)), Some(&json!("sig-0")));
        assert!(sigs.get(&(4, 1)).is_none());
        assert!(sigs.get(&(4, 2)).is_none());
        assert_eq!(sigs.get(&(9, 2)), Some(&json!("other-agent")));
    }

    #[test]
    fn stream_preview_clear_for_only_clears_matching_request() {
        let preview = StreamPreviewHandle::new();
        preview.set(5, 10, "active stream");
        preview.clear_for(5, 9);
        assert_eq!(preview.get().unwrap().text, "active stream");
        preview.clear_for(6, 10);
        assert_eq!(preview.get().unwrap().text, "active stream");
        preview.clear_for(5, 10);
        assert!(preview.get().is_none());
    }

    #[test]
    fn sanitize_mcp_schema_preserves_parameter_names_matching_json_schema_keywords() {
        let mut schema = json!({
            "type": "object",
            "title": "OuterSchemaTitle",
            "additionalProperties": false,
            "properties": {
                "title": { "type": "string", "title": "InnerTitle", "default": "fallback" },
                "default": { "type": "boolean" },
                "const": { "type": "string" },
                "readOnly": { "type": "boolean" },
                "deprecated": { "type": "boolean" },
                "if": { "type": "string" },
                "not": { "type": "string" },
                "nested": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "title": { "type": "string", "default": "n" },
                        "default": { "type": "string" }
                    }
                }
            }
        });
        sanitize_gemini_mcp_schema(&mut schema);

        assert_eq!(schema["type"], "OBJECT");
        assert!(schema.get("title").is_none());
        assert!(schema.get("additionalProperties").is_none());

        let props = schema["properties"].as_object().unwrap();
        for key in [
            "title",
            "default",
            "const",
            "readOnly",
            "deprecated",
            "if",
            "not",
            "nested",
        ] {
            assert!(
                props.contains_key(key),
                "missing MCP param `{key}`: {props:?}"
            );
        }
        assert_eq!(props["title"]["type"], "STRING");
        assert!(props["title"].get("title").is_none());
        assert!(props["title"].get("default").is_none());
        assert_eq!(props["default"]["type"], "BOOLEAN");

        let nested = &props["nested"];
        assert_eq!(nested["type"], "OBJECT");
        assert!(nested.get("additionalProperties").is_none());
        let nested_props = nested["properties"].as_object().unwrap();
        assert_eq!(nested_props["title"]["type"], "STRING");
        assert!(nested_props["title"].get("default").is_none());
        assert_eq!(nested_props["default"]["type"], "STRING");
    }

    #[test]
    fn gemini_retry_delay_respects_backoff_floor_and_caps_retry_after() {
        assert_eq!(gemini_retry_delay_ms(0, None), Some(RETRY_BACKOFF_MS[0]));
        assert_eq!(gemini_retry_delay_ms(0, Some(0)), Some(RETRY_BACKOFF_MS[0]));
        assert_eq!(gemini_retry_delay_ms(1, Some(2_000)), Some(2_000));
        assert_eq!(
            gemini_retry_delay_ms(0, Some(60_000)),
            Some(MAX_RETRY_AFTER_MS)
        );
        assert_eq!(
            gemini_retry_delay_ms(MAX_TRANSIENT_RETRIES, Some(1_000)),
            None
        );
    }
}
