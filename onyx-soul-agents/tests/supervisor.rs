//! El supervisor propio de soul-agents aplica las reglas de `SystemState`
//! del contrato de core, y junto con los demás puertos de este soul basta
//! para ejecutar un `tick` sin depender de `onyx-soul`.
//!
//! Como una aplicación, estas pruebas solo nombran `onyx_soul_agents`: todo
//! lo de core que usan llega por sus reexportaciones.

use onyx_soul_agents::{
    AgentBus, AgentDriver, AgentGraph, AgentSnapshotStore, AgentSpec, FACTORIES,
    InMemorySystemRegistry, RustLoader, Supervisor, Vocab, spawn_event, stage,
};
use onyx_soul_agents::{AttrRegistry, Ctx, EventBus, OnyxError, OnyxSystem, SystemState, tick};

/// Sistema cuyo `init` falla mientras `fail` sea true.
struct Flaky {
    fail: bool,
}

impl OnyxSystem for Flaky {
    fn name(&self) -> &str {
        "flaky"
    }
    fn init(&mut self, _: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        if self.fail {
            Err(OnyxError::Missing)
        } else {
            Ok(())
        }
    }
    fn run(&mut self, _: &mut Ctx) {}
}

#[test]
fn failed_init_blocks_until_a_successful_init_returns_it_to_stopped() {
    let mut graph = AgentGraph::new();
    let mut systems: Vec<Box<dyn OnyxSystem>> = vec![Box::new(Flaky { fail: true })];
    let mut sup = Supervisor::new(1);

    sup.init_all(&mut systems, &mut graph);
    assert_eq!(sup.states(), [SystemState::Blocked]);
    assert!(!sup.start(0), "Start no activa un sistema bloqueado");
    assert_eq!(sup.states(), [SystemState::Blocked]);

    systems[0] = Box::new(Flaky { fail: false });
    sup.init_all(&mut systems, &mut graph);
    assert_eq!(
        sup.states(),
        [SystemState::Stopped],
        "un init correcto desbloquea a Stopped"
    );

    assert!(sup.start(0));
    assert_eq!(sup.states(), [SystemState::Active]);
}

#[test]
fn successful_reinit_keeps_the_execution_state_of_an_unblocked_system() {
    let mut graph = AgentGraph::new();
    let mut systems: Vec<Box<dyn OnyxSystem>> = vec![Box::new(Flaky { fail: false })];
    let mut sup = Supervisor::new(1);

    sup.init_all(&mut systems, &mut graph);
    assert!(sup.start(0));
    sup.init_all(&mut systems, &mut graph);
    assert_eq!(sup.states(), [SystemState::Active]);

    sup.stop(0);
    assert_eq!(sup.states(), [SystemState::Stopped]);
    sup.init_all(&mut systems, &mut graph);
    assert_eq!(sup.states(), [SystemState::Stopped]);
}

#[test]
fn a_tick_with_only_this_souls_ports_spawns_and_projects_an_agent() {
    let dir = std::env::temp_dir().join(format!(
        "onyx-soul-agents-supervisor-test-{}",
        std::process::id()
    ));
    let mut store = AgentSnapshotStore::new(&dir);
    let mut graph = AgentGraph::new();
    let mut bus = AgentBus::new();
    let mut driver = AgentDriver::new();

    let mut loader = RustLoader::new();
    let mut registry = InMemorySystemRegistry::new();
    let mut systems = loader
        .load_app(&mut registry, &FACTORIES, &[stage("main")], "main")
        .unwrap();
    let mut sup = Supervisor::new(systems.len());
    sup.init_all(&mut systems, &mut graph);
    for i in 0..systems.len() {
        assert!(sup.start(i));
    }

    let vocab = Vocab::resolve(&mut graph);
    driver.set_vocab(vocab);
    bus.push(spawn_event(&vocab, &AgentSpec::new("a1", "resumir")));

    tick(
        1,
        0,
        &mut driver,
        &mut graph,
        &mut store,
        &mut bus,
        &mut systems,
        sup.states_mut(),
        &[],
    );

    let requests = driver.last_requests();
    assert_eq!(
        requests.len(),
        1,
        "el agente creado debe pedir su primer paso"
    );
    assert_eq!(requests[0].goal, "resumir");

    let _ = std::fs::remove_dir(&dir);
}
