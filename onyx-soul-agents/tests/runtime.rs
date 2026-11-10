//! Runtime de agentes: máquina de estados, `Reply`, memoria, plazos y políticas.
//! Solo se usa la API pública del soul.

use std::cell::RefCell;
use std::rc::Rc;

use onyx_soul_agents::{
    AgentConfig, AgentGraph, AgentRuntime, AgentSpawner, AgentSpec, AgentState, AgentView, Attr,
    AttrOnly, AttrRegistry, ConceptId, Ctx, Decision, DetRng, DomainEvent, Graph, GraphView,
    IngressEvent, MemoryEntry, MemoryPolicy, OnyxEvent, OnyxSystem, Policy, REPLY_SOURCE, Reply,
    ScriptedPolicy, SystemEvent, UniformTool, Value, Vocab, spawn_event,
};

struct World {
    g: AgentGraph,
    v: Vocab,
    spawner: AgentSpawner,
    runtime: AgentRuntime,
    n: u64,
    seed: u64,
}

fn cfg() -> AgentConfig {
    AgentConfig {
        default_max_steps: 8,
        default_timeout_ticks: 4,
        ..AgentConfig::default()
    }
}

impl World {
    fn new(cfg: AgentConfig) -> Self {
        Self::with_runtime(cfg.clone(), AgentRuntime::new(cfg))
    }

    fn with_runtime(cfg: AgentConfig, mut runtime: AgentRuntime) -> Self {
        let mut g = AgentGraph::new();
        let mut spawner = AgentSpawner::new(cfg.clone());
        spawner.init(&mut AttrOnly(&mut g)).unwrap();
        runtime.init(&mut AttrOnly(&mut g)).unwrap();
        let v = Vocab::resolve_with(&mut g, &cfg.vocab_prefix);
        Self {
            g,
            v,
            spawner,
            runtime,
            n: 0,
            seed: 1,
        }
    }

    fn run(&mut self, extra: Vec<OnyxEvent>) -> Vec<OnyxEvent> {
        self.n += 1;
        let mut inbox = vec![OnyxEvent::System(SystemEvent::Tick { n: self.n })];
        inbox.extend(extra);
        self.run_inbox(&inbox)
    }

    fn run_inbox(&mut self, inbox: &[OnyxEvent]) -> Vec<OnyxEvent> {
        let mut outbox = Vec::new();
        let mut rng = DetRng::from_seed_and_tick(self.seed, self.n);
        self.spawner.run(&mut Ctx {
            graph: &mut self.g,
            inbox,
            outbox: &mut outbox,
            rng: &mut rng,
        });
        self.runtime.run(&mut Ctx {
            graph: &mut self.g,
            inbox,
            outbox: &mut outbox,
            rng: &mut rng,
        });
        outbox
    }

    fn spawn(&mut self, spec: &AgentSpec) -> ConceptId {
        let ev = spawn_event(&self.v, spec, None);
        let out = self.run(vec![ev]);
        out.iter()
            .find_map(|e| match e {
                OnyxEvent::Domain(d) if d.kind == self.v.ev_spawned => d.subject,
                _ => None,
            })
            .expect("el spawner debía emitir ev_spawned")
    }

    fn spawn_only(&mut self, spec: &AgentSpec) -> ConceptId {
        self.n += 1;
        let inbox = [
            OnyxEvent::System(SystemEvent::Tick { n: self.n }),
            spawn_event(&self.v, spec, None),
        ];
        let mut outbox = Vec::new();
        let mut rng = DetRng::from_seed_and_tick(self.seed, self.n);
        self.spawner.run(&mut Ctx {
            graph: &mut self.g,
            inbox: &inbox,
            outbox: &mut outbox,
            rng: &mut rng,
        });
        outbox
            .iter()
            .find_map(|e| match e {
                OnyxEvent::Domain(d) if d.kind == self.v.ev_spawned => d.subject,
                _ => None,
            })
            .expect("el spawner debía emitir ev_spawned")
    }

