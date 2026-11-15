use onyx_soul_agents::{AgentView, Reply, Responder};
use onyx_workspace::WorkspaceTools;
use serde_json::{Value as JsonValue, json};

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";

/// Adaptador síncrono para Google AI Studio (Gemini `generateContent` REST API).
pub struct GeminiResponder {
    api_key: String,
    model: String,
}

impl GeminiResponder {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn build_request_body(&self, req: &AgentView) -> JsonValue {
        let mut contents = Vec::new();
        contents.push(json!({
            "role": "user",
            "parts": [{ "text": format!("Goal: {}", req.goal) }]
        }));

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
                    let parsed_args: JsonValue =
                        serde_json::from_str(args).unwrap_or_else(|_| json!({ "raw": args }));
                    contents.push(json!({
                        "role": "model",
                        "parts": [{ "functionCall": { "name": tool, "args": parsed_args } }]
                    }));
                }
                "observe" => contents.push(json!({
                    "role": "user",
                    "parts": [{ "text": format!("Tool result:\n{text}") }]
                })),
                "tool_error" => contents.push(json!({
                    "role": "user",
                    "parts": [{ "text": format!("Tool error:\n{text}") }]
                })),
                "denied" => contents.push(json!({
                    "role": "user",
                    "parts": [{ "text": format!("Tool call denied: {text}. Adjust your plan.") }]
                })),
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
            Err(e) => return Some(Reply::fail(req.agent, req.request, format!("gemini error: {e}"))),
        };
        let json: JsonValue = match resp.into_json() {
            Ok(v) => v,
            Err(e) => return Some(Reply::fail(req.agent, req.request, format!("gemini json error: {e}"))),
        };

        let parts = json["candidates"][0]["content"]["parts"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        for part in &parts {
            if let Some(fc) = part.get("functionCall") {
                let name = fc["name"].as_str().unwrap_or_default();
                let args = serde_json::to_string(&fc["args"]).unwrap_or_else(|_| "{}".to_string());
                return Some(Reply::call_structured(req.agent, req.request, name, args));
            }
        }

        let text: String = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");

        if text.is_empty() {
            Some(Reply::fail(req.agent, req.request, "empty gemini response"))
        } else {
            Some(Reply::done(req.agent, req.request, text))
        }
    }
}
