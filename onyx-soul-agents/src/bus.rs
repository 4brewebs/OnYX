//! In-memory bus. Reuses the semantics of `InMemoryBus`.
//!
//! Provided so the application does not have to depend on `onyx-soul`
//! for the common case. The application can still swap it.

use onyx_core::OnyxEvent;

pub struct AgentBus {
    pending: Vec<OnyxEvent>,
}

impl AgentBus {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    pub fn push(&mut self, ev: OnyxEvent) {
        self.pending.push(ev);
    }

    pub fn take(&mut self) -> Vec<OnyxEvent> {
        std::mem::take(&mut self.pending)
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}