    fn state(&self, id: ConceptId) -> AgentState {
        match self.g.get(id, self.v.state) {
            Some(Value::Text(s)) => AgentState::parse(&s).expect("estado conocido"),
            other => panic!("sin estado: {other:?}"),
        }
    }

    fn int(&self, id: ConceptId, attr: Attr) -> i64 {
        match self.g.get(id, attr) {
            Some(Value::Int(n)) => n,
            other => panic!("se esperaba Int, llegó {other:?}"),
        }
    }

    fn text(&self, id: ConceptId, attr: Attr) -> String {
        match self.g.get(id, attr) {
            Some(Value::Text(s)) => s,
            other => panic!("se esperaba Text, llegó {other:?}"),
        }
    }

    fn memory(&self, id: ConceptId) -> Vec<(String, String)> {
        self.g
            .related(id, self.v.rel_memory)
            .into_iter()
            .map(|m| (self.text(m, self.v.mem_kind), self.text(m, self.v.mem_text)))
            .collect()
    }
}

fn faults(out: &[OnyxEvent]) -> Vec<String> {
    out.iter()
        .filter_map(|e| match e {
            OnyxEvent::System(SystemEvent::Fault(m)) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

fn payload_text(d: &DomainEvent, key: Attr) -> Option<String> {
    d.data.iter().rev().find_map(|(k, val)| match val {
        Value::Text(t) if *k == key => Some(t.clone()),
        _ => None,
    })
}

fn finished(v: &Vocab, out: &[OnyxEvent]) -> Vec<(ConceptId, String)> {
    out.iter()
        .filter_map(|e| match e {
            OnyxEvent::Domain(d) if d.kind == v.ev_finished => Some((
                d.subject.expect("subject"),
                payload_text(d, v.text).unwrap_or_default(),
            )),
            _ => None,
        })
        .collect()
}

fn failed(v: &Vocab, out: &[OnyxEvent]) -> Vec<(ConceptId, String)> {
    out.iter()
        .filter_map(|e| match e {
            OnyxEvent::Domain(d) if d.kind == v.ev_failed => Some((
                d.subject.expect("subject"),
                payload_text(d, v.text).unwrap_or_default(),
            )),
            _ => None,
        })
        .collect()
}

fn tool_calls(v: &Vocab, out: &[OnyxEvent]) -> Vec<(ConceptId, String, String)> {
    out.iter()
        .filter_map(|e| match e {
            OnyxEvent::Domain(d) if d.kind == v.ev_tool_call => Some((
                d.subject.expect("subject"),
                payload_text(d, v.tool).unwrap_or_default(),
                payload_text(d, v.args).unwrap_or_default(),
            )),
            _ => None,
        })
        .collect()
}

fn tool_result(
    v: &Vocab,
    agent: Option<ConceptId>,
    request: Value,
    extra: Vec<(Attr, Value)>,
) -> OnyxEvent {
    let mut data = vec![(v.request, request)];
    data.extend(extra);
    OnyxEvent::Domain(DomainEvent {
        kind: v.ev_tool_result,
        subject: agent,
        data,
    })
}

#[test]
fn external_agent_enters_thinking_with_pending_and_relative_remaining() {
    let mut w = World::new(AgentConfig {
        default_timeout_ticks: 5,
        ..cfg()
    });
    let id = w.spawn(&AgentSpec::new("alice", "resumir").with_timeout_ticks(5));
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.steps), 1);
    assert_eq!(w.int(id, w.v.pending), 1);
    assert_eq!(
        w.int(id, w.v.remaining),
        6,
        "remaining es timeout + 1 para que la espera que empieza este tick no caduque al instante"
    );
    assert_eq!(w.text(id, w.v.goal), "resumir");
}

#[test]
fn say_is_remembered_and_the_next_request_opens_in_the_same_tick() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g"));
    let out = w.run(vec![Reply::say(id, 1, "hmm").to_event()]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert_eq!(w.memory(id), [("say".into(), "hmm".into())]);
    assert_eq!(
        w.state(id),
        AgentState::Thinking,
        "tras Say vuelve a Idle y planifica otra vez"
    );
    assert_eq!(w.int(id, w.v.pending), 2);
    assert_eq!(w.int(id, w.v.steps), 2);
}

#[test]
fn done_and_fail_emit_finished_or_failed_with_the_text_key() {
    let mut w = World::new(cfg());
    let done_id = w.spawn(&AgentSpec::new("ok", "g"));
    let out = w.run(vec![Reply::done(done_id, 1, "listo").to_event()]);
    assert_eq!(w.state(done_id), AgentState::Done);
    assert_eq!(w.int(done_id, w.v.pending), 0);
    assert_eq!(w.text(done_id, w.v.result), "listo");
    assert_eq!(finished(&w.v, &out), [(done_id, "listo".into())]);
    assert!(failed(&w.v, &out).is_empty());

    let fail_id = w.spawn(&AgentSpec::new("ko", "g"));
    let out = w.run(vec![Reply::fail(fail_id, 1, "nope").to_event()]);
    assert_eq!(w.state(fail_id), AgentState::Failed);
    assert_eq!(w.text(fail_id, w.v.result), "nope");
    assert_eq!(failed(&w.v, &out), [(fail_id, "nope".into())]);
}

#[test]
fn stale_or_duplicate_replies_are_discarded_with_a_fault() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g"));

    let out = w.run(vec![Reply::say(id, 99, "late").to_event()]);
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.pending), 1);
    assert_eq!(w.memory(id), []);
    assert!(
        faults(&out).iter().any(|m| m.contains("stale reply")),
        "{out:?}"
    );

    let out = w.run(vec![Reply::say(id, 1, "hmm").to_event()]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert_eq!(w.memory(id), [("say".into(), "hmm".into())]);
    assert_eq!(w.int(id, w.v.pending), 2);

    let out = w.run(vec![Reply::say(id, 1, "hmm").to_event()]);
    assert_eq!(
        w.memory(id),
        [("say".into(), "hmm".into())],
        "la repetida no se aplica"
    );
    assert_eq!(w.int(id, w.v.pending), 2);
    assert!(
        faults(&out).iter().any(|m| m.contains("stale reply")),
        "{out:?}"
    );
}

