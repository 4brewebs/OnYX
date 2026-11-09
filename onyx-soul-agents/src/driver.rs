//! Driver specialized for agent harnesses.
//!
//! Projects pending agent requests (concepts in `Thinking`) and accepts
//! replies as ingress. The application implements the actual decision
//! (LLM, human, script) by calling `submit_reply`.

use onyx_core::{
    ConceptId, EventBus, GraphView, IngressEvent, OnyxEvent, PlatformDriver, SurfaceBinding, Value,
};

use crate::vocab::{AgentState, Vocab};

#[derive(Clone, Debug)]
pub struct AgentRequest {
    pub agent: ConceptId,
    pub goal: String,
    pub steps: i64,
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub agent: ConceptId,
    pub verb: String,
    pub tool: Option<String>,
    pub args: Option<String>,
    pub text: Option<String>,
}

#[derive(Default)]
pub struct AgentDriver {
    vocab: Option<Vocab>,
    pending_replies: Vec<Reply>,
    last_requests: Vec<AgentRequest>,
}

impl AgentDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// The application (or a bridge) submits a reply for an agent.
    pub fn submit_reply(&mut self, reply: Reply) {
        self.pending_replies.push(reply);
    }

    /// Requests projected in the last `present`.
    pub fn last_requests(&self) -> &[AgentRequest] {
        &self.last_requests
    }
}

impl PlatformDriver for AgentDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        // Turn pending replies into ingress events.
        // The runtime system is expected to interpret them.
        for reply in self.pending_replies.drain(..) {
            bus.push(OnyxEvent::Ingress(IngressEvent::Raw {
                source: "agent.reply".into(),
                bytes: format!(
                    "{}|{}|{}|{}|{}",
                    reply.agent.0,
                    reply.verb,
                    reply.tool.unwrap_or_default(),
                    reply.args.unwrap_or_default(),
                    reply.text.unwrap_or_default()
                )
                .into_bytes(),
            }));
        }
    }

    fn present(&mut self, view: &dyn GraphView, _surfaces: &[SurfaceBinding]) {
        // Project concepts that are currently Thinking.
        self.last_requests.clear();
        // We cannot call attr() on a shared view, so we rely on the
        // application having resolved the vocab already, or we do a
        // best-effort scan. For a real soul this would be injected.
        // Here we keep it simple: the application sets the vocab via
        // a method if it wants projection.
        if let Some(vocab) = self.vocab {
            for id in view.query(&[]) {
                if let Some(Value::Text(state)) = view.get(id, vocab.state) {
                    if state == AgentState::Thinking.as_str() {
                        let goal = view
                            .get(id, vocab.goal)
                            .and_then(|v| match v {
                                Value::Text(s) => Some(s),
                                _ => None,
                            })
                            .unwrap_or_default();
                        let steps = view
                            .get(id, vocab.steps)
                            .and_then(|v| match v {
                                Value::Int(n) => Some(n),
                                _ => None,
                            })
                            .unwrap_or(0);
                        self.last_requests.push(AgentRequest {
                            agent: id,
                            goal,
                            steps,
                        });
                    }
                }
            }
        }
    }
}

impl AgentDriver {
    /// Optional: inject the resolved vocab so `present` can project.
    pub fn set_vocab(&mut self, vocab: Vocab) {
        self.vocab = Some(vocab);
    }
}
