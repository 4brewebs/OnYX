//! Host de este soul: fingerprint, replay, restore y superficies.

use std::cell::RefCell;
use std::rc::Rc;

use onyx_soul_agents::{
    AgentHost, AgentSpec, AgentState, AgentView, AttrRegistry, Ctx, HeadlessDriver, OnyxError,
    OnyxEvent, OnyxSystem, RecordingBridge, ReplayBridge, Reply, SurfaceBinding, SurfaceId,
    SystemEvent, TOOLS_NAME, ToolCall, ToolCtx, ToolExecutor,
};

struct TickLog {
    seen: Rc<RefCell<Vec<u64>>>,
}

impl OnyxSystem for TickLog {
    fn name(&self) -> &str {
        "tick_log"
    }

    fn init(&mut self, _: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        for event in ctx.inbox {
            if let OnyxEvent::System(SystemEvent::Tick { n }) = event {
                self.seen.borrow_mut().push(*n);
            }
        }
    }
}

struct Search;

impl ToolExecutor for Search {
    fn name(&self) -> &str {
        "search"
    }

    fn call(&mut self, call: &ToolCall, _: &mut ToolCtx) -> Result<String, String> {
        Ok(format!("{}:ok", call.tool))
    }
}

fn spec(name: &str) -> AgentSpec {
    AgentSpec::new(name, "research")
        .with_max_steps(6)
        .with_timeout_ticks(8)
}

fn done_on_request(req: &AgentView) -> Option<Reply> {
    Some(Reply::done(req.agent, req.request, "ok"))
}

fn settled_run(
    seed: u64,
    name: &str,
) -> (onyx_soul_agents::Fingerprint, onyx_soul_agents::Transcript) {
    let mut host = AgentHost::builder(seed).build().unwrap();
    host.spawn(&spec(name));
    let mut rec = RecordingBridge::new(host.vocab(), done_on_request);
    assert!(
        host.run_until_settled(&mut rec, 32),
        "un done del puente tiene que asentar al agente"
    );
    (host.fingerprint(), rec.transcript().clone())
}

#[test]
fn without_agents_the_host_counts_as_settled() {
    let host = AgentHost::builder(1).build().unwrap();
    assert!(host.agents().is_empty());
    assert!(
        host.is_settled(),
        "sin agentes no queda trabajo: settled debe ser true"
    );
    assert_eq!(host.ticks(), 0);
}

#[test]
fn spawn_and_done_settle_with_a_result() {
    let mut host = AgentHost::builder(2).build().unwrap();
    host.spawn(&spec("alice"));
    assert!(
        host.is_settled(),
        "el spawn aún no se ha procesado: no hay agentes"
    );
    let mut rec = RecordingBridge::new(host.vocab(), done_on_request);
    assert!(host.run_until_settled(&mut rec, 32));
    let id = host.agents()[0];
    assert_eq!(host.agent_state(id), Some(AgentState::Done));
    assert_eq!(host.result(id).as_deref(), Some("ok"));
    assert!(host.ticks() > 0);
}

#[test]
fn identical_inputs_produce_the_same_fingerprint() {
    let (first, _) = settled_run(7, "alice");
    let (second, _) = settled_run(7, "alice");
    assert_eq!(first, second);
    let (other, _) = settled_run(7, "bob");
    assert_ne!(
        first, other,
        "un agente con otro nombre tiene que cambiar el estado observable"
    );
}

#[test]
fn replay_through_the_host_matches_the_recording() {
    let (recorded, transcript) = settled_run(11, "alice");
    let mut host = AgentHost::builder(11).build().unwrap();
    host.spawn(&spec("alice"));
    let mut replay = ReplayBridge::new(transcript);
    assert!(host.run_until_settled(&mut replay, 32));
    assert_eq!(host.fingerprint(), recorded);
    assert_eq!(replay.remaining(), 0);
}

#[test]
fn restore_returns_the_graph_to_the_snapshotted_state() {
    let mut host = AgentHost::builder(3).build().unwrap();
    host.spawn(&spec("alice"));
    let mut idle = HeadlessDriver::new();
    host.step(&mut idle);
    let id = host.agents()[0];
    assert_eq!(host.agent_state(id), Some(AgentState::Thinking));
    host.request_snapshot("mid");
    host.step(&mut idle);
    assert_eq!(host.agent_state(id), Some(AgentState::Thinking));
    let mid = host.fingerprint();

    let mut rec = RecordingBridge::new(host.vocab(), done_on_request);
    assert!(host.run_until_settled(&mut rec, 32));
    assert_eq!(host.agent_state(id), Some(AgentState::Done));
    assert_ne!(host.fingerprint(), mid);

    host.request_restore("mid");
    host.step(&mut idle);
    assert_eq!(host.agent_state(id), Some(AgentState::Thinking));
    assert_eq!(host.fingerprint(), mid);
}

#[test]
fn step_with_forwards_surfaces_to_the_driver() {
    let mut host = AgentHost::builder(4).build().unwrap();
    let view = host.attr("view.main");
    let binding = SurfaceBinding {
        surface: SurfaceId(2),
        view,
        focused: true,
    };
    let headless = HeadlessDriver::new();
    let handle = headless.handle();
    let mut driver = headless;
    host.step_with(&mut driver, &[binding]);
    assert_eq!(handle.present_count(), 1);
    assert_eq!(handle.last_surfaces(), [binding]);

    host.step(&mut driver);
    assert_eq!(handle.present_count(), 2);
    assert!(
        handle.last_surfaces().is_empty(),
        "step sin bindings tiene que pasar un slice vacío"
    );
}

