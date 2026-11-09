//! Runtime system.
//!
//! Advances agents according to the rules of this soul.
//! The application can add other systems that react to the same events.

use onyx_core::{AttrRegistry, Ctx, DomainEvent, OnyxEvent, OnyxSystem, Value};

use crate::config::AgentConfig;
use crate::vocab::{AgentState, Vocab};

pub const RUNTIME_NAME: &str = "agent_runtime";

pub struct AgentRuntime {
    /// Prefijo del vocabulario; debe ser el mismo que el del spawner.
    prefix: String,
    vocab: Option<Vocab>,
}

impl AgentRuntime {
    /// Runtime con el prefijo de `config`.
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            prefix: config.vocab_prefix.clone(),
            vocab: None,
        }
    }
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::new(&AgentConfig::default())
    }
}

impl OnyxSystem for AgentRuntime {
    fn name(&self) -> &str {
        RUNTIME_NAME
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), onyx_core::OnyxError> {
        self.vocab = Some(Vocab::resolve_with(attrs, &self.prefix));
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let vocab = match self.vocab {
            Some(v) => v,
            None => return,
        };

        // React to tool results
        for ev in ctx.inbox {
            if let OnyxEvent::Domain(d) = ev {
                if d.kind == vocab.ev_tool_result {
                    if let Some(subject) = d.subject {
                        if let Some(Value::Text(s)) = ctx.graph.get(subject, vocab.state) {
                            if s == AgentState::Acting.as_str() {
                                ctx.graph.set(
                                    subject,
                                    vocab.state,
                                    Value::Text(AgentState::Idle.as_str().into()),
                                );
                            }
                        }
                    }
                }
            }
        }

        // Advance Idle agents under budget
        for id in ctx.graph.query(&[]) {
            if let Some(Value::Text(state)) = ctx.graph.get(id, vocab.state) {
                if state == AgentState::Idle.as_str() {
                    let steps = match ctx.graph.get(id, vocab.steps) {
                        Some(Value::Int(n)) => n,
                        _ => 0,
                    };
                    let max = match ctx.graph.get(id, vocab.max_steps) {
                        Some(Value::Int(n)) => n,
                        _ => 32,
                    };
                    if steps >= max {
                        ctx.graph.set(
                            id,
                            vocab.state,
                            Value::Text(AgentState::Failed.as_str().into()),
                        );
                        ctx.outbox.push(OnyxEvent::Domain(DomainEvent {
                            kind: vocab.ev_failed,
                            subject: Some(id),
                            data: vec![(vocab.reason, Value::Text("max_steps".into()))],
                        }));
                        continue;
                    }
                    ctx.graph.set(
                        id,
                        vocab.state,
                        Value::Text(AgentState::Thinking.as_str().into()),
                    );
                    ctx.graph.set(id, vocab.steps, Value::Int(steps + 1));
                }
            }
        }
    }
}
