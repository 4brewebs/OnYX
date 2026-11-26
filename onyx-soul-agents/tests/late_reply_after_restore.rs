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

/// La única petición pendiente `(agent, request)` que ve un bridge observador.
fn the_pending_request(host: &mut AgentHost) -> (u64, u64) {
    let pending = pending_requests(host);
    assert_eq!(pending.len(), 1, "exactly one pending request: {pending:?}");
    pending[0]
}

/// Rewind con petición en vuelo: la respuesta de la petición en vuelo de antes del restore
/// (la «vieja», con su `(agent, request)`) llega cuando el turno nuevo ya tiene pendiente su
/// propia petición. No debe aplicarse al turno nuevo, y este debe terminar con SU respuesta.
///
/// Este test NO fija qué número de `request` tiene cada petición tras el restore: hoy el
/// restore rebobina `request` (la vieja y la nueva comparten clave, que es justo el bug), pero
/// FIX-31 prevé una época por petición, con lo que los números pueden cambiar. Por eso solo
/// aserta lo observable y estable: exactamente una pendiente en cada momento, que OLD no
/// completa nada y que el turno nuevo acaba con la respuesta que corresponde a su pendiente.
/// Cuando se decida cómo viaja la época en `Reply`, la respuesta vieja se construirá con la
/// época vieja y este test se adaptará (el resto de asserts se mantiene).
#[test]
#[ignore = "BUG: FIX-31 la respuesta tardía de antes del restore se aplica al turno nuevo: Reply solo lleva (agent, request) y el restore rebobina request, sin época/generación que distinga peticiones (onyx-soul-agents/src/runtime.rs:433-439, reply.rs:68-75)"]
fn late_reply_from_before_restore_is_not_applied_to_new_turn() {
    let store = Store::default();
    let mut host = host(store.clone());
    let mut drv = HeadlessDriver::new();
    host.request_snapshot("cut");
    host.step(&mut drv);
    assert!(store.0.borrow().contains_key("cut"));
    step_until_thinking(&mut host, &mut drv);
    let agent = host.agents()[0];
    // Turno previo al rewind: la primera petición contesta `say` y la siguiente queda en
    // vuelo; su respuesta "OLD" se retrasa.
    let (_, first) = the_pending_request(&mut host);
    drv.push_event(Reply::say(agent, first, "before").to_event());
    host.step(&mut drv);
    step_until_thinking(&mut host, &mut drv);
    let (old_agent, old_request) = the_pending_request(&mut host);
    assert_eq!(old_agent, agent.0);
    let late = Reply::done(agent, old_request, "OLD");

    host.request_restore("cut");
    host.step(&mut drv);
    step_until_thinking(&mut host, &mut drv);
    // Turno nuevo: una petición pendiente, que se contesta con `say`.
    let (_, first_after) = the_pending_request(&mut host);
    drv.push_event(Reply::say(agent, first_after, "after").to_event());
    host.step(&mut drv);
    step_until_thinking(&mut host, &mut drv);
    // Segunda petición del turno nuevo, pendiente. Llega la respuesta vieja.
    let (_, current) = the_pending_request(&mut host);
    drv.push_event(late.to_event());
    host.step(&mut drv);
    assert_ne!(
        host.result(agent).as_deref(),
        Some("OLD"),
        "a reply produced before the restore must not complete the new turn"
    );
    assert_eq!(host.agent_state(agent), Some(AgentState::Thinking));
    assert_eq!(
        the_pending_request(&mut host),
        (agent.0, current),
        "the new turn's request must still be pending"
    );

    // El turno nuevo acaba con su propia respuesta.
    drv.push_event(Reply::done(agent, current, "NEW").to_event());
    host.step(&mut drv);
    assert_eq!(host.agent_state(agent), Some(AgentState::Done));
    assert_eq!(host.result(agent).as_deref(), Some("NEW"));
}
