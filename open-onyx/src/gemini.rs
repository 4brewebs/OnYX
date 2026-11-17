use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use onyx_soul_agents::{
    AgentView, ConceptId, EventBus, GraphView, PlatformDriver, Reply, Responder, SurfaceBinding,
    Vocab, pending_requests,
};
use onyx_workspace::WorkspaceTools;
use serde_json::{Value as JsonValue, json};

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-4-hydrogen";
pub const DEFAULT_CONTEXT_BUDGET_BYTES: usize = 96 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub max_memory_bytes: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_CONTEXT_BUDGET_BYTES,
        }
    }
}

type SignatureMap = Arc<Mutex<HashMap<(u64, usize), JsonValue>>>;
type PendingCallQueue = Arc<Mutex<HashMap<u64, VecDeque<(String, String)>>>>;

#[derive(Clone, Default)]
struct GeminiSharedState {
    thought_signatures: SignatureMap,
    pending_calls: PendingCallQueue,
}

struct QueuedToolCall {
    name: String,
    args: String,
}

fn next_queued_call(pending_calls: &PendingCallQueue, req: &AgentView) -> Option<Reply> {
    let mut guard = pending_calls.lock().ok()?;
    if matches!(
        req.memory.last().map(|(k, _)| k.as_str()),
        Some("tool_error" | "denied")
    ) {
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

fn push_queued_calls(
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

struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

fn normalize_tool_path(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("./")
        .trim_start_matches(".\\")
        .replace('\\', "/")
}

fn extract_call_path(args_json: &str) -> Option<String> {
    let parsed: JsonValue = serde_json::from_str(args_json).ok()?;
    parsed
        .get("path")
        .and_then(|v| v.as_str())
        .map(normalize_tool_path)
}

fn find_superseded_reads(memory: &[(String, String)]) -> HashMap<usize, String> {
    let mut read_observe_indices: HashMap<String, Vec<usize>> = HashMap::new();
    let mut superseded: HashMap<usize, String> = HashMap::new();
    let mut last_call: Option<(String, Option<String>)> = None;

    for (idx, (kind, text)) in memory.iter().enumerate() {
        match kind.as_str() {
            "call" => {
                let (tool, args) = match text.split_once(' ') {
                    Some((t, a)) => (t, a),
                    None => (text.as_str(), "{}"),
                };
                last_call = Some((tool.to_string(), extract_call_path(args)));
            }
            "observe" => {
                if let Some((tool, Some(path))) = last_call.take() {
                    if tool == "fs_read" {
                        read_observe_indices.entry(path).or_default().push(idx);
                    } else if matches!(tool.as_str(), "fs_write" | "fs_edit") {
                        if let Some(indices) = read_observe_indices.remove(&path) {
                            for old_idx in indices {
                                superseded.insert(old_idx, path.clone());
                            }
                        }
                    }
                }
            }
            "tool_error" | "denied" => {
                last_call = None;
            }
            _ => {}
        }
    }
    superseded
}

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
    struct ProjectedEntry {
        msg: JsonValue,
        tool_name: String,
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
            "parts": [{ "text": goal_text }]
        }),
        tool_name: String::new(),
        is_observe: false,
        is_toon: false,
        is_superseded: false,
    });

    let mut last_tool: Option<String> = None;
    let mut call_index = 0usize;
    for (mem_idx, (kind, text)) in req.memory.iter().enumerate() {
        match kind.as_str() {
            "say" => entries.push(ProjectedEntry {
                bytes: text.len(),
                msg: json!({
                    "role": "model",
                    "parts": [{ "text": text }]
                }),
                tool_name: String::new(),
                is_observe: false,
                is_toon: false,
                is_superseded: false,
            }),
            "call" => {
                let (tool, args) = match text.split_once(' ') {
                    Some((t, a)) => (t, a),
                    None => (text.as_str(), "{}"),
                };
                last_tool = Some(tool.to_string());
                let parsed_args: JsonValue =
                    serde_json::from_str(args).unwrap_or_else(|_| json!({ "raw": args }));
                let mut part = json!({
                    "functionCall": { "name": tool, "args": parsed_args }
                });
                if let Some(sig) = thought_signatures.get(&(req.agent.0, call_index)) {
                    part["thoughtSignature"] = sig.clone();
                }
                call_index += 1;
                entries.push(ProjectedEntry {
                    bytes: tool.len() + args.len(),
                    msg: json!({
                        "role": "model",
                        "parts": [part]
                    }),
                    tool_name: tool.to_string(),
                    is_observe: false,
                    is_toon: false,
                    is_superseded: false,
                });
            }
            "observe" => {
                let name = last_tool.take().unwrap_or_else(|| "tool".to_string());
                if let Some(sup_path) = superseded.get(&mem_idx) {
                    let marker = format!(
                        "[superseded: prior fs_read of `{sup_path}` was invalidated by a subsequent edit]"
                    );
                    entries.push(ProjectedEntry {
                        bytes: marker.len(),
                        msg: json!({
                            "role": "user",
                            "parts": [{
                                "functionResponse": {
                                    "name": name,
                                    "response": { "result": marker }
                                }
                            }]
                        }),
                        tool_name: name,
                        is_observe: true,
                        is_toon: false,
                        is_superseded: true,
                    });
                } else {
                    let is_toon = text.trim_start().starts_with("@TOON v1");
                    entries.push(ProjectedEntry {
                        bytes: text.len(),
                        msg: json!({
                            "role": "user",
                            "parts": [{
                                "functionResponse": {
                                    "name": name,
                                    "response": { "result": text }
                                }
                            }]
                        }),
                        tool_name: name,
                        is_observe: true,
                        is_toon,
                        is_superseded: false,
                    });
                }
            }
            "tool_error" => {
                let name = last_tool.take().unwrap_or_else(|| "tool".to_string());
                entries.push(ProjectedEntry {
                    bytes: text.len(),
                    msg: json!({
                        "role": "user",
                        "parts": [{
                            "functionResponse": {
                                "name": name,
                                "response": { "error": text }
                            }
                        }]
                    }),
                    tool_name: name,
                    is_observe: false,
                    is_toon: false,
                    is_superseded: false,
                });
            }
            "denied" => {
                let name = last_tool.take().unwrap_or_else(|| "tool".to_string());
                let msg_text = format!("Tool call denied: {text}. Adjust your plan.");
                entries.push(ProjectedEntry {
                    bytes: msg_text.len(),
                    msg: json!({
                        "role": "user",
                        "parts": [{
                            "functionResponse": {
                                "name": name,
                                "response": { "error": msg_text }
                            }
                        }]
                    }),
                    tool_name: name,
                    is_observe: false,
                    is_toon: false,
                    is_superseded: false,
                });
            }
            _ => {}
        }
    }

    let mut total_bytes: usize = entries.iter().map(|e| e.bytes).sum();
    let keep_tail_from = entries.len().saturating_sub(4);
    if total_bytes > budget.max_memory_bytes {
        // Pase 1 del presupuesto: compactar observaciones no-TOON antiguas antes de tocar TOON.
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
                        "role": "user",
                        "parts": [{
                            "functionResponse": {
                                "name": entry.tool_name,
                                "response": { "result": marker }
                            }
                        }]
                    });
                }
            }
        }
    }

    let contents: Vec<JsonValue> = entries.into_iter().map(|e| e.msg).collect();
    let declarations: Vec<JsonValue> = WorkspaceTools::schemas_with_subagents(true)
        .into_iter()
        .filter(|s| req.tools.iter().any(|t| t == s.name))
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "parameters": s.parameters
            })
        })
        .collect();

    let mut body = json!({
        "systemInstruction": {
            "parts": [{
                "text": "You are openOnyx, a precise software engineering agent. When mapping architecture or inspecting unfamiliar crates, call `scent_outline` first to get compact @TOON v1 signatures and line ranges, call `scent_impact` before editing shared symbols, and use `fs_read` with bounded `start_line`/`end_line` ranges rather than reading whole files unnecessarily. Never use `shell_exec` to read, list, or search files (such as `nl`, `cat`, `grep`, `find`); always use `fs_read`, `fs_list`, `code_search`, `scent_outline`, and `scent_impact`, reserving `shell_exec` strictly for builds, tests, or git/project commands. Verify exact dependency and enum names with `fs_read` before citing them. You can delegate bounded subtasks with `spawn_agent` when useful. Read files before editing them, prefer calling one tool at a time, keep changes minimal, and summarize your final answer clearly when done."
            }]
        },
        "contents": contents
    });

    if !declarations.is_empty() {
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    }
    body
}

