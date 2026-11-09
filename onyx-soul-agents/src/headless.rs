//! Driver headless de este soul.
//!
//! No abre terminal ni ventana. Sirve para tests, CI y para monitorizar las
//! [`SurfaceBinding`] que el cerebro declara en `present`. El puente puede
//! envolverlo con [`crate::RecordingBridge::wrapping`]: el modelo entra por el
//! puente y este driver recibe el mismo slice de superficies.
//!
//! `wrapping` guarda el inner en un `Box` y no lo expone. Por eso el estado
//! observable vive detrás de un [`HeadlessHandle`] clonable: la app conserva
//! el handle y entrega el [`HeadlessDriver`].

use std::cell::RefCell;
use std::rc::Rc;

use onyx_core::{EventBus, GraphView, OnyxEvent, PlatformDriver, SurfaceBinding};

#[derive(Default)]
struct HeadlessState {
    pending_events: Vec<OnyxEvent>,
    last_surfaces: Vec<SurfaceBinding>,
    present_count: usize,
    poll_count: usize,
}

/// Vista compartida del monitor. Sobrevive a `wrapping`.
#[derive(Clone, Default)]
pub struct HeadlessHandle {
    inner: Rc<RefCell<HeadlessState>>,
}

impl HeadlessHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construye un driver que comparte este estado.
    pub fn driver(&self) -> HeadlessDriver {
        HeadlessDriver {
            inner: Rc::clone(&self.inner),
        }
    }

    pub fn last_surfaces(&self) -> Vec<SurfaceBinding> {
        self.inner.borrow().last_surfaces.clone()
    }

    pub fn present_count(&self) -> usize {
        self.inner.borrow().present_count
    }

    pub fn poll_count(&self) -> usize {
        self.inner.borrow().poll_count
    }

    /// Eventos que el driver inyectará en el próximo `poll`.
    pub fn push_events(&self, events: impl IntoIterator<Item = OnyxEvent>) {
        self.inner.borrow_mut().pending_events.extend(events);
    }

    /// Atajo para un solo evento.
    pub fn push_event(&self, event: OnyxEvent) {
        self.inner.borrow_mut().pending_events.push(event);
    }
}

/// Driver sin salida. Acumula superficies y contadores para inspección.
#[derive(Default)]
pub struct HeadlessDriver {
    inner: Rc<RefCell<HeadlessState>>,
}

impl HeadlessDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle que sigue viendo contadores y superficies tras `wrapping`.
    pub fn handle(&self) -> HeadlessHandle {
        HeadlessHandle {
            inner: Rc::clone(&self.inner),
        }
    }

    pub fn last_surfaces(&self) -> Vec<SurfaceBinding> {
        self.inner.borrow().last_surfaces.clone()
    }

    pub fn present_count(&self) -> usize {
        self.inner.borrow().present_count
    }

    pub fn poll_count(&self) -> usize {
        self.inner.borrow().poll_count
    }

    /// Encola eventos que el driver emitirá en el próximo `poll`.
    pub fn push_events(&mut self, events: impl IntoIterator<Item = OnyxEvent>) {
        self.inner.borrow_mut().pending_events.extend(events);
    }

    /// Atajo para un solo evento.
    pub fn push_event(&mut self, event: OnyxEvent) {
        self.inner.borrow_mut().pending_events.push(event);
    }
}

impl PlatformDriver for HeadlessDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        let mut state = self.inner.borrow_mut();
        state.poll_count += 1;
        for event in state.pending_events.drain(..) {
            bus.push(event);
        }
    }

    fn present(&mut self, _graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        let mut state = self.inner.borrow_mut();
        state.present_count += 1;
        state.last_surfaces = surfaces.to_vec();
    }
}
