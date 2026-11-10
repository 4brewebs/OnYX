//! Bus FIFO síncrono en memoria de este soul.
//!
//! Implementa el puerto `EventBus` de core para que una aplicación de agentes
//! pueda pasar este bus a `tick` sin depender de `onyx-soul`. Es una copia
//! propia, no compartida: los souls no están acoplados entre sí.

use std::collections::VecDeque;

use onyx_core::{EventBus, OnyxEvent};

/// Cola local de eventos. No aporta persistencia ni transporte externo.
#[derive(Default)]
pub struct AgentBus {
    queue: VecDeque<OnyxEvent>,
}

impl AgentBus {
    pub fn new() -> Self {
        Self::default()
    }
}

impl EventBus for AgentBus {
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
