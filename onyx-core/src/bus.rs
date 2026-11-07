//! Puerto síncrono del bus de eventos.

use crate::OnyxEvent;

/// Cola FIFO de eventos propiedad del llamador.
/// El soul o un adaptador aporta la implementación; el core no elige el transporte.
///
/// `push` conserva el orden de inserción. `drain` extrae todos los eventos
/// pendientes en ese orden y los retira del bus, sin duplicarlos ni perderlos.
/// Tras `drain`, `is_empty` es true hasta el siguiente `push`.
///
/// Este puerto no expresa fallos de transporte, confirmaciones ni operaciones
/// asíncronas. Un adaptador externo debe resolverlos fuera del tick y cumplir
/// esta semántica síncrona; conectar un broker no la garantiza por sí solo.
pub trait EventBus {
    fn push(&mut self, event: OnyxEvent);
    fn drain(&mut self) -> Vec<OnyxEvent>;
    fn is_empty(&self) -> bool;
}
