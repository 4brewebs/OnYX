//! OnYX Core — Contratos del cerebro.
//! Cero dependencias. Solo reglas y puertos.

pub mod detrng;
pub mod event;
pub mod graph;

pub use event::{DomainEvent, IngressEvent, Key, KeyCode, Mods, OnyxEvent, SystemEvent};
pub use graph::{Attr, ConceptId, Graph, OnyxError, PlatformDriver, SnapshotStore, Value};

pub use detrng::DetRng;
