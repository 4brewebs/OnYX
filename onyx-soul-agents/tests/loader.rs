//! El loader y el registry propios de soul-agents arrancan su stage base
//! sin depender de onyx-soul.

use onyx_soul_agents::OnyxError;
use onyx_soul_agents::{
    FACTORIES, InMemorySystemRegistry, RUNTIME_NAME, RustLoader, SPAWNER_NAME, stage,
};

#[test]
fn load_app_builds_the_base_stage_systems_in_blueprint_order() {
    let mut loader = RustLoader::new();
    let mut registry = InMemorySystemRegistry::new();

    let systems = loader
        .load_app(&mut registry, &FACTORIES, &[stage("main")], "main")
        .expect("el stage base debe cargar");

    let names: Vec<&str> = systems.iter().map(|s| s.name()).collect();
    assert_eq!(names, [SPAWNER_NAME, RUNTIME_NAME]);
}

#[test]
fn unknown_stage_and_unknown_kind_are_missing() {
    let mut loader = RustLoader::new();
    let mut registry = InMemorySystemRegistry::new();
    let unknown_stage = loader.load_app(&mut registry, &FACTORIES, &[stage("main")], "otro");
    assert!(matches!(unknown_stage, Err(OnyxError::Missing)));

    let empty_registry = InMemorySystemRegistry::new();
    let unknown_kind = loader.load_systems("main", &empty_registry);
    assert!(matches!(unknown_kind, Err(OnyxError::Missing)));
}
