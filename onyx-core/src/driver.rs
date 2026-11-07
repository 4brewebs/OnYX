//! Puerto de plataforma.
//! El driver solo vuelca ingress y proyecta.
//! Nunca escribe en el grafo.

use crate::bus::EventBus;
use crate::graph::Graph;
use crate::surface::SurfaceBinding;

/// Adaptador de plataforma.
///
/// - `poll` inyecta eventos de entrada en el bus.
/// - `present` recibe las superficies activas que el cerebro declara.
///   El driver decide cómo materializarlas (o ignorarlas).
/// - Solo lectura sobre el grafo.
pub trait PlatformDriver {
    /// Vuelca ingress al bus.
    /// Solo debe emitir `OnyxEvent::Ingress`.
    fn poll(&mut self, bus: &mut dyn EventBus);

    /// Proyecta el estado sobre las superficies lógicas activas.
    /// `surfaces` puede estar vacío.
    /// El driver no muta el grafo.
    fn present(&mut self, graph: &dyn Graph, surfaces: &[SurfaceBinding]);
}