#[test]
fn malformed_and_non_utf8_replies_are_faults_not_panics() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g"));

    let bad = OnyxEvent::Ingress(IngressEvent::Raw {
        source: REPLY_SOURCE.into(),
        bytes: b"nope".to_vec(),
    });
    let out = w.run(vec![bad]);
    assert_eq!(w.state(id), AgentState::Thinking);
    assert!(
        faults(&out)
            .iter()
            .any(|m| m.contains("agent reply rejected")),
        "{out:?}"
    );

    let invalid = OnyxEvent::Ingress(IngressEvent::Raw {
        source: REPLY_SOURCE.into(),
        bytes: vec![0xff, 0xfe],
    });
    let out = w.run(vec![invalid]);
    assert!(
        faults(&out)
            .iter()
            .any(|m| m.contains("agent reply rejected")),
        "{out:?}"
    );
}

#[test]
fn unexpected_verb_and_unknown_agent_are_faults() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g"));

    let out = w.run(vec![Reply::observe(id, 1, "??").to_event()]);
    assert_eq!(w.state(id), AgentState::Thinking);
    assert!(
        faults(&out).iter().any(|m| m.contains("unexpected")),
        "{out:?}"
    );

    let out = w.run(vec![Reply::say(ConceptId(9_999), 1, "x").to_event()]);
    assert!(
        faults(&out).iter().any(|m| m.contains("unknown agent")),
        "{out:?}"
    );
}

