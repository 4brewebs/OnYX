//! Agent specification helpers.
//!
//! The application can build spawn events itself or use these.

use onyx_core::{ConceptId, DomainEvent, OnyxEvent, Value};

use crate::vocab::Vocab;

#[derive(Clone, Debug)]
pub struct AgentSpec {
    pub name: String,
    pub goal: String,
    pub max_steps: i64,
    pub timeout: i64,
    pub tools: Vec<String>,
}

impl AgentSpec {
    pub fn new(name: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            goal: goal.into(),
            max_steps: 32,
            timeout: 64,
            tools: Vec::new(),
        }
    }
}

pub fn spawn_event(vocab: &Vocab, spec: &AgentSpec) -> OnyxEvent {
    let mut data = vec![
        (vocab.name, Value::Text(spec.name.clone())),
        (vocab.goal, Value::Text(spec.goal.clone())),
        (vocab.max_steps, Value::Int(spec.max_steps)),
        (vocab.timeout, Value::Int(spec.timeout)),
    ];
    if !spec.tools.is_empty() {
        data.push((vocab.tools_csv, Value::Text(spec.tools.join(","))));
    }
    OnyxEvent::Domain(DomainEvent {
        kind: vocab.ev_spawn,
        subject: None,
        data,
    })
}
