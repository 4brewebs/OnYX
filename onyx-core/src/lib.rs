//! OnYX Core — Contratos del cerebro.
//! Cero dependencias. Solo reglas y puertos.

pub mod bus;
pub mod detrng;
pub mod event;
pub mod graph;
pub mod system;
pub mod tick;
pub mod surface;
pub mod driver;

pub use bus::EventBus;
pub use event::{DomainEvent, IngressEvent, Key, KeyCode, Mods, OnyxEvent, SystemEvent};
pub use graph::{
    Attr, AttrOnly, AttrRegistry, ConceptId, Graph, GraphView, OnyxError, PlatformDriver, ReadOnly,
    SnapshotStore, Value,
};

pub use system::{
    BlueprintLoader, Ctx, EntitySpec, OnyxSystem, StageBlueprint, SystemRegistry, SystemSpec,
    SystemState,
};

pub use tick::tick;

pub use detrng::DetRng;

pub use surface::{SufaceId, Surface binding};

pub use driver::PlatformDriver;

#[cfg(feature = "conformance")]
pub mod conformance;
