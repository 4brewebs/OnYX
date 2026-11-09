//! Agent harness soul for OnYX.
//!
//! This is an implementation of the core ports, specialized for building
//! deterministic multi-agent applications.
//!
//! It depends only on `onyx-core`. Where a piece of `onyx-soul` fits (loader,
//! registry, bus, supervisor), this crate keeps its own copy instead of
//! depending on it, so souls stay decoupled. It adds the peculiarities that
//! agent harnesses need:
//!
//! - Tick-indexed snapshots (rewind to any tick)
//! - Agent-oriented graph helpers (spawn, relate tools/memory)
//! - A driver that projects pending agent requests and accepts replies
//! - Systems that only speak `OnyxEvent`
//!
//! The application decides stages, systems, tools, and policies.
//! This soul closes the framework so the app can be written against it.

pub mod bus;
pub mod config;
pub mod driver;
pub mod graph;
pub mod loader;
pub mod policy;
pub mod registry;
pub mod reply;
pub mod runtime;
pub mod snapshot;
pub mod spawner;
pub mod spec;
pub mod supervisor;
pub mod vocab;

mod util;

pub use bus::AgentBus;
pub use config::{AgentConfig, DEFAULT_PREFIX};
pub use driver::{AgentDriver, AgentRequest};
pub use graph::AgentGraph;
pub use loader::RustLoader;
pub use policy::{
    AgentView, Decision, DefaultMemory, MemoryEntry, MemoryPolicy, ScriptedPolicy, UniformTool,
};
pub use registry::{InMemorySystemRegistry, SystemFactory};
pub use reply::{REPLY_SOURCE, Reply, Verb};
pub use runtime::{AgentRuntime, RUNTIME_NAME};
pub use snapshot::AgentSnapshotStore;
pub use spawner::{AgentSpawner, SPAWNER_NAME};
pub use spec::{AgentSpec, Policy, spawn_event};
pub use supervisor::Supervisor;
pub use vocab::{AgentState, Vocab};

// Contratos de core que una aplicación necesita, reexportados para que la app
// dependa solo de este soul y no declare `onyx-core` en su `Cargo.toml`.
// Incluye los traits de los puertos (`EventBus`, `PlatformDriver`, `Graph`...):
// sin ellos la app no podría llamar a sus métodos.
// Los módulos `bus`, `driver` y `graph` de core no se reexportan con ese
// nombre porque este soul tiene los suyos; sus tipos sí van en la raíz.
pub use onyx_core::{
    Attr, AttrOnly, AttrRegistry, BlueprintLoader, ConceptId, Ctx, DetRng, DomainEvent, EntitySpec,
    EventBus, Graph, GraphView, IngressEvent, Key, KeyCode, Mods, OnyxError, OnyxEvent, OnyxSystem,
    PlatformDriver, ReadOnly, SnapshotStore, StageBlueprint, SurfaceBinding, SurfaceId,
    SystemEvent, SystemRegistry, SystemSpec, SystemState, Value,
};
pub use onyx_core::{detrng, event, surface, system, tick};

/// Lee la configuración de `spec.params` con [`AgentConfig::from_params`].
pub fn make_spawner(spec: &SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError> {
    let config = AgentConfig::from_params(&spec.params)?;
    Ok(Box::new(AgentSpawner::new(config)))
}

/// Lee la misma configuración que el spawner para compartir prefijo.
pub fn make_runtime(spec: &SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError> {
    let config = AgentConfig::from_params(&spec.params)?;
    Ok(Box::new(AgentRuntime::new(config)))
}

type Factory = fn(&SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError>;

pub const FACTORIES: [(&str, Factory); 2] =
    [(SPAWNER_NAME, make_spawner), (RUNTIME_NAME, make_runtime)];

/// Stage base con spawner y runtime, en ese orden, y la configuración por
/// defecto. La aplicación lo amplía o lo sustituye.
pub fn stage(name: &str) -> StageBlueprint {
    stage_with(name, &AgentConfig::default())
}

/// Como [`stage`], con `config` en los params de ambos sistemas para que
/// compartan prefijo y límites.
pub fn stage_with(name: &str, config: &AgentConfig) -> StageBlueprint {
    let spec = |kind: &str| SystemSpec {
        kind: kind.to_string(),
        params: config.to_params(),
        persistent: true,
    };
    StageBlueprint {
        name: name.to_string(),
        systems: vec![spec(SPAWNER_NAME), spec(RUNTIME_NAME)],
        entities: Vec::new(),
    }
}
