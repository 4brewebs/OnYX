//! Respuesta retrasada de ANTES de un restore/rewind que llega DESPUÉS.
//!
//! Contrato (rustdoc de `Reply` y de `AgentRuntime::apply`): una respuesta que no
//! corresponde a la petición pendiente se descarta, «ni tarde, ni duplicada, ni ajena».
//! Las respuestas en vuelo no forman parte del snapshot (`AgentHost::request_restore`).
//! Tras el restore, el contador `request` vuelve atrás y la petición del turno nuevo reutiliza
//! el mismo par `(agent, request)` que la petición en vuelo de antes del restore. Una respuesta
//! tardía a esa petición vieja no debe aplicarse al turno nuevo.
//!
//! Solo API pública de `onyx-soul-agents`: `HeadlessDriver::push_event(reply.to_event())`
//! simula al proveedor que contesta tarde.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSpec, AgentState, AgentView, HeadlessDriver, OnyxError,
    RecordingBridge, Reply, SnapshotStore,
};

#[derive(Clone, Default)]
struct Store(Rc<RefCell<BTreeMap<String, Vec<u8>>>>);

impl SnapshotStore for Store {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        self.0.borrow_mut().insert(key.into(), bytes.to_vec());
        Ok(())
    }
    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> {
        self.0.borrow().get(key).cloned().ok_or(OnyxError::Missing)
    }
}

fn host(store: Store) -> AgentHost {
    let mut host = AgentHost::builder(7)
        .config(AgentConfig::default())
        .store(Box::new(store))
        .build()
        .unwrap();
    host.spawn(
        &AgentSpec::new("worker", "goal")
            .with_max_steps(10)
            .with_timeout_ticks(10_000),
    );
    host
}

fn step_until_thinking(host: &mut AgentHost, drv: &mut HeadlessDriver) {
    for _ in 0..20 {
        if let Some(&agent) = host.agents().first() {
            if host.agent_state(agent) == Some(AgentState::Thinking) {
                return;
            }
        }
        host.step(drv);
    }
    panic!("agent never reached Thinking");
}

/// Peticiones pendientes vistas por un bridge observador que no contesta.
fn pending_requests(host: &mut AgentHost) -> Vec<(u64, u64)> {
    let seen: Rc<RefCell<Vec<(u64, u64)>>> = Rc::default();
    let sink = seen.clone();
    let mut probe = RecordingBridge::new(host.vocab(), move |v: &AgentView| {
        sink.borrow_mut().push((v.agent.0, v.request));
        None
    });
    host.step(&mut probe);
    seen.take()
}

/// Control: sin restore, una respuesta tardía a una petición ya superada se descarta y el
/// turno vigente termina con su propia respuesta.
#[test]
fn late_reply_to_superseded_request_is_discarded() {
    let mut host = host(Store::default());
    let mut drv = HeadlessDriver::new();
    step_until_thinking(&mut host, &mut drv);
    let agent = host.agents()[0];

    drv.push_event(Reply::say(agent, 1, "thinking").to_event());
    host.step(&mut drv);
    step_until_thinking(&mut host, &mut drv);

    drv.push_event(Reply::done(agent, 1, "OLD").to_event());
    host.step(&mut drv);
    assert_ne!(host.result(agent).as_deref(), Some("OLD"));
    assert_eq!(host.agent_state(agent), Some(AgentState::Thinking));

    drv.push_event(Reply::done(agent, 2, "NEW").to_event());
    host.step(&mut drv);
    assert_eq!(host.agent_state(agent), Some(AgentState::Done));
    assert_eq!(host.result(agent).as_deref(), Some("NEW"));
}

/// Rewind con petición en vuelo: la respuesta de la petición `(agent, 2)` de antes del restore
/// llega cuando el turno nuevo ya tiene pendiente su propia `(agent, 2)`. No debe aplicarse.
#[test]
#[ignore = "BUG: la respuesta tardía de antes del restore se aplica al turno nuevo: Reply solo lleva (agent, request) y el restore rebobina request, sin época/generación que distinga peticiones (onyx-soul-agents/src/runtime.rs:433-439, reply.rs:68-75)"]
fn late_reply_from_before_restore_is_not_applied_to_new_turn() {
    let store = Store::default();
    let mut host = host(store.clone());
    let mut drv = HeadlessDriver::new();
    host.request_snapshot("cut");
    host.step(&mut drv);
    assert!(store.0.borrow().contains_key("cut"));
    step_until_thinking(&mut host, &mut drv);
    let agent = host.agents()[0];
    // Turno previo al rewind: request 1 contesta `say`, request 2 queda en vuelo y su
    // respuesta "OLD" se retrasa.
    drv.push_event(Reply::say(agent, 1, "before").to_event());
    host.step(&mut drv);
    step_until_thinking(&mut host, &mut drv);
    let late = Reply::done(agent, 2, "OLD");

    host.request_restore("cut");
    host.step(&mut drv);
    step_until_thinking(&mut host, &mut drv);
    // El restore rebobinó `request`: la petición pendiente vuelve a ser la 1.
    assert_eq!(pending_requests(&mut host), vec![(agent.0, 1)]);
    drv.push_event(Reply::say(agent, 1, "after").to_event());
    host.step(&mut drv);
    step_until_thinking(&mut host, &mut drv);
    // Turno nuevo con su propia (agent, 2) pendiente. Llega la respuesta vieja.
    assert_eq!(pending_requests(&mut host), vec![(agent.0, 2)]);
    drv.push_event(late.to_event());
    host.step(&mut drv);
    assert_ne!(
        host.result(agent).as_deref(),
        Some("OLD"),
        "a reply produced before the restore must not complete the new turn"
    );
    assert_eq!(host.agent_state(agent), Some(AgentState::Thinking));
}
