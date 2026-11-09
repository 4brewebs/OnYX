//! Supervisor de estados de sistemas de este soul.
//! Mantiene el slice de `SystemState` y aplica las reglas
//! de Active / Stopped / Blocked entre ticks.
//!
//! Copia propia del supervisor de `onyx-soul`, no compartida: los souls no
//! están acoplados entre sí y una app de agentes no depende de `onyx-soul`.

use onyx_core::graph::{AttrOnly, Graph};
use onyx_core::system::{OnyxSystem, SystemState};

/// Mantiene el estado de ejecución de cada sistema.
pub struct Supervisor {
    states: Vec<SystemState>,
}

impl Supervisor {
    /// Crea un supervisor con todos los sistemas en `Stopped`.
    pub fn new(system_count: usize) -> Self {
        Self {
            states: vec![SystemState::Stopped; system_count],
        }
    }

    /// Slice mutable para pasarlo al `tick`.
    pub fn states_mut(&mut self) -> &mut [SystemState] {
        &mut self.states
    }

    /// Slice de solo lectura.
    pub fn states(&self) -> &[SystemState] {
        &self.states
    }

    /// Marca un sistema como Active (si no está Blocked).
    pub fn start(&mut self, index: usize) -> bool {
        if let Some(state) = self.states.get_mut(index) {
            if *state != SystemState::Blocked {
                *state = SystemState::Active;
                return true;
            }
        }
        false
    }

    /// Marca un sistema como Stopped.
    pub fn stop(&mut self, index: usize) {
        if let Some(state) = self.states.get_mut(index) {
            if *state == SystemState::Active {
                *state = SystemState::Stopped;
            }
        }
    }

    /// Fuerza Blocked (por ejemplo tras un init fallido).
    pub fn block(&mut self, index: usize) {
        if let Some(state) = self.states.get_mut(index) {
            *state = SystemState::Blocked;
        }
    }

    /// Desbloquea a Stopped si estaba Blocked.
    pub fn unblock(&mut self, index: usize) {
        if let Some(state) = self.states.get_mut(index) {
            if *state == SystemState::Blocked {
                *state = SystemState::Stopped;
            }
        }
    }

    /// Inicializa todos los sistemas y aplica la política de bloqueo.
    /// Llama a `init`. Si falla → Blocked. Si tiene éxito y estaba Blocked → Stopped.
    pub fn init_all(&mut self, systems: &mut [Box<dyn OnyxSystem>], graph: &mut dyn Graph) {
        for (i, system) in systems.iter_mut().enumerate() {
            match system.init(&mut AttrOnly(&mut *graph)) {
                Ok(()) => {
                    if self.states.get(i) == Some(&SystemState::Blocked) {
                        self.unblock(i);
                    }
                }
                Err(_) => {
                    self.block(i);
                }
            }
        }
    }
}
