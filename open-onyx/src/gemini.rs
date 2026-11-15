use std::collections::HashMap;

use onyx_soul_agents::{AgentView, Reply, Responder};
use onyx_workspace::WorkspaceTools;
use serde_json::{Value as JsonValue, json};

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-4-hydrogen";

/// Adaptador síncrono para Google AI Studio (Gemini `generateContent` REST API).
pub struct GeminiResponder {
    api_key: String,
    model: String,
    thought_signatures: HashMap<(u64, usize), JsonValue>,
}

impl GeminiResponder {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            thought_signatures: HashMap::new(),
        }
    }

    fn build_request_body(&self, req: &AgentView) -> JsonValue {
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
                    if let Some(sig) = self.thought_signatures.get(&(req.agent.0, call_index)) {
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
}

impl Responder for GeminiResponder {
    fn respond(&mut self, req: &AgentView) -> Option<Reply> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );
        let body = self.build_request_body(req);
        let resp = match ureq::post(&url).send_json(body) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let err_body = r.into_string().unwrap_or_default();
                return Some(Reply::fail(
                    req.agent,
                    req.request,
                    format!("gemini HTTP {code}: {err_body}"),
                ));
            }
            Err(e) => {
                return Some(Reply::fail(
                    req.agent,
                    req.request,
                    format!("gemini error: {e}"),
                ));
            }
        };
        let json: JsonValue = match resp.into_json() {
            Ok(v) => v,
            Err(e) => {
                return Some(Reply::fail(
                    req.agent,
                    req.request,
                    format!("gemini json error: {e}"),
                ));
            }
        };

        let parts = json["candidates"][0]["content"]["parts"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let call_index = req.memory.iter().filter(|(k, _)| k == "call").count();
        for part in &parts {
            if let Some(fc) = part.get("functionCall") {
                if let Some(sig) = part.get("thoughtSignature") {
                    self.thought_signatures
                        .insert((req.agent.0, call_index), sig.clone());
                }
                let name = fc["name"].as_str().unwrap_or_default();
                let args = serde_json::to_string(&fc["args"]).unwrap_or_else(|_| "{}".to_string());
                return Some(Reply::call_structured(req.agent, req.request, name, args));
            }
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
            Some(Reply::fail(
                req.agent,
                req.request,
                format!("empty gemini response (finishReason={reason}): {json}"),
            ))
        } else {
            Some(Reply::done(req.agent, req.request, text))
        }
    }
}
