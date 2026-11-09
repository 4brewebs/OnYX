//! Record/replay del puente: el decisor queda fuera del cerebro.

use std::cell::Cell;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::rc::Rc;

use onyx_soul_agents::{
    AgentBus, AgentConfig, AgentGraph, AgentRuntime, AgentSpawner, AgentSpec, AgentState,
    AgentView, ConceptId, EventBus, Graph, GraphView, Logged, OnyxError, OnyxEvent, OnyxSystem,
    PlatformDriver, RecordingBridge, ReplayBridge, Reply, SnapshotStore, Supervisor,
    SurfaceBinding, ToolCall, ToolCtx, ToolExecutor, ToolSystem, Transcript, Value, Vocab,
    pending_requests, spawn_event, tick,
};

struct Search;

impl ToolExecutor for Search {
    fn name(&self) -> &str {
        "search"
    }

    fn call(&mut self, call: &ToolCall, _: &mut ToolCtx) -> Result<String, String> {
        Ok(format!("{}:ok", call.tool))
    }
}

struct NullStore;

impl SnapshotStore for NullStore {
    fn save(&mut self, _: &str, _: &[u8]) -> Result<(), OnyxError> {
        Ok(())
    }

    fn load(&self, _: &str) -> Result<Vec<u8>, OnyxError> {
        Err(OnyxError::Missing)
    }
}

#[derive(Default)]
struct InjectingDriver {
    events: Vec<OnyxEvent>,
}

impl PlatformDriver for InjectingDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        for event in self.events.drain(..) {
            bus.push(event);
        }
    }

    fn present(&mut self, _: &dyn GraphView, _: &[SurfaceBinding]) {}
}

struct World {
    graph: AgentGraph,
    bus: AgentBus,
    store: NullStore,
    systems: Vec<Box<dyn OnyxSystem>>,
    sup: Supervisor,
    v: Vocab,
    seed: u64,
}

impl World {
    fn new(seed: u64) -> Self {
        Self::build(seed, false)
    }

    fn with_search(seed: u64) -> Self {
        Self::build(seed, true)
    }

    fn build(seed: u64, search: bool) -> Self {
        let cfg = AgentConfig::default();
        let mut graph = AgentGraph::new();
        let mut systems: Vec<Box<dyn OnyxSystem>> = vec![
            Box::new(AgentSpawner::new(cfg.clone())),
            Box::new(AgentRuntime::new(cfg.clone())),
        ];
        if search {
            systems.push(Box::new(
                ToolSystem::new(cfg.clone()).with_tool(Box::new(Search)),
            ));
        }
        let mut sup = Supervisor::new(systems.len());
        sup.init_all(&mut systems, &mut graph);
        for i in 0..systems.len() {
            assert!(sup.start(i));
        }
        let v = Vocab::resolve_with(&mut graph, &cfg.vocab_prefix);
        Self {
            graph,
            bus: AgentBus::new(),
            store: NullStore,
            systems,
            sup,
            v,
            seed,
        }
    }

    fn spawn(&mut self, spec: &AgentSpec) {
        self.bus.push(spawn_event(&self.v, spec, None));
    }

    fn step(&mut self, driver: &mut dyn PlatformDriver, n: u64) {
        tick(
            n,
            self.seed,
            driver,
            &mut self.graph,
            &mut self.store,
            &mut self.bus,
            &mut self.systems,
            self.sup.states_mut(),
            &[],
        );
    }

    fn run(&mut self, driver: &mut dyn PlatformDriver, from: u64, ticks: u64) {
        for n in from..from + ticks {
            self.step(driver, n);
        }
    }

    fn run_until_settled(&mut self, driver: &mut dyn PlatformDriver, max: u64) -> bool {
        for n in 0..max {
            if !self.agents().is_empty() && self.settled() {
                return true;
            }
            self.step(driver, n);
        }
        self.settled()
    }

    fn settled(&self) -> bool {
        let agents = self.agents();
        !agents.is_empty()
            && agents
                .iter()
                .all(|id| matches!(self.state(*id), AgentState::Done | AgentState::Failed))
    }

    fn agents(&self) -> Vec<ConceptId> {
        self.graph.query(&[self.v.state])
    }

    fn state(&self, id: ConceptId) -> AgentState {
        match self.graph.get(id, self.v.state) {
            Some(Value::Text(s)) => AgentState::parse(&s).expect("estado conocido"),
            other => panic!("sin estado: {other:?}"),
        }
    }

    fn result(&self, id: ConceptId) -> Option<String> {
        match self.graph.get(id, self.v.result) {
            Some(Value::Text(s)) => Some(s),
            _ => None,
        }
    }

