//! Agent harness soul for OnYX.
//!
//! This is an implementation of the core ports, specialized for building
//! deterministic multi-agent applications.
//!
//! It reuses pieces from `onyx-soul` where they fit, and adds the
//! peculiarities that agent harnesses need:
//!
//! - Tick-indexed snapshots (rewind to any tick)
//! - Agent-oriented graph helpers (spawn, relate tools/memory)
//! - A driver that projects pending agent requests and accepts replies
//! - Systems that only speak `OnyxEvent`
//!
//! The application decides stages, systems, tools, and policies.
//! This soul closes the framework so the app can be written against it.

pub mod bus;
pub mod driver;
pub mod graph;
pub mod loader;
pub mod registry;
pub mod runtime;
pub mod snapshot;
pub mod spawner;
pub mod spec;
pub mod supervisor;
pub mod vocab;

pub use bus::AgentBus;
pub use driver::{AgentDriver, AgentRequest, Reply};
pub use graph::AgentGraph;
pub use loader::RustLoader;
pub use registry::{InMemorySystemRegistry, SystemFactory};
pub use runtime::{AgentRuntime, RUNTIME_NAME};
pub use snapshot::AgentSnapshotStore;
pub use spawner::{AgentSpawner, SPAWNER_NAME};
pub use spec::{AgentSpec, spawn_event};
pub use supervisor::Supervisor;
pub use vocab::{AgentState, Vocab};

use onyx_core::{OnyxError, OnyxSystem, StageBlueprint, SystemSpec, Value};

pub const DEFAULT_MAX_AGENTS: i64 = 64;

pub fn make_spawner(spec: &SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError> {
    let max = spec
        .params
        .iter()
        .find(|(k, _)| k == "max_agents")
        .map(|(_, v)| v.clone())
        .unwrap_or(Value::Int(DEFAULT_MAX_AGENTS));
    match max {
        Value::Int(n) if n >= 1 => Ok(Box::new(AgentSpawner::new(n as usize))),
        _ => Err(OnyxError::Format(
            "max_agents must be an integer >= 1".into(),
        )),
    }
}

pub fn make_runtime(_spec: &SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError> {
    Ok(Box::new(AgentRuntime::new()))
}

type Factory = fn(&SystemSpec) -> Result<Box<dyn OnyxSystem>, OnyxError>;

pub const FACTORIES: [(&str, Factory); 2] =
    [(SPAWNER_NAME, make_spawner), (RUNTIME_NAME, make_runtime)];

/// Base stage. The application extends or replaces this.
pub fn stage(name: &str) -> StageBlueprint {
    StageBlueprint {
        name: name.to_string(),
        systems: vec![
            SystemSpec {
                kind: SPAWNER_NAME.to_string(),
                params: Vec::new(),
                persistent: true,
            },
            SystemSpec {
                kind: RUNTIME_NAME.to_string(),
                params: Vec::new(),
                persistent: true,
            },
        ],
        entities: Vec::new(),
    }
}
