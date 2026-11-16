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

fn build_request_body(
    req: &AgentView,
    thought_signatures: &HashMap<(u64, usize), JsonValue>,
) -> JsonValue {
    let mut contents = Vec::new();
    contents.push(json!({
        "role": "user",
        "parts": [{ "text": format!("Goal: {}", req.goal) }]
    }));

    let mut last_tool: Option<String> = None;
    let mut call_index = 0usize;
    for (kind, text) in &req.memory {
        match kind.as_str() {
            "say" => contents.push(json!({
                "role": "model",
                "parts": [{ "text": text }]
            })),
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
                contents.push(json!({
                    "role": "model",
                    "parts": [part]
                }));
            }
            "observe" => {
                let name = last_tool.take().unwrap_or_else(|| "tool".to_string());
                contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": name,
                            "response": { "result": text }
                        }
                    }]
                }));
            }
            "tool_error" => {
                let name = last_tool.take().unwrap_or_else(|| "tool".to_string());
                contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": name,
                            "response": { "error": text }
                        }
                    }]
                }));
            }
            "denied" => {
                let name = last_tool.take().unwrap_or_else(|| "tool".to_string());
                contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": name,
                            "response": { "error": format!("Tool call denied: {text}. Adjust your plan.") }
                        }
                    }]
                }));
            }
            _ => {}
        }
    }

    let declarations: Vec<JsonValue> = WorkspaceTools::schemas()
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
                "text": "You are openOnyx, a precise software engineering agent. When mapping architecture or inspecting unfamiliar crates, call `scent_outline` first to get compact @TOON v1 signatures and line ranges, call `scent_impact` before editing shared symbols, and use `fs_read` with bounded `start_line`/`end_line` ranges rather than reading whole files unnecessarily. Read files before editing them, prefer calling one tool at a time, keep changes minimal, and summarize your final answer clearly when done."
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
}