#[test]
fn a_reply_on_another_source_is_ignored() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g"));
    let bytes = Reply::say(id, 1, "hmm").to_bytes();
    let out = w.run(vec![OnyxEvent::Ingress(IngressEvent::Raw {
        source: "other.channel".into(),
        bytes,
    })]);
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.memory(id), []);
    assert!(faults(&out).is_empty(), "{out:?}");
}

#[test]
fn without_a_tick_the_runtime_does_not_apply_replies_or_advance_deadlines() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g"));
    let remaining = w.int(id, w.v.remaining);
    let out = w.run_inbox(&[Reply::say(id, 1, "hmm").to_event()]);
    assert!(out.is_empty(), "{out:?}");
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.remaining), remaining);
    assert_eq!(w.memory(id), []);
}

#[test]
fn unanswered_request_fails_by_timeout_in_ticks() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("slow", "wait").with_timeout_ticks(2));
    assert_eq!(w.int(id, w.v.remaining), 3);
    w.run(vec![]);
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.remaining), 2);
    w.run(vec![]);
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.remaining), 1);
    let out = w.run(vec![]);
    assert_eq!(w.state(id), AgentState::Failed);
    assert_eq!(w.text(id, w.v.result), "timeout");
    assert_eq!(failed(&w.v, &out), [(id, "timeout".into())]);
}

#[test]
fn a_wait_started_this_tick_is_not_immediately_expired() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("a", "g").with_timeout_ticks(1));
    assert_eq!(w.int(id, w.v.remaining), 2);
    assert_eq!(w.state(id), AgentState::Thinking);
    w.run(vec![]);
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.remaining), 1);
    let out = w.run(vec![]);
    assert_eq!(w.state(id), AgentState::Failed);
    assert_eq!(failed(&w.v, &out), [(id, "timeout".into())]);
}

#[test]
fn a_restored_snapshot_resumes_waiting_instead_of_timing_out() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("a", "g").with_timeout_ticks(2));
    assert_eq!(w.int(id, w.v.remaining), 3);
    let snapshot = w.g.export();

    w.run(vec![]);
    w.run(vec![]);
    w.run(vec![]);
    assert_eq!(w.state(id), AgentState::Failed);
    assert_eq!(w.text(id, w.v.result), "timeout");

    w.g.import(&snapshot).unwrap();
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.remaining), 3);
    w.run(vec![]);
    assert_eq!(
        w.state(id),
        AgentState::Thinking,
        "remaining es relativo: restaurar no caduca de golpe"
    );
    assert_eq!(w.int(id, w.v.remaining), 2);
}

#[test]
fn max_steps_is_checked_when_leaving_idle() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("a", "g").with_max_steps(1));
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.steps), 1);
    let out = w.run(vec![Reply::say(id, 1, "once").to_event()]);
    assert_eq!(w.memory(id), [("say".into(), "once".into())]);
    assert_eq!(w.state(id), AgentState::Failed);
    assert_eq!(w.text(id, w.v.result), "max_steps");
    assert_eq!(failed(&w.v, &out), [(id, "max_steps".into())]);
}

#[test]
fn scripted_without_tools_finishes_in_the_same_tick() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("bot", "g").with_policy(Policy::Scripted));
    assert_eq!(w.state(id), AgentState::Done);
    assert_eq!(w.text(id, w.v.result), "no_tools");
}