    fn memory(&self, id: ConceptId) -> Vec<(String, String)> {
        self.graph
            .related(id, self.v.rel_memory)
            .into_iter()
            .map(|m| {
                let kind = match self.graph.get(m, self.v.mem_kind) {
                    Some(Value::Text(s)) => s,
                    other => panic!("mem_kind: {other:?}"),
                };
                let text = match self.graph.get(m, self.v.mem_text) {
                    Some(Value::Text(s)) => s,
                    other => panic!("mem_text: {other:?}"),
                };
                (kind, text)
            })
            .collect()
    }
}

#[test]
fn pending_requests_project_a_thinking_agent_with_goal_tools_and_memory() {
    let mut world = World::new(1);
    world.spawn(&AgentSpec::new("bob", "summarize").with_tools(["search", "read"]));
    let mut driver = RecordingBridge::new(world.v, |_: &AgentView| None);
    world.step(&mut driver, 0);

    let requests = pending_requests(&world.graph, &world.v);
    assert_eq!(requests.len(), 1);
    let r = &requests[0];
    assert_eq!(
        (r.name.as_str(), r.goal.as_str(), r.request),
        ("bob", "summarize", 1)
    );
    assert_eq!(r.tools, ["search", "read"]);
    assert!(r.memory.is_empty());
    assert!(r.props.iter().any(|(k, _)| k == "agent.state"));
}

#[test]
fn external_decider_can_drive_a_tool_then_finish() {
    let mut world = World::with_search(1);
    world.spawn(&AgentSpec::new("alice", "find the answer").with_tools(["search"]));
    let mut bridge = RecordingBridge::new(world.v, |req: &AgentView| {
        Some(match req.request {
            1 => Reply::call(req.agent, 1, "search rust determinism"),
            n => Reply::done(req.agent, n, "42"),
        })
    });
    assert!(world.run_until_settled(&mut bridge, 20));

    let id = world.agents()[0];
    assert_eq!(world.state(id), AgentState::Done);
    assert_eq!(world.result(id).as_deref(), Some("42"));
    assert_eq!(
        world.memory(id),
        [
            ("call".to_string(), "search rust determinism".to_string()),
            ("observe".to_string(), "search:ok".to_string())
        ]
    );
}

#[test]
fn nondeterministic_decider_is_reproduced_exactly_from_its_transcript() {
    let spec = |name: &str| AgentSpec::new(name, "explore").with_tools(["search"]);

    let mut world = World::with_search(5);
    world.spawn(&spec("x"));
    world.spawn(&spec("y"));
    let mut recorder = RecordingBridge::new(world.v, |req: &AgentView| {
        let roll = RandomState::new().build_hasher().finish();
        Some(if req.request >= 3 {
            Reply::done(req.agent, req.request, "enough")
        } else {
            Reply::call(req.agent, req.request, format!("search q{roll}"))
        })
    });
    assert!(world.run_until_settled(&mut recorder, 30));
    let recorded = world.graph.export();
    let transcript = recorder.transcript().clone();
    assert_eq!(transcript.entries.len(), 6, "2 agentes x 3 respuestas");
    assert!(
        transcript
            .entries
            .windows(2)
            .all(|w| w[0].poll <= w[1].poll),
        "los poll quedan en orden no decreciente"
    );

    let transcript = Transcript::from_bytes(&transcript.to_bytes()).unwrap();

    let mut replay_world = World::with_search(5);
    replay_world.spawn(&spec("x"));
    replay_world.spawn(&spec("y"));
    let mut replay = ReplayBridge::new(transcript);
    assert_eq!(replay.remaining(), 6, "nada se ha reinyectado todavía");
    assert!(replay_world.run_until_settled(&mut replay, 30));

    assert_eq!(replay.remaining(), 0);
    assert_eq!(replay_world.graph.export(), recorded);
}

#[test]
fn bridges_wrap_another_driver_without_hiding_its_events() {
    let mut world = World::new(1);
    world.spawn(&AgentSpec::new("alice", "g"));
    let mut inner = InjectingDriver::default();
    world.step(&mut inner, 0);
    let id = world.agents()[0];
    assert_eq!(world.state(id), AgentState::Thinking);
    inner
        .events
        .push(Reply::done(id, 1, "via inner").to_event());

    let mut bridge = RecordingBridge::new(world.v, |_: &AgentView| None).wrapping(inner);
    world.run(&mut bridge, 1, 3);
    assert_eq!(world.state(id), AgentState::Done);
    assert_eq!(world.result(id).as_deref(), Some("via inner"));
}

