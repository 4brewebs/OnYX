//! Anclajes lógicos de proyección.
//! No implican dimensiones, visual ni plano.
//! Headless, TUI, WASM, 3D o VR los materializan como quieran.

use crate::graph::Attr;

/// Anclaje semántico de proyección.
/// El cerebro lo nombra; el driver decide cómo (o si) se dibuja.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SurfaceId(pub u32);

/// Proyección activa declarada por el cerebro en un tick.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SurfaceBinding {
    pub surface: SurfaceId,
    /// Vista o concepto que se proyecta en este anclaje.
    pub view: Attr,
    pub focused: bool,
}