#[test]
fn a_custom_scripted_policy_replaces_the_default() {
    struct AlwaysDone(&'static str);
    impl ScriptedPolicy for AlwaysDone {
        fn decide(&mut self, _: &AgentView, _: &mut DetRng) -> Decision {
            Decision::Done(self.0.into())
        }
    }

    let mut w = World::with_runtime(
        cfg(),
        AgentRuntime::new(cfg()).with_scripted(Box::new(AlwaysDone("custom"))),
    );
    let id = w.spawn(&AgentSpec::new("bot", "g").with_policy(Policy::Scripted));
    assert_eq!(w.state(id), AgentState::Done);
    assert_eq!(w.text(id, w.v.result), "custom");
}

#[test]
fn scripted_policy_receives_the_full_agent_view() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut w = World::with_runtime(
        cfg(),
        AgentRuntime::new(cfg()).with_scripted(Box::new(RecordViews(seen.clone()))),
    );
    let mood = w.g.attr("agent.mood");
    let id = w.spawn(
        &AgentSpec::new("bob", "summarize")
            .with_policy(Policy::Scripted)
            .with_tools(["search", "read"])
            .with_extra(mood, Value::Text("curious".into())),
    );

    {
        let views = seen.borrow();
        assert_eq!(
            views.len(),
            1,
            "Say deja al agente Idle hasta el tick siguiente"
        );
        assert_eq!(
            views[0],
            full_view(
                id,
                1,
                vec![],
                &[("agent.mood", Value::Text("curious".into()))],
            )
        );
    }

    let out = w.run(vec![]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert_eq!(w.state(id), AgentState::Done);
    assert_eq!(w.text(id, w.v.result), "ok");

    let views = seen.borrow();
    assert_eq!(views.len(), 2);
    assert_eq!(
        views[1],
        full_view(
            id,
            2,
            vec![("say".into(), "noted".into())],
            &[("agent.mood", Value::Text("curious".into()))],
        )
    );
}

struct RecordViews(Rc<RefCell<Vec<AgentView>>>);

impl ScriptedPolicy for RecordViews {
    fn decide(&mut self, view: &AgentView, _: &mut DetRng) -> Decision {
        self.0.borrow_mut().push(view.clone());
        if view.memory.is_empty() {
            Decision::Say("noted".into())
        } else {
            Decision::Done("ok".into())
        }
    }
}

fn full_view(
    id: ConceptId,
    request: u64,
    memory: Vec<(String, String)>,
    extra: &[(&str, Value)],
) -> AgentView {
    let steps = i64::try_from(request).expect("request cabe en i64");
    let mut props = vec![
        ("agent.born".into(), Value::Int(1)),
        ("agent.goal".into(), Value::Text("summarize".into())),
        ("agent.max_steps".into(), Value::Int(8)),
        (
            "agent.mem_count".into(),
            Value::Int(i64::try_from(memory.len()).expect("memoria cabe en i64")),
        ),
        ("agent.name".into(), Value::Text("bob".into())),
        ("agent.pending".into(), Value::Int(steps)),
        ("agent.policy".into(), Value::Text("scripted".into())),
        ("agent.remaining".into(), Value::Int(0)),
        ("agent.state".into(), Value::Text("idle".into())),
        ("agent.steps".into(), Value::Int(steps)),
        ("agent.timeout".into(), Value::Int(4)),
    ];
    props.extend(extra.iter().map(|(k, v)| ((*k).to_string(), v.clone())));
    props.sort_by(|a, b| a.0.cmp(&b.0));
    AgentView {
        agent: id,
        request,
        name: "bob".into(),
        goal: "summarize".into(),
        tools: vec!["search".into(), "read".into()],
        memory,
        props,
    }
}

#[test]
fn uniform_tool_is_deterministic_for_a_given_rng() {
    let view = AgentView {
        agent: ConceptId(1),
        request: 1,
        name: "a".into(),
        goal: "g".into(),
        tools: vec!["search".into(), "shell".into(), "browse".into()],
        memory: vec![],
        props: vec![],
    };
    let mut a = DetRng::from_seed_and_tick(7, 3);
    let mut b = DetRng::from_seed_and_tick(7, 3);
    assert_eq!(
        UniformTool.decide(&view, &mut a),
        UniformTool.decide(&view, &mut b)
    );
    assert_eq!(
        UniformTool.decide(
            &AgentView {
                tools: vec![],
                ..view
            },
            &mut DetRng::from_seed_and_tick(1, 0)
        ),
        Decision::Done("no_tools".into())
    );
}

#[test]
fn scripted_with_tools_emits_a_reproducible_tool_call() {
    let spec = AgentSpec::new("bot", "g")
        .with_policy(Policy::Scripted)
        .with_tools(["search", "shell"]);

    let mut a = World::new(cfg());
    a.seed = 11;
    let id_a = a.spawn(&spec);
    let mut b = World::new(cfg());
    b.seed = 11;
    let id_b = b.spawn(&spec);

    assert_eq!(a.state(id_a), AgentState::Acting);
    assert_eq!(b.state(id_b), AgentState::Acting);
    assert_eq!(a.g.export(), b.g.export());
}

#[test]
fn tool_outside_the_agents_toolset_is_denied() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g").with_tools(["search"]));
    let out = w.run(vec![Reply::call(id, 1, "shell ls").to_event()]);
    assert!(tool_calls(&w.v, &out).is_empty(), "{out:?}");
    assert_eq!(w.memory(id), [("denied".into(), "shell".into())]);
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.pending), 2);
}

