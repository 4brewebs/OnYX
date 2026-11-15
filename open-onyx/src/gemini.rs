use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use onyx_soul_agents::{AgentView, ConceptId, Reply, Responder};
use onyx_workspace::WorkspaceTools;
use serde_json::{Value as JsonValue, json};

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-4-hydrogen";

type SignatureMap = Arc<Mutex<HashMap<(u64, usize), JsonValue>>>;

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
                "text": "You are openOnyx, a precise software engineering agent. Use the workspace tools to inspect files before editing, keep changes minimal, and summarize your final answer clearly when done."
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
    thought_signatures: &SignatureMap,
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

    if function_calls.len() > 1 {
        return Reply::fail(
            agent,
            request,
            format!(
                "gemini returned {} simultaneous functionCall parts; only single tool calls are supported",
                function_calls.len()
            ),
        );
    }

    if let Some((part, fc)) = function_calls.into_iter().next() {
        if let Some(sig) = part.get("thoughtSignature") {
            if let Ok(mut guard) = thought_signatures.lock() {
                guard.insert((agent.0, call_index), sig.clone());
            }
        }
        let name = fc["name"].as_str().unwrap_or_default();
        let args = serde_json::to_string(&fc["args"]).unwrap_or_else(|_| "{}".to_string());
        return Reply::call_structured(agent, request, name, args);
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
    thought_signatures: &SignatureMap,
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

    parse_gemini_response(&json, agent, request, call_index, thought_signatures)
}

/// Adaptador síncrono para Google AI Studio (Gemini `generateContent` REST API).
pub struct GeminiResponder {
    api_key: String,
    model: String,
    thought_signatures: SignatureMap,
}

impl GeminiResponder {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            thought_signatures: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Responder for GeminiResponder {
    fn respond(&mut self, req: &AgentView) -> Option<Reply> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );
        let body = {
            let sigs = self
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
            &self.thought_signatures,
        ))
    }
}

/// Adaptador asíncrono no bloqueante para la TUI de `Mode::Chat`.
pub struct AsyncGeminiResponder {
    api_key: String,
    model: String,
    thought_signatures: SignatureMap,
    in_flight: HashSet<(u64, u64)>,
    in_flight_count: Arc<AtomicUsize>,
    rx: mpsc::Receiver<((u64, u64), Reply)>,
    tx: mpsc::Sender<((u64, u64), Reply)>,
    ready: HashMap<(u64, u64), Reply>,
}

impl AsyncGeminiResponder {
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        in_flight_count: Arc<AtomicUsize>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            api_key: api_key.into(),
            model: model.into(),
            thought_signatures: Arc::new(Mutex::new(HashMap::new())),
            in_flight: HashSet::new(),
            in_flight_count,
            rx,
            tx,
            ready: HashMap::new(),
        }
    }

    fn respond_with<F>(&mut self, req: &AgentView, spawn_fn: F) -> Option<Reply>
    where
        F: FnOnce(
            (u64, u64),
            String,
            String,
            JsonValue,
            ConceptId,
            u64,
            usize,
            SignatureMap,
            InFlightGuard,
            mpsc::Sender<((u64, u64), Reply)>,
        ),
    {
        while let Ok((k, r)) = self.rx.try_recv() {
            self.in_flight.remove(&k);
            self.ready.insert(k, r);
        }

        let key = (req.agent.0, req.request);
        if let Some(reply) = self.ready.remove(&key) {
            return Some(reply);
        }
        if self.in_flight.contains(&key) {
            return None;
        }

        self.in_flight.insert(key);
        self.in_flight_count.fetch_add(1, Ordering::Release);
        let guard = InFlightGuard(Arc::clone(&self.in_flight_count));
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );
        let body = {
            let sigs = self
                .thought_signatures
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            build_request_body(req, &sigs)
        };
        let call_index = req.memory.iter().filter(|(k, _)| k == "call").count();

        spawn_fn(
            key,
            url,
            self.api_key.clone(),
            body,
            req.agent,
            req.request,
            call_index,
            Arc::clone(&self.thought_signatures),
            guard,
            self.tx.clone(),
        );
        None
    }
}

impl Responder for AsyncGeminiResponder {
    fn respond(&mut self, req: &AgentView) -> Option<Reply> {
        self.respond_with(
            req,
            |key, url, api_key, body, agent, request, call_index, sigs, guard, tx| {
                thread::spawn(move || {
                    let _guard = guard;
                    let reply = execute_gemini_http(
                        &url, &api_key, body, agent, request, call_index, &sigs,
                    );
                    let _ = tx.send((key, reply));
                });
            },
        )
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
            goal: "check async".to_string(),
            tools: vec![],
            memory: vec![],
            pending_approval: None,
            props: vec![],
        }
    }

    #[test]
    fn async_responder_defers_and_delivers_via_channel() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let mut responder =
            AsyncGeminiResponder::new("fake-key", DEFAULT_GEMINI_MODEL, Arc::clone(&in_flight));
        let req = dummy_agent_view(7, 42);

        let mut captured_tx = None;
        let mut captured_guard = None;
        let first = responder.respond_with(
            &req,
            |_key, _url, _key_str, _body, _agent, _request, _idx, _sigs, guard, tx| {
                captured_guard = Some(guard);
                captured_tx = Some(tx);
            },
        );
        assert!(first.is_none());
        assert_eq!(in_flight.load(Ordering::Acquire), 1);

        // While worker is still running, a repeated poll returns None and does not spawn again.
        let mut spawned_again = false;
        let second = responder.respond_with(&req, |_, _, _, _, _, _, _, _, _, _| {
            spawned_again = true;
        });
        assert!(second.is_none());
        assert!(!spawned_again);
        assert_eq!(in_flight.load(Ordering::Acquire), 1);

        // Simulate worker finishing: send reply then drop guard.
        captured_tx
            .unwrap()
            .send(((7, 42), Reply::done(ConceptId(7), 42, "all good")))
            .unwrap();
        drop(captured_guard);
        assert_eq!(in_flight.load(Ordering::Acquire), 0);

        let third = responder.respond_with(&req, |_, _, _, _, _, _, _, _, _, _| {
            panic!("should not spawn when reply is ready");
        });
        assert!(third.is_some());
        assert_eq!(in_flight.load(Ordering::Acquire), 0);
    }

    #[test]
    fn multi_function_call_is_rejected_explicitly() {
        let sigs = Arc::new(Mutex::new(HashMap::new()));
        let json = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "functionCall": { "name": "fs_read", "args": { "path": "a.rs" } } },
                        { "functionCall": { "name": "fs_read", "args": { "path": "b.rs" } } }
                    ]
                }
            }]
        });
        let reply = parse_gemini_response(&json, ConceptId(1), 1, 0, &sigs);
        let debug = format!("{reply:?}");
        assert!(debug.contains("simultaneous functionCall"), "{debug}");
    }
}
