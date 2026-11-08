//! Implementaciones por defecto de los puertos de OnYX.
//! Cero dependencias externas.

// Adaptadores con dependencias externas.
// Temporalmente aquí. El soul zero-deps no los incluye.
// En el futuro vivirán en crates aparte.
pub mod adapters;
pub mod bus;
pub mod graph;
pub mod loader;
pub mod registry;
pub mod store;
pub mod supervisor;

pub use adapters::{HeadlessDriver, PromptDriver, TuiDriver}; // Dependencias temporales
pub use bus::InMemoryBus;
pub use graph::WorldGraph;
pub use loader::RustLoader;
pub use registry::InMemorySystemRegistry;
pub use store::FileSnapshotStore;
pub use supervisor::Supervisor;

pub use onyx_core::{event, system, tick};
pub use onyx_core::graph::{Attr, ConceptId, Value, AttrRegistry, OnyxError, GraphView};
