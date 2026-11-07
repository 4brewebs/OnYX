//! Implementaciones por defecto de los puertos de OnYX.
//! Sin dependencias externas; usa el crate interno onyx-core.

pub mod bus;

pub use bus::InMemoryBus;

pub mod graph;
pub use graph::WorldGraph;