#[test]
fn builder_registers_tools_and_they_run() {
    let mut host = AgentHost::builder(5)
        .tool(Box::new(Search))
        .build()
        .unwrap();
    assert!(
        host.system_names().contains(&TOOLS_NAME),
        "con herramientas el host tiene que montar ToolSystem"
    );
    host.spawn(&spec("alice").with_tools(["search"]));
    let mut rec = RecordingBridge::new(host.vocab(), |req: &AgentView| {
        if req.memory.iter().any(|(kind, _)| kind == "observe") {
            Some(Reply::done(req.agent, req.request, "fin"))
        } else {
            Some(Reply::call(req.agent, req.request, "search q"))
        }
    });
    assert!(host.run_until_settled(&mut rec, 32));
    let id = host.agents()[0];
    assert_eq!(host.agent_state(id), Some(AgentState::Done));
    assert_eq!(host.result(id).as_deref(), Some("fin"));
}

#[test]
fn mem_store_overwrites_a_key_and_misses_unknown_ones() {
    let mut store = onyx_soul_agents::MemStore::new();
    use onyx_soul_agents::SnapshotStore;
    store.save("k", b"one").unwrap();
    store.save("k", b"two").unwrap();
    assert_eq!(store.load("k").unwrap(), b"two");
    assert!(matches!(
        store.load("missing"),
        Err(onyx_soul_agents::OnyxError::Missing)
    ));
}

#[test]
fn set_tick_sets_the_n_of_the_next_step() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut host = AgentHost::builder(1)
        .system(Box::new(TickLog { seen: seen.clone() }))
        .build()
        .unwrap();
    let mut idle = HeadlessDriver::new();
    host.step(&mut idle);
    host.step(&mut idle);
    assert_eq!(*seen.borrow(), [0, 1]);
    assert_eq!(host.ticks(), 2);

    host.set_tick(7);
    assert_eq!(host.ticks(), 7);
    host.step(&mut idle);
    assert_eq!(
        *seen.borrow(),
        [0, 1, 7],
        "set_tick tiene que cambiar el n que ve el próximo tick, no el que ya se guardó"
    );
    assert_eq!(host.ticks(), 8);
}

fn say_twice_then_done(req: &AgentView) -> Option<Reply> {
    let says = req.memory.iter().filter(|(kind, _)| kind == "say").count();
    match says {
        0 => Some(Reply::say(req.agent, req.request, "thinking")),
        1 => Some(Reply::say(req.agent, req.request, "more")),
        _ => Some(Reply::done(req.agent, req.request, "ok")),
    }
}

fn host_replayed_to_mid(
    seed: u64,
    transcript: onyx_soul_agents::Transcript,
) -> (AgentHost, ReplayBridge) {
    let mut host = AgentHost::builder(seed).build().unwrap();
    host.spawn(&spec("alice"));
    let mut replay = ReplayBridge::new(transcript);
    host.step(&mut replay);
    host.request_snapshot("mid");
    host.step(&mut replay);
    (host, replay)
}

#[test]
fn set_tick_and_a_new_replay_bridge_continue_like_an_uninterrupted_run() {
    // Tras el snapshot el primer Say ya está en memoria y el runtime ya
    // abrió la siguiente petición. Un Done no escribe mem.tick ni usa el
    // rng; el segundo Say sí graba el now, así que el fingerprint exige
    // el n que fija set_tick.
    let mut recorded = AgentHost::builder(13).build().unwrap();
    recorded.spawn(&spec("alice"));
    let mut rec = RecordingBridge::new(recorded.vocab(), say_twice_then_done);
    recorded.step(&mut rec);
    recorded.request_snapshot("mid");
    recorded.step(&mut rec);
    let n = recorded.ticks();
    let mid = recorded.fingerprint();
    assert!(recorded.run_until_settled(&mut rec, 32));
    let done = recorded.fingerprint();
    let transcript = rec.transcript().clone();
    assert_ne!(
        mid, done,
        "el snapshot tiene que pillarse antes de terminar, si no el restore no demuestra nada"
    );

    let (mut replayed, mut replay) = host_replayed_to_mid(13, transcript.clone());
    assert_eq!(replayed.ticks(), n);
    assert_eq!(replayed.fingerprint(), mid);
    assert!(replayed.run_until_settled(&mut replay, 32));
    assert_eq!(replayed.fingerprint(), done);

    // Restore en un host que no ha pasado de mid: import conserva el
    // high_water más alto de esta ejecución, y un create posterior no
    // reutilizaría el id del segundo recuerdo.
    let (mut host, _) = host_replayed_to_mid(13, transcript.clone());
    host.request_restore("mid");
    let mut idle = HeadlessDriver::new();
    host.step(&mut idle);
    assert_eq!(host.fingerprint(), mid);
    assert_ne!(
        host.ticks(),
        n,
        "request_restore no debe realinear n: ese tick corre sobre el grafo anterior"
    );

    host.set_tick(n);
    let mut replay = ReplayBridge::new(transcript.clone()).starting_at(n);
    assert!(host.run_until_settled(&mut replay, 32));
    assert_eq!(host.fingerprint(), done);

    let (mut drifted, _) = host_replayed_to_mid(13, transcript.clone());
    drifted.request_restore("mid");
    drifted.step(&mut HeadlessDriver::new());
    let mut replay = ReplayBridge::new(transcript).starting_at(n);
    assert!(drifted.run_until_settled(&mut replay, 32));
    assert_ne!(
        drifted.fingerprint(),
        done,
        "sin set_tick el segundo Say tiene que grabar otro mem.tick"
    );
}