#[test]
fn unanswered_request_fails_by_timeout_in_ticks() {
    let mut world = World::new(1);
    world.spawn(&AgentSpec::new("slow", "wait").with_timeout_ticks(2));
    let mut bridge = RecordingBridge::new(world.v, |_: &AgentView| None);
    world.step(&mut bridge, 0);
    let id = world.agents()[0];
    assert_eq!(world.state(id), AgentState::Thinking);
    world.run(&mut bridge, 1, 2);
    assert_eq!(
        world.state(id),
        AgentState::Thinking,
        "timeout=2 deja remaining=3; tras dos ticks vacíos aún espera"
    );
    world.step(&mut bridge, 3);
    assert_eq!(world.state(id), AgentState::Failed);
    assert_eq!(world.result(id).as_deref(), Some("timeout"));
}

#[test]
fn a_pending_request_is_dispatched_to_the_responder_only_once() {
    let calls = Rc::new(Cell::new(0u32));
    let seen = calls.clone();
    let mut world = World::new(1);
    world.spawn(&AgentSpec::new("once", "g").with_timeout_ticks(4));
    let mut bridge = RecordingBridge::new(world.v, move |_: &AgentView| {
        seen.set(seen.get() + 1);
        None
    });
    world.run(&mut bridge, 0, 5);
    assert_eq!(
        calls.get(),
        1,
        "si el decisor no contesta, no se le vuelve a preguntar la misma petición"
    );
}

#[test]
fn replay_does_not_consult_a_decider() {
    let mut world = World::new(1);
    world.spawn(&AgentSpec::new("alice", "g"));
    let mut recorder = RecordingBridge::new(world.v, |req: &AgentView| {
        Some(Reply::done(req.agent, req.request, "recorded"))
    });
    assert!(world.run_until_settled(&mut recorder, 10));
    let recorded = world.graph.export();
    let transcript = recorder.transcript().clone();
    assert_eq!(transcript.entries.len(), 1);
    assert_eq!(
        transcript.entries[0].poll, 1,
        "un tick por poll: la respuesta entra en el tick 1"
    );

    let mut replay_world = World::new(1);
    replay_world.spawn(&AgentSpec::new("alice", "g"));
    let mut replay = ReplayBridge::new(transcript);
    assert_eq!(replay.remaining(), 1, "nada se ha reinyectado todavía");
    assert!(replay_world.run_until_settled(&mut replay, 10));
    assert_eq!(replay.remaining(), 0);
    assert_eq!(replay_world.graph.export(), recorded);
    assert_eq!(
        replay_world.result(replay_world.agents()[0]).as_deref(),
        Some("recorded")
    );
}

#[test]
fn transcript_codec_round_trips_and_rejects_corruption() {
    let t = Transcript {
        entries: vec![
            Logged {
                poll: 1,
                reply: Reply::call(ConceptId(2), 1, "search a\tb"),
            },
            Logged {
                poll: 1,
                reply: Reply::say(ConceptId(3), 1, "same poll"),
            },
            Logged {
                poll: 9,
                reply: Reply::done(ConceptId(2), 2, "fin\n"),
            },
        ],
    };
    let bytes = t.to_bytes();
    assert_eq!(Transcript::from_bytes(&bytes).unwrap(), t);
    assert_eq!(
        Transcript::from_bytes(&Transcript::default().to_bytes()).unwrap(),
        Transcript::default()
    );

    assert!(Transcript::from_bytes(b"").is_err());
    assert!(Transcript::from_bytes(b"NOPE").is_err());
    let magic_len = b"ONYXTR1\n".len();
    let mut ok_cuts = vec![magic_len];
    let mut pos = magic_len;
    for e in &t.entries {
        pos += 12 + e.reply.to_bytes().len();
        ok_cuts.push(pos);
    }
    for cut in 0..bytes.len() {
        let r = Transcript::from_bytes(&bytes[..cut]);
        if let Some(i) = ok_cuts.iter().position(|&p| p == cut) {
            assert_eq!(r.unwrap().entries.len(), i, "prefijo válido en {cut}");
        } else {
            assert!(r.is_err(), "truncado en {cut} debe fallar");
        }
    }
    let mut huge = b"ONYXTR1\n".to_vec();
    huge.extend_from_slice(&0u64.to_le_bytes());
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(Transcript::from_bytes(&huge).is_err());
    let swapped = Transcript {
        entries: vec![t.entries[2].clone(), t.entries[0].clone()],
    };
    assert!(Transcript::from_bytes(&swapped.to_bytes()).is_err());
}
