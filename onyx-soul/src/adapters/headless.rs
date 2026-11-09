//! Driver headless.
//! No abre terminal ni ventana.
//! Sirve para tests, CI y ejecución sin UI.
//! Guarda las superficies y eventos para inspección.

use onyx_core::PlatformDriver;
use onyx_core::bus::EventBus;
use onyx_core::event::OnyxEvent;
use onyx_core::graph::GraphView;
use onyx_core::surface::SurfaceBinding;

/// Driver sin salida.
/// Acumula lo que recibe para poder assertar en tests.
#[derive(Default)]
pub struct HeadlessDriver {
    /// Eventos que se inyectarán en el próximo `poll`.
    pending_events: Vec<OnyxEvent>,
    /// Últimas superficies recibidas en `present`.
    pub last_surfaces: Vec<SurfaceBinding>,
    /// Número de veces que se ha llamado a `present`.
    pub present_count: usize,
    /// Número de veces que se ha llamado a `poll`.
    pub poll_count: usize,
}

impl HeadlessDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encola eventos que el driver emitirá en el próximo `poll`.
    pub fn push_events(&mut self, events: impl IntoIterator<Item = OnyxEvent>) {
        self.pending_events.extend(events);
    }

    /// Atajo para un solo evento.
    pub fn push_event(&mut self, event: OnyxEvent) {
        self.pending_events.push(event);
    }
}

impl PlatformDriver for HeadlessDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        self.poll_count += 1;
        for event in self.pending_events.drain(..) {
            bus.push(event);
        }
    }

    fn present(&mut self, _graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        self.present_count += 1;
        self.last_surfaces = surfaces.to_vec();
    }
}