#[test]
fn call_to_a_known_tool_enters_acting_and_observe_returns_to_idle() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g").with_tools(["search"]));
    let out = w.run(vec![Reply::call(id, 1, "search foo bar").to_event()]);
    assert_eq!(
        tool_calls(&w.v, &out),
        [(id, "search".into(), "foo bar".into())]
    );
    assert_eq!(w.state(id), AgentState::Acting);
    assert_eq!(w.int(id, w.v.pending), 1);
    assert_eq!(w.memory(id), [("call".into(), "search foo bar".into())]);

    let out = w.run(vec![Reply::observe(id, 1, "found").to_event()]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert_eq!(
        w.memory(id),
        [
            ("call".into(), "search foo bar".into()),
            ("observe".into(), "found".into())
        ]
    );
    assert_eq!(w.state(id), AgentState::Thinking);
    assert_eq!(w.int(id, w.v.pending), 2);
}

#[test]
fn fail_is_accepted_while_acting() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g").with_tools(["search"]));
    w.run(vec![Reply::call(id, 1, "search").to_event()]);
    assert_eq!(w.state(id), AgentState::Acting);
    let out = w.run(vec![Reply::fail(id, 1, "abort").to_event()]);
    assert_eq!(w.state(id), AgentState::Failed);
    assert_eq!(failed(&w.v, &out), [(id, "abort".into())]);
}

#[test]
fn tool_result_uses_last_wins_for_ok_and_text() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g").with_tools(["search"]));
    w.run(vec![Reply::call(id, 1, "search").to_event()]);

    let first = w.run(vec![tool_result(
        &w.v,
        Some(id),
        Value::Int(1),
        vec![
            (w.v.ok, Value::Bool(false)),
            (w.v.text, Value::Text("first".into())),
            (w.v.ok, Value::Bool(true)),
            (w.v.text, Value::Text("second".into())),
        ],
    )]);
    assert!(faults(&first).is_empty(), "{first:?}");
    assert_eq!(
        w.memory(id).last().map(|(k, t)| (k.as_str(), t.as_str())),
        Some(("observe", "second")),
        "ganan las últimas claves ok y text"
    );

    // El agente ya planificó la petición 2; un resultado con request negativa es un Fault.
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g").with_tools(["search"]));
    w.run(vec![Reply::call(id, 1, "search").to_event()]);
    let out = w.run(vec![tool_result(
        &w.v,
        Some(id),
        Value::Int(-1),
        vec![(w.v.text, Value::Text("x".into()))],
    )]);
    assert!(
        faults(&out)
            .iter()
            .any(|m| m.contains("tool result without request")),
        "{out:?}"
    );
    assert_eq!(w.state(id), AgentState::Acting);

    let out = w.run(vec![tool_result(
        &w.v,
        None,
        Value::Int(1),
        vec![(w.v.text, Value::Text("x".into()))],
    )]);
    assert!(
        faults(&out)
            .iter()
            .any(|m| m.contains("tool result without agent")),
        "{out:?}"
    );
}