fn parse_gemini_response(
    json: &JsonValue,
    agent: ConceptId,
    request: u64,
    call_index: usize,
    shared: &GeminiSharedState,
) -> Reply {
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
        for (offset, (part, fc)) in function_calls.into_iter().enumerate() {
            if let Some(sig) = part.get("thoughtSignature") {
                batch_sig = Some(sig.clone());
            }
            if let Some(ref sig) = batch_sig {
                if let Ok(mut guard) = shared.thought_signatures.lock() {
                    guard.insert((agent.0, call_index + offset), sig.clone());
                }
            }
            let name = fc["name"].as_str().unwrap_or_default().to_string();
            let args = serde_json::to_string(&fc["args"]).unwrap_or_else(|_| "{}".to_string());
            parsed.push(QueuedToolCall { name, args });
        }

        let mut iter = parsed.into_iter();
        let first = iter.next().expect("non-empty function_calls");
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

fn execute_gemini_http(
    url: &str,
    api_key: &str,
    body: JsonValue,
    agent: ConceptId,
    request: u64,
    call_index: usize,
    shared: &GeminiSharedState,
) -> Reply {
    let resp = match ureq::post(url)
        .set("x-goog-api-key", api_key)
        .send_json(body)
    {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let err_body = r.into_string().unwrap_or_default();
            return Reply::fail(agent, request, format!("gemini HTTP {code}: {err_body}"));
        }
        Err(ureq::Error::Transport(t)) => {
            return Reply::fail(
                agent,
                request,
                format!(
                    "gemini transport error ({:?}): {}",
                    t.kind(),
                    t.message().unwrap_or("request failed")
                ),
            );
        }
    };
    let json: JsonValue = match resp.into_json() {
        Ok(v) => v,
        Err(e) => {
            return Reply::fail(agent, request, format!("gemini json error: {e}"));
        }
    };

    parse_gemini_response(&json, agent, request, call_index, shared)
}

