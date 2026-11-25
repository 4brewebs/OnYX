//! `RecordingBridge` y el conjunto `dispatched` tras un restore sobre el MISMO driver.
//!
//! Contrato: tras `request_restore`, el grafo vuelve a tener pendiente una petición
//! `(agent, request)` que ya se despachó antes del restore. Las respuestas en vuelo no
//! forman parte del snapshot (rustdoc de `AgentHost::request_restore`), así que la petición
//! restaurada es una petición NUEVA para el bridge y debe volver a despacharse. El rustdoc
//! de `RecordingBridge::starting_at` habla de seguir grabando «tras un restore».
//!
//! Solo API pública de `onyx-soul-agents`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSpec, AgentState, AgentView, OnyxError, RecordingBridge, Reply,
    SnapshotStore,
};

type Blobs = Rc<RefCell<BTreeMap<String, Vec<u8>>>>;

#[derive(Clone, Default)]
struct Store(Blobs);

impl SnapshotStore for Store {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        self.0.borrow_mut().insert(key.into(), bytes.to_vec());
        Ok(())
    }
    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> {
        self.0.borrow().get(key).cloned().ok_or(OnyxError::Missing)
    }
}

type Calls = Rc<RefCell<Vec<(u64, u64)>>>;

/// Responder que cuenta llamadas: request 1 -> `say`, request >= 2 -> `done`.
fn counting_responder(calls: Calls) -> impl FnMut(&AgentView) -> Option<Reply> {
    move |req: &AgentView| {
        calls.borrow_mut().push((req.agent.0, req.request));
        Some(if req.request == 1 {
            Reply::say(req.agent, req.request, "thinking")
        } else {
            Reply::done(req.agent, req.request, format!("done@{}", req.request))
        })
    }
}

fn host(store: Store) -> AgentHost {
    let mut host = AgentHost::builder(7)
        .config(AgentConfig::default())
        .store(Box::new(store))
        .build()
        .unwrap();
    // Plazo enorme: si el agente termina, es porque le respondieron, no por timeout.
    host.spawn(
        &AgentSpec::new("worker", "goal")
            .with_max_steps(10)
            .with_timeout_ticks(10_000),
    );
    host
}

/// Graba hasta el reposo con un snapshot tomado en el primer tick, y devuelve el host,
/// el bridge usado y las llamadas vistas.
fn run_first_session(
    store: &Store,
    calls: &Calls,
) -> (
    AgentHost,
    RecordingBridge<impl FnMut(&AgentView) -> Option<Reply>>,
) {
    let mut host = host(store.clone());
    let mut bridge = RecordingBridge::new(host.vocab(), counting_responder(calls.clone()));
    host.request_snapshot("cut");
    host.step(&mut bridge);
    assert!(
        store.0.borrow().contains_key("cut"),
        "snapshot must be saved"
    );
    assert!(
        host.run_until_settled(&mut bridge, 50),
        "first session settles"
    );
    let agent = host.agents()[0];
    assert_eq!(host.agent_state(agent), Some(AgentState::Done));
    assert_eq!(host.result(agent).as_deref(), Some("done@2"));
    assert_eq!(*calls.borrow(), vec![(agent.0, 1), (agent.0, 2)]);
    (host, bridge)
}

/// Control: con un bridge NUEVO tras el restore, la petición restaurada se despacha y el
/// agente vuelve a terminar. Demuestra que el escenario es válido.
#[test]
fn restore_with_fresh_bridge_redispatches_restored_request() {
    let store = Store::default();
    let calls: Calls = Rc::default();
    let (mut host, old_bridge) = run_first_session(&store, &calls);
    let agent = host.agents()[0];
    let polls = old_bridge.transcript().entries.len() as u64;
    drop(old_bridge);

    let mut fresh =
        RecordingBridge::new(host.vocab(), counting_responder(calls.clone())).starting_at(polls);
    host.request_restore("cut");
    host.step(&mut fresh);
    assert!(
        host.run_until_settled(&mut fresh, 50),
        "fresh bridge must settle after restore"
    );
    assert_eq!(host.agent_state(agent), Some(AgentState::Done));
    let after: Vec<(u64, u64)> = calls.borrow()[2..].to_vec();
    assert_eq!(
        after.first(),
        Some(&(agent.0, 1)),
        "calls after restore: {after:?}"
    );
}

/// Rewind en vivo: mismo host, mismo `RecordingBridge`. La petición `(agent, 1)` vuelve a
/// estar pendiente tras el restore y debe despacharse otra vez.
#[test]
#[ignore = "BUG: RecordingBridge::dispatched no se limpia tras restore; la petición restaurada (agent, request) se salta y el agente queda colgado (onyx-soul-agents/src/bridge.rs:356)"]
fn restore_with_same_bridge_redispatches_restored_request() {
    let store = Store::default();
    let calls: Calls = Rc::default();
    let (mut host, mut bridge) = run_first_session(&store, &calls);
    let agent = host.agents()[0];

    host.request_restore("cut");
    host.step(&mut bridge);
    assert_ne!(
        host.agent_state(agent),
        Some(AgentState::Done),
        "restore must bring back the pending request"
    );
    let settled = host.run_until_settled(&mut bridge, 50);
    let after: Vec<(u64, u64)> = calls.borrow()[2..].to_vec();
    assert_eq!(
        after.first(),
        Some(&(agent.0, 1)),
        "restored request must be dispatched again; calls after restore: {after:?}"
    );
    assert!(settled, "agent must settle after restore");
    assert_eq!(host.agent_state(agent), Some(AgentState::Done));
}
