//! Spawner system.
//!
//! Listens for spawn events and creates agent concepts.
//! Limit is configurable via SystemSpec params.

use onyx_core::{AttrRegistry, Ctx, DomainEvent, OnyxEvent, OnyxSystem, Value};

use crate::vocab::{AgentState, Vocab};

pub const SPAWNER_NAME: &str = "agent_spawner";

pub struct AgentSpawner {
    max_agents: usize,
    vocab: Option<Vocab>,
}

impl AgentSpawner {
    pub fn new(max_agents: usize) -> Self {
        Self {
            max_agents,
            vocab: None,
        }
    }
}

impl OnyxSystem for AgentSpawner {
    fn name(&self) -> &str {
        SPAWNER_NAME
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), onyx_core::OnyxError> {
        self.vocab = Some(Vocab::resolve(attrs));
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let vocab = match self.vocab {
            Some(v) => v,
            None => return,
        };

        let mut count = ctx
            .graph
            .query(&[])
            .into_iter()
            .filter(|id| ctx.graph.get(*id, vocab.state).is_some())
            .count();

        for ev in ctx.inbox {
            if let OnyxEvent::Domain(d) = ev {
                if d.kind == vocab.ev_spawn {
                    if count >= self.max_agents {
                        ctx.outbox.push(OnyxEvent::Domain(DomainEvent {
                            kind: vocab.ev_rejected,
                            subject: None,
                            data: vec![(vocab.reason, Value::Text("max_agents".into()))],
                        }));
                        continue;
                    }
                    let id = ctx.graph.create();
                    for (attr, val) in &d.data {
                        ctx.graph.set(id, *attr, val.clone());
                    }
                    ctx.graph.set(
                        id,
                        vocab.state,
                        Value::Text(AgentState::Idle.as_str().into()),
                    );
                    ctx.graph.set(id, vocab.steps, Value::Int(0));
                    ctx.outbox.push(OnyxEvent::Domain(DomainEvent {
                        kind: vocab.ev_spawned,
                        subject: Some(id),
                        data: Vec::new(),
                    }));
                    count += 1;
                }
            }
        }
    }
}
