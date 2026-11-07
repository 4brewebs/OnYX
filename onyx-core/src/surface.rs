/// Anclaje lógico de proyección. No implica dimensiones ni visual.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SurfaceId(pub u32);

/// Proyección activa declarada por el cerebro.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SurfaceBinding {
    pub surface: SurfaceId,
    pub view: Attr,
    pub focused: bool,
}