/// Adaptador síncrono para Google AI Studio (Gemini `generateContent` REST API).
pub struct GeminiResponder {
    api_key: String,
    model: String,
    shared: GeminiSharedState,
}

impl GeminiResponder {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            shared: GeminiSharedState::default(),
        }
    }
}

impl Responder for GeminiResponder {
    fn respond(&mut self, req: &AgentView) -> Option<Reply> {
        if let Some(queued_reply) = next_queued_call(&self.shared.pending_calls, req) {
            return Some(queued_reply);
        }
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );
        let body = {
            let sigs = self
                .shared
                .thought_signatures
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            build_request_body(req, &sigs)
        };
        let call_index = req.memory.iter().filter(|(k, _)| k == "call").count();
        Some(execute_gemini_http(
            &url,
            &self.api_key,
            body,
            req.agent,
            req.request,
            call_index,
            &self.shared,
        ))
    }
}

/// Driver asíncrono no bloqueante para la TUI de `Mode::Chat`.
///
/// Despacha peticiones HTTP en `present()` y entrega los `Reply` recibidos en `poll()`,
/// de modo que `RecordingBridge` pueda grabarlos vía `RecordingBus` sin violar el
/// contrato de disparo único de `Responder::respond`.
pub struct AsyncGeminiDriver {
    vocab: Vocab,
    api_key: String,
    model: String,
    shared: GeminiSharedState,
    dispatched: HashSet<(u64, u64)>,
    in_flight_count: Arc<AtomicUsize>,
    has_ready: Arc<AtomicBool>,
    rx: mpsc::Receiver<Reply>,
    tx: mpsc::Sender<Reply>,
    inner: Option<Box<dyn PlatformDriver>>,
}

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
            shared: GeminiSharedState::default(),
            dispatched: HashSet::new(),
            in_flight_count,
            has_ready,
            rx,
            tx,
            inner: None,
        }
    }

    pub fn wrapping(mut self, inner: impl PlatformDriver + 'static) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

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
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );
        let body = {
            let sigs = self
                .shared
                .thought_signatures
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            build_request_body(req, &sigs)
        };
        let call_index = req.memory.iter().filter(|(k, _)| k == "call").count();

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

impl PlatformDriver for AsyncGeminiDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Some(inner) = self.inner.as_mut() {
            inner.poll(bus);
        }
        self.has_ready.store(false, Ordering::Release);
        while let Ok(reply) = self.rx.try_recv() {
            bus.push(reply.to_event());
        }
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
        for req in pending_requests(graph, &self.vocab) {
            self.dispatch_with(
                &req,
                |url, api_key, body, agent, request, call_index, shared, guard, ready_flag, tx| {
                    thread::spawn(move || {
                        let _guard = guard;
                        let reply = execute_gemini_http(
                            &url, &api_key, body, agent, request, call_index, &shared,
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

        let first = parse_gemini_response(&json, ConceptId(1), 10, 0, &shared);
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

        let _ = parse_gemini_response(&json, ConceptId(1), 1, 0, &shared);
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
            serialized.contains("trimmed by ContextBudget"),
            "{serialized}"
        );
        assert!(serialized.contains("@TOON v1"), "{serialized}");
        assert!(serialized.contains("RECENT_ERROR_KEEP"), "{serialized}");
    }
}
