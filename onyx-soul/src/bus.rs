//! Bus FIFO síncrono en memoria.

use std::collections::VecDeque;

use onyx_core::{EventBus, OnyxEvent};

/// Cola local de eventos. No aporta persistencia ni transporte externo.
#[derive(Default)]
pub struct InMemoryBus {
    queue: VecDeque<OnyxEvent>,
}

impl InMemoryBus {
    pub fn new() -> Self {
        Self::default()
    }
}

impl EventBus for InMemoryBus {
    fn push(&mut self, event: OnyxEvent) {
        self.queue.push_back(event);
    }

    fn drain(&mut self) -> Vec<OnyxEvent> {
        self.queue.drain(..).collect()
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}