#[test]
fn tool_error_arrives_as_a_memory_entry() {
    let mut w = World::new(cfg());
    let id = w.spawn(&AgentSpec::new("alice", "g").with_tools(["search"]));
    w.run(vec![Reply::call(id, 1, "search").to_event()]);
    let out = w.run(vec![tool_result(
        &w.v,
        Some(id),
        Value::Int(1),
        vec![
            (w.v.ok, Value::Bool(false)),
            (w.v.text, Value::Text("boom".into())),
        ],
    )]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert_eq!(
        w.memory(id),
        [
            ("call".into(), "search".into()),
            ("tool_error".into(), "boom".into())
        ]
    );
    assert_eq!(w.state(id), AgentState::Thinking);
}

#[test]
fn memory_keep_last_prunes_oldest_entries() {
    let mut w = World::new(AgentConfig {
        memory_keep_last: 2,
        ..cfg()
    });
    let id = w.spawn(&AgentSpec::new("alice", "g"));
    for req in 1..=4 {
        w.run(vec![Reply::say(id, req, format!("m{req}")).to_event()]);
    }
    assert_eq!(
        w.memory(id),
        [("say".into(), "m3".into()), ("say".into(), "m4".into())]
    );
    assert_eq!(
        w.g.query(&[w.v.mem_seq]).len(),
        2,
        "los conceptos podados se destruyen"
    );
    assert_eq!(w.int(id, w.v.mem_count), 4, "el contador no retrocede");
}

#[test]
fn memory_keep_last_zero_does_not_prune() {
    let mut w = World::new(AgentConfig {
        memory_keep_last: 0,
        ..cfg()
    });
    let id = w.spawn(&AgentSpec::new("alice", "g"));
    w.run(vec![Reply::say(id, 1, "m1").to_event()]);
    w.run(vec![Reply::say(id, 2, "m2").to_event()]);
    assert_eq!(
        w.memory(id),
        [("say".into(), "m1".into()), ("say".into(), "m2".into())]
    );
}

#[test]
fn a_memory_policy_can_filter_and_rewrite() {
    struct Redact;
    impl MemoryPolicy for Redact {
        fn filter(&self, e: MemoryEntry) -> Option<MemoryEntry> {
            match e.kind.as_str() {
                "denied" => None,
                "say" => Some(MemoryEntry {
                    text: e.text.replace("secret", "***"),
                    ..e
                }),
                _ => Some(e),
            }
        }
    }

    let mut w = World::with_runtime(
        cfg(),
        AgentRuntime::new(cfg()).with_memory(Box::new(Redact)),
    );
    let id = w.spawn(&AgentSpec::new("alice", "g").with_tools(["search"]));
    w.run(vec![Reply::call(id, 1, "shell ls").to_event()]);
    assert!(w.memory(id).is_empty(), "denied no se anota");
    w.run(vec![Reply::say(id, 2, "the secret is out").to_event()]);
    assert_eq!(w.memory(id), [("say".into(), "the *** is out".into())]);
}

#[test]
fn invalid_policy_on_an_idle_agent_fails() {
    let mut w = World::new(cfg());
    let id = w.spawn_only(&AgentSpec::new("alice", "g"));
    assert_eq!(w.state(id), AgentState::Idle);
    w.g.set(id, w.v.policy, Value::Text("llm".into()));
    let out = w.run(vec![]);
    assert_eq!(w.state(id), AgentState::Failed);
    assert_eq!(w.text(id, w.v.result), "invalid_policy");
    assert_eq!(failed(&w.v, &out), [(id, "invalid_policy".into())]);
}
