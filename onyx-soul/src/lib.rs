
//! Implementaciones por defecto de los puertos de OnYX.
//! Cero dependencias externas.

pub mod promt;
pub mod headless;
pub mod bus;
pub mod graph;
pub mod loader;
pub mod registry;

pub use headless::HeadlessDriver;
pub use prompt::PromptDriver;
pub use bus::InMemoryBus;
pub use graph::WorldGraph;
pub use loader::RustLoader;
pub use registry::InMemorySystemRegistry;