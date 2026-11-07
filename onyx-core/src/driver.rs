use crate::bus::EventBus;
use crate::graph::Graph;
use crate::surface::SurfaceBinding;

pub trait PlatformDriver {
    fn poll(&mut self, bus: &mut dyn EventBus);
    fn present(&mut self, graph: &dyn Graph, surfaces: &[SurfaceBinding]);
}