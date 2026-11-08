//! Agent-oriented graph.
//!
//! Wraps `WorldGraph` and adds helpers that agent systems expect.
//! The contract remains `Graph`. The application can still use the raw graph.

use onyx_core::{Attr, ConceptId, Graph, OnyxError, Value};
use onyx_soul::WorldGraph;

use crate::vocab::{AgentState, Vocab};

pub struct AgentGraph {
    inner: WorldGraph,
}

impl AgentGraph {
    pub fn new() -> Self {
        Self {
            inner: WorldGraph::new(),
        }
    }

    pub fn inner(&self) -> &WorldGraph {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut WorldGraph {
        &mut self.inner
    }

    /// Spawn an agent concept with the minimal attributes.
    /// The application can set more afterwards.
    pub fn spawn_agent(&mut self, vocab: &Vocab, name: &str, goal: &str) -> ConceptId {
        let id = self.inner.create();
        self.inner.set(id, vocab.name, Value::Text(name.into()));
        self.inner.set(id, vocab.goal, Value::Text(goal.into()));
        self.inner
            .set(id, vocab.state, Value::Text(AgentState::Idle.as_str().into()));
        self.inner.set(id, vocab.steps, Value::Int(0));
        id
    }

    /// Count concepts that have an agent state.
    pub fn agent_count(&self, vocab: &Vocab) -> usize {
        self.inner
            .query(&[])
            .into_iter()
            .filter(|id| self.inner.get(*id, vocab.state).is_some())
            .count()
    }
}

impl Graph for AgentGraph {
    fn attr(&mut self, name: &str) -> Attr {
        self.inner.attr(name)
    }

    fn create(&mut self) -> ConceptId {
        self.inner.create()
    }

    fn destroy(&mut self, id: ConceptId) {
        self.inner.destroy(id);
    }

    fn get(&self, id: ConceptId, attr: Attr) -> Option<&Value> {
        self.inner.get(id, attr)
    }

    fn set(&mut self, id: ConceptId, attr: Attr, value: Value) {
        self.inner.set(id, attr, value);
    }

    fn query(&self, with: &[Attr]) -> Vec<ConceptId> {
        self.inner.query(with)
    }

    fn relate(&mut self, from: ConceptId, rel: Attr, to: ConceptId) {
        self.inner.relate(from, rel, to);
    }

    fn unrelate(&mut self, from: ConceptId, rel: Attr, to: ConceptId) {
        self.inner.unrelate(from, rel, to);
    }

    fn related(&self, from: ConceptId, rel: Attr) -> Vec<ConceptId> {
        self.inner.related(from, rel)
    }

    fn export(&self) -> Vec<u8> {
        self.inner.export()
    }

    fn import(&mut self, bytes: &[u8]) -> Result<(), OnyxError> {
        self.inner.import(bytes)
    }
}
