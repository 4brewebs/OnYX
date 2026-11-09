//! Creación de agentes: `Vocab` con prefijo, `AgentSpec`/`spawn_event` y
//! `AgentSpawner` aplicando `AgentConfig`. Solo se usa la API pública del soul.

use onyx_soul_agents::{
    AgentBus, AgentConfig, AgentDriver, AgentGraph, AgentSnapshotStore, AgentSpawner, AgentSpec,
    AgentState, Attr, AttrOnly, AttrRegistry, ConceptId, Ctx, DetRng, DomainEvent, EventBus,
    FACTORIES, Graph, GraphView, InMemorySystemRegistry, OnyxEvent, OnyxSystem, Policy, RustLoader,
    Supervisor, SystemEvent, Value, Vocab, spawn_event, stage_with, tick,
};

// ---------- utilidades de prueba ----------

fn config(max_agents: usize) -> AgentConfig {
    AgentConfig {
        max_agents,
        default_max_steps: 5,
        default_timeout_ticks: 3,
        ..AgentConfig::default()
    }
}

/// Spawner inicializado sobre `graph`, con el vocabulario que registra.
fn spawner(graph: &mut AgentGraph, config: AgentConfig) -> (AgentSpawner, Vocab) {
    let prefix = config.vocab_prefix.clone();
    let mut s = AgentSpawner::new(config);
    s.init(&mut AttrOnly(graph)).unwrap();
    (s, Vocab::resolve_with(graph, &prefix))
}

/// Ejecuta un `run` con `Tick { n }` delante de `events`; devuelve el outbox.
fn run(
    s: &mut AgentSpawner,
    graph: &mut AgentGraph,
    n: u64,
    events: Vec<OnyxEvent>,
) -> Vec<OnyxEvent> {
    let mut inbox = vec![OnyxEvent::System(SystemEvent::Tick { n })];
    inbox.extend(events);
    run_raw(s, graph, &inbox)
}

fn run_raw(s: &mut AgentSpawner, graph: &mut AgentGraph, inbox: &[OnyxEvent]) -> Vec<OnyxEvent> {
    let mut outbox = Vec::new();
    let mut rng = DetRng::from_seed_and_tick(0, 0);
    s.run(&mut Ctx {
        graph,
        inbox,
        outbox: &mut outbox,
        rng: &mut rng,
    });
    outbox
}

fn domain(e: &OnyxEvent) -> &DomainEvent {
    match e {
        OnyxEvent::Domain(d) => d,
        other => panic!("se esperaba un evento de dominio, llegó {other:?}"),
    }
}

/// Id del agente creado o motivo del rechazo, por evento de salida.
#[derive(Debug, PartialEq)]
enum Outcome {
    Spawned(ConceptId),
    Rejected(String, Option<ConceptId>),
}

fn outcomes(v: &Vocab, out: &[OnyxEvent]) -> Vec<Outcome> {
    out.iter()
        .map(|e| {
            let d = domain(e);
            if d.kind == v.ev_spawned {
                Outcome::Spawned(d.subject.expect("spawned lleva el agente como subject"))
            } else if d.kind == v.ev_rejected {
                let reason = d
                    .data
                    .iter()
                    .rev()
                    .find_map(|(k, val)| match val {
                        Value::Text(r) if *k == v.reason => Some(r.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("rechazo sin motivo: {d:?}"));
                Outcome::Rejected(reason, d.subject)
            } else {
                panic!("evento inesperado {d:?}")
            }
        })
        .collect()
}

fn spawned_id(v: &Vocab, out: &[OnyxEvent]) -> ConceptId {
    match outcomes(v, out).as_slice() {
        [Outcome::Spawned(id)] => *id,
        other => panic!("se esperaba un único spawned, llegó {other:?}"),
    }
}

fn agents(g: &AgentGraph, v: &Vocab) -> Vec<ConceptId> {
    g.query(&[v.state])
}

fn set_state(g: &mut AgentGraph, v: &Vocab, id: ConceptId, s: AgentState) {
    g.set(id, v.state, Value::Text(s.as_str().into()));
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

// ---------- Vocab ----------

#[test]
fn vocab_is_idempotent_and_every_name_is_distinct() {
    let mut g = AgentGraph::new();
    let a = Vocab::resolve(&mut g);
    let b = Vocab::resolve(&mut g);
    assert_eq!(
        format!("{a:?}"),
        format!("{b:?}"),
        "resolver dos veces da los mismos Attr"
    );

    // Todos los nombres deben ser distintos: una colisión mezclaría, por
    // ejemplo, la propiedad `tool` de un evento con `tool.name` de un concepto.
    let all: Vec<Attr> = vec![
        a.name,
        a.goal,
        a.state,
        a.policy,
        a.steps,
        a.max_steps,
        a.pending,
        a.remaining,
        a.timeout,
        a.born,
        a.result,
        a.mem_count,
        a.tools_csv,
        a.rel_tool,
        a.rel_memory,
        a.rel_parent,
        a.tool_name,
        a.mem_seq,
        a.mem_kind,
        a.mem_text,
        a.mem_tick,
        a.ev_spawn,
        a.ev_spawned,
        a.ev_rejected,
        a.ev_tool_call,
        a.ev_tool_result,
        a.ev_finished,
        a.ev_failed,
        a.request,
        a.tool,
        a.args,
        a.text,
        a.reason,
        a.ok,
    ];
    let mut unique = all.clone();
    unique.sort_by_key(|x| x.raw());
    unique.dedup();
    assert_eq!(unique.len(), all.len());
    assert_eq!(a.name, g.attr("agent.name"));
    assert_eq!(a.tool_name, g.attr("agent.tool.name"));
}

#[test]
fn prefixes_give_disjoint_vocabularies() {
    let mut g = AgentGraph::new();
    let a = Vocab::resolve_with(&mut g, "agent");
    let t = Vocab::resolve_with(&mut g, "team");
    assert_eq!(t.goal, g.attr("team.goal"));
    assert_ne!(a.goal, t.goal);
    assert_ne!(a.ev_spawn, t.ev_spawn);
    assert!(
        !a.is_reserved(t.state),
        "el estado de otro prefijo no es reservado aquí"
    );
}

#[test]
fn reserved_attrs_are_exactly_the_framework_properties() {
    let mut g = AgentGraph::new();
    let v = Vocab::resolve(&mut g);
    for attr in [
        v.name,
        v.goal,
        v.state,
        v.policy,
        v.steps,
        v.max_steps,
        v.pending,
        v.remaining,
        v.timeout,
        v.born,
        v.result,
        v.mem_count,
        v.tools_csv,
    ] {
        assert!(v.is_reserved(attr), "{attr:?} debe estar reservado");
    }
    let app = g.attr("agent.mood");
    assert!(!v.is_reserved(app));
}

#[test]
fn agent_state_text_roundtrips_and_only_done_failed_are_terminal() {
    use AgentState::*;
    for s in [Idle, Thinking, Acting, Done, Failed] {
        assert_eq!(AgentState::parse(s.as_str()), Some(s));
        assert_eq!(s.is_terminal(), matches!(s, Done | Failed), "{s:?}");
    }
    assert_eq!(AgentState::parse("IDLE"), None);
    assert_eq!(AgentState::parse(""), None);
    assert_eq!(
        Policy::parse(Policy::Scripted.as_str()),
        Some(Policy::Scripted)
    );
    assert_eq!(
        Policy::parse(Policy::External.as_str()),
        Some(Policy::External)
    );
    assert_eq!(Policy::parse("llm"), None);
}

// ---------- spawn_event ----------

#[test]
fn spawn_event_carries_only_what_the_spec_sets() {
    let mut g = AgentGraph::new();
    let v = Vocab::resolve(&mut g);
    let mood = g.attr("agent.mood");
    let parent = ConceptId(7);

    let minimal = spawn_event(&v, &AgentSpec::new("a", "g"), None);
    let d = domain(&minimal);
    assert_eq!(d.kind, v.ev_spawn);
    assert_eq!(d.subject, None);
    assert_eq!(
        d.data,
        vec![
            (v.name, text("a")),
            (v.goal, text("g")),
            (v.policy, text("external")),
            (v.tools_csv, text("")),
        ],
        "sin max_steps ni timeout: el spawner pondrá los de AgentConfig"
    );

    let full = AgentSpec::new("b", "h")
        .with_policy(Policy::Scripted)
        .with_tools(["leer", "escribir"])
        .with_max_steps(9)
        .with_timeout_ticks(4)
        .with_extra(mood, text("calma"));
    let e = spawn_event(&v, &full, Some(parent));
    let d = domain(&e);
    assert_eq!(d.subject, Some(parent));
    assert_eq!(
        d.data,
        vec![
            (v.name, text("b")),
            (v.goal, text("h")),
            (v.policy, text("scripted")),
            (v.tools_csv, text("leer,escribir")),
            (v.max_steps, Value::Int(9)),
            (v.timeout, Value::Int(4)),
            (mood, text("calma")),
        ]
    );
}

// ---------- AgentSpawner ----------

#[test]
fn spawned_agent_has_every_framework_property_and_nothing_else() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let out = run(
        &mut s,
        &mut g,
        42,
        vec![spawn_event(&v, &AgentSpec::new("ana", "resumir"), None)],
    );
    let id = spawned_id(&v, &out);
    assert_eq!(domain(&out[0]).data, vec![(v.name, text("ana"))]);

    let mut props = g.props(id);
    props.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![
        ("agent.born".to_string(), Value::Int(42)),
        ("agent.goal".into(), text("resumir")),
        ("agent.max_steps".into(), Value::Int(5)),
        ("agent.mem_count".into(), Value::Int(0)),
        ("agent.name".into(), text("ana")),
        ("agent.pending".into(), Value::Int(0)),
        ("agent.policy".into(), text("external")),
        ("agent.remaining".into(), Value::Int(0)),
        ("agent.state".into(), text("idle")),
        ("agent.steps".into(), Value::Int(0)),
        ("agent.timeout".into(), Value::Int(3)),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(props, expected, "tools_csv no se copia como propiedad");
    assert!(g.related(id, v.rel_tool).is_empty());
    assert!(g.related(id, v.rel_parent).is_empty());
}

#[test]
fn spec_budget_overrides_config() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(16));
    let explicit = AgentSpec::new("a", "g")
        .with_max_steps(11)
        .with_timeout_ticks(2);
    let id = spawned_id(
        &v,
        &run(&mut s, &mut g, 1, vec![spawn_event(&v, &explicit, None)]),
    );
    assert_eq!(g.get(id, v.max_steps), Some(Value::Int(11)));
    assert_eq!(g.get(id, v.timeout), Some(Value::Int(2)));
}

#[test]
fn malformed_specs_are_rejected_without_touching_the_graph() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let parent = ConceptId(99);
    let ev = |data: Vec<(Attr, Value)>| {
        OnyxEvent::Domain(DomainEvent {
            kind: v.ev_spawn,
            subject: Some(parent),
            data,
        })
    };
    let before = g.export();
    let cases = [
        (vec![(v.goal, text("g"))], "invalid_spec"),
        (vec![(v.name, text("a"))], "invalid_spec"),
        (
            vec![(v.name, Value::Int(1)), (v.goal, text("g"))],
            "invalid_spec",
        ),
        (
            vec![
                (v.name, text("a")),
                (v.goal, text("g")),
                (v.policy, text("llm")),
            ],
            "invalid_policy",
        ),
        (
            vec![
                (v.name, text("a")),
                (v.goal, text("g")),
                (v.policy, Value::Bool(true)),
            ],
            "invalid_policy",
        ),
    ];
    for (data, reason) in cases {
        let out = run(&mut s, &mut g, 1, vec![ev(data.clone())]);
        assert_eq!(
            outcomes(&v, &out),
            [Outcome::Rejected(reason.into(), Some(parent))],
            "{data:?}"
        );
    }
    assert_eq!(
        g.export(),
        before,
        "un rechazo no crea conceptos ni atributos"
    );
}

#[test]
fn app_cannot_forge_framework_state_but_keeps_its_own_properties() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let mood = g.attr("agent.mood");
    let spec = AgentSpec::new("a", "g")
        .with_max_steps(11)
        .with_extra(v.state, text("done"))
        .with_extra(v.steps, Value::Int(99))
        .with_extra(v.born, Value::Int(-1))
        .with_extra(v.result, text("falso"))
        .with_extra(v.name, text("hacker"))
        .with_extra(v.max_steps, Value::Int(1))
        .with_extra(mood, text("calma"));
    let id = spawned_id(
        &v,
        &run(&mut s, &mut g, 6, vec![spawn_event(&v, &spec, None)]),
    );
    assert_eq!(g.get(id, v.name), Some(text("a")));
    assert_eq!(g.get(id, v.max_steps), Some(Value::Int(11)));
    assert_eq!(g.get(id, v.state), Some(text("idle")));
    assert_eq!(g.get(id, v.steps), Some(Value::Int(0)));
    assert_eq!(g.get(id, v.born), Some(Value::Int(6)));
    assert_eq!(
        g.get(id, v.result),
        None,
        "result solo lo escribe el framework"
    );
    assert_eq!(g.get(id, mood), Some(text("calma")));
}

#[test]
fn tools_are_trimmed_deduplicated_and_shared_between_agents() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let a = AgentSpec::new("a", "g").with_tools([" leer ", "", "escribir", "leer"]);
    let b = AgentSpec::new("b", "g").with_tools(["escribir"]);
    let out = run(
        &mut s,
        &mut g,
        1,
        vec![spawn_event(&v, &a, None), spawn_event(&v, &b, None)],
    );
    let [Outcome::Spawned(ia), Outcome::Spawned(ib)] = outcomes(&v, &out)[..] else {
        panic!("se esperaban dos spawned")
    };
    let name = |g: &AgentGraph, id| match g.get(id, v.tool_name) {
        Some(Value::Text(s)) => s,
        other => panic!("herramienta sin nombre: {other:?}"),
    };
    let tools_a: Vec<_> = g
        .related(ia, v.rel_tool)
        .into_iter()
        .map(|t| name(&g, t))
        .collect();
    assert_eq!(tools_a.len(), 2, "{tools_a:?}");
    assert!(tools_a.contains(&"leer".into()) && tools_a.contains(&"escribir".into()));
    let tools_b = g.related(ib, v.rel_tool);
    assert_eq!(tools_b.len(), 1);
    assert!(
        g.related(ia, v.rel_tool).contains(&tools_b[0]),
        "un nombre, un concepto"
    );
    assert_eq!(
        g.query(&[v.tool_name]).len(),
        2,
        "solo dos conceptos herramienta"
    );
    assert!(
        g.get(tools_b[0], v.state).is_none(),
        "una herramienta no es un agente"
    );
}

#[test]
fn parent_link_only_to_an_existing_agent() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let parent = spawned_id(
        &v,
        &run(
            &mut s,
            &mut g,
            1,
            vec![spawn_event(&v, &AgentSpec::new("p", "g"), None)],
        ),
    );
    let plain = g.create();
    let child = spawned_id(
        &v,
        &run(
            &mut s,
            &mut g,
            2,
            vec![spawn_event(&v, &AgentSpec::new("c", "g"), Some(parent))],
        ),
    );
    assert_eq!(g.related(child, v.rel_parent), vec![parent]);

    for subject in [plain, ConceptId(10_000)] {
        let orphan = spawned_id(
            &v,
            &run(
                &mut s,
                &mut g,
                3,
                vec![spawn_event(&v, &AgentSpec::new("o", "g"), Some(subject))],
            ),
        );
        assert!(
            g.related(orphan, v.rel_parent).is_empty(),
            "{subject:?} no es un agente"
        );
    }
}

#[test]
fn limit_counts_live_agents_and_frees_slots_when_they_finish() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(2));
    let spec = |n: &str| spawn_event(&v, &AgentSpec::new(n, "g"), None);

    // En un mismo tick el cupo se recalcula tras cada alta.
    let out = run(&mut s, &mut g, 1, vec![spec("a"), spec("b"), spec("c")]);
    let o = outcomes(&v, &out);
    assert!(
        matches!(o[..], [Outcome::Spawned(_), Outcome::Spawned(_), Outcome::Rejected(ref r, None)] if r == "max_agents"),
        "{o:?}"
    );
    let ids = agents(&g, &v);
    assert_eq!(ids.len(), 2);

    // Estados no terminales siguen ocupando cupo.
    for st in [AgentState::Thinking, AgentState::Acting] {
        set_state(&mut g, &v, ids[0], st);
        assert!(
            matches!(
                outcomes(&v, &run(&mut s, &mut g, 2, vec![spec("x")]))[..],
                [Outcome::Rejected(..)]
            ),
            "{st:?}"
        );
    }
    // Done y Failed liberan cupo.
    set_state(&mut g, &v, ids[0], AgentState::Done);
    assert!(matches!(
        outcomes(&v, &run(&mut s, &mut g, 3, vec![spec("d")]))[..],
        [Outcome::Spawned(_)]
    ));
    set_state(&mut g, &v, ids[1], AgentState::Failed);
    assert!(matches!(
        outcomes(&v, &run(&mut s, &mut g, 4, vec![spec("e")]))[..],
        [Outcome::Spawned(_)]
    ));
    assert!(matches!(
        outcomes(&v, &run(&mut s, &mut g, 5, vec![spec("f")]))[..],
        [Outcome::Rejected(..)]
    ));
    assert_eq!(agents(&g, &v).len(), 4, "los terminados siguen en el grafo");
}

#[test]
fn without_a_tick_in_the_inbox_nothing_is_spawned() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let before = g.export();
    let out = run_raw(
        &mut s,
        &mut g,
        &[spawn_event(&v, &AgentSpec::new("a", "g"), None)],
    );
    assert!(out.is_empty());
    assert_eq!(g.export(), before);
}

#[test]
fn spawners_with_different_prefixes_ignore_each_other() {
    let mut g = AgentGraph::new();
    let (mut sa, va) = spawner(&mut g, config(1));
    let (mut st, vt) = spawner(
        &mut g,
        AgentConfig {
            vocab_prefix: "team".into(),
            ..config(1)
        },
    );

    let ea = spawn_event(&va, &AgentSpec::new("a", "g"), None);
    let et = spawn_event(&vt, &AgentSpec::new("t", "g"), None);
    // Cada spawner solo atiende su evento, aunque vea los dos.
    let out_a = run(&mut sa, &mut g, 1, vec![ea.clone(), et.clone()]);
    let out_t = run(&mut st, &mut g, 1, vec![ea, et]);
    let ia = spawned_id(&va, &out_a);
    let it = spawned_id(&vt, &out_t);
    // Con max_agents = 1 en ambos, el agente del otro prefijo no ocupa cupo.
    assert_eq!(agents(&g, &va), vec![ia]);
    assert_eq!(agents(&g, &vt), vec![it]);
    assert_eq!(g.get(it, vt.name), Some(text("t")));
    assert_eq!(g.get(it, va.name), None);
}

#[test]
fn same_inputs_give_the_same_graph() {
    let script = |g: &mut AgentGraph| {
        let (mut s, v) = spawner(g, config(3));
        let mut log = Vec::new();
        for n in 1..=4u64 {
            let specs = (0..n).map(|i| {
                spawn_event(
                    &v,
                    &AgentSpec::new(format!("a{n}{i}"), "g").with_tools(["t", "u"]),
                    None,
                )
            });
            log.extend(run(&mut s, g, n, specs.collect()));
            if let Some(first) = agents(g, &v).first() {
                set_state(g, &v, *first, AgentState::Done);
            }
        }
        log
    };
    let (mut g1, mut g2) = (AgentGraph::new(), AgentGraph::new());
    let (l1, l2) = (script(&mut g1), script(&mut g2));
    // OnyxEvent no implementa PartialEq; su Debug incluye todos los campos.
    assert_eq!(format!("{l1:?}"), format!("{l2:?}"));
    assert_eq!(g1.export(), g2.export());
    assert!(l1.len() >= 10, "el guion debe ejercitar altas y rechazos");
}

// ---------- extremo a extremo por tick ----------

#[test]
fn a_custom_prefix_flows_through_loader_spawner_runtime_and_driver() {
    let dir = std::env::temp_dir().join(format!(
        "onyx-soul-agents-spawn-test-{}",
        std::process::id()
    ));
    let mut store = AgentSnapshotStore::new(&dir);
    let mut graph = AgentGraph::new();
    let mut bus = AgentBus::new();
    let mut driver = AgentDriver::new();
    let cfg = AgentConfig {
        vocab_prefix: "team".into(),
        ..config(4)
    };

    let mut systems = RustLoader::new()
        .load_app(
            &mut InMemorySystemRegistry::new(),
            &FACTORIES,
            &[stage_with("main", &cfg)],
            "main",
        )
        .unwrap();
    let mut sup = Supervisor::new(systems.len());
    sup.init_all(&mut systems, &mut graph);
    for i in 0..systems.len() {
        assert!(sup.start(i));
    }
    let v = Vocab::resolve_with(&mut graph, "team");
    driver.set_vocab(v);
    // Un evento con el prefijo por defecto no debe crear nada en este stage.
    let default_vocab = Vocab::resolve(&mut graph);
    bus.push(spawn_event(
        &default_vocab,
        &AgentSpec::new("intruso", "x"),
        None,
    ));
    bus.push(spawn_event(&v, &AgentSpec::new("t1", "resumir"), None));

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

    let ids = agents(&graph, &v);
    assert_eq!(ids.len(), 1);
    assert!(agents(&graph, &default_vocab).is_empty());
    // El runtime, con el mismo prefijo, ya ha hecho avanzar al agente.
    assert_eq!(graph.get(ids[0], v.state), Some(text("thinking")));
    assert_eq!(graph.get(ids[0], v.steps), Some(Value::Int(1)));
    let req = driver.last_requests();
    assert_eq!(req.len(), 1);
    assert_eq!((req[0].agent, req[0].goal.as_str()), (ids[0], "resumir"));
    // El spawned del tick 1 queda en el bus para el siguiente ciclo.
    let pending = bus.drain();
    assert!(
        pending
            .iter()
            .any(|e| matches!(e, OnyxEvent::Domain(d) if d.kind == v.ev_spawned))
    );

    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn an_event_without_policy_defaults_to_external() {
    // Un evento construido a mano por la app puede omitir la política.
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let ev = OnyxEvent::Domain(DomainEvent {
        kind: v.ev_spawn,
        subject: None,
        data: vec![(v.name, text("a")), (v.goal, text("g"))],
    });
    let id = spawned_id(&v, &run(&mut s, &mut g, 1, vec![ev]));
    assert_eq!(g.get(id, v.policy), Some(text("external")));
}

#[test]
fn repeated_keys_in_the_event_keep_the_last_value() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let mood = g.attr("agent.mood");
    let ev = OnyxEvent::Domain(DomainEvent {
        kind: v.ev_spawn,
        subject: None,
        data: vec![
            (v.name, text("primero")),
            (v.goal, text("viejo")),
            (v.max_steps, Value::Int(2)),
            (v.timeout, Value::Int(1)),
            (v.policy, text("llm")),
            (v.tools_csv, text("a")),
            (mood, text("triste")),
            (v.name, text("ultimo")),
            (v.goal, text("nuevo")),
            (v.max_steps, Value::Int(9)),
            (v.timeout, Value::Int(4)),
            (v.policy, text("scripted")),
            (v.tools_csv, text("b")),
            (mood, text("alegre")),
        ],
    });
    let id = spawned_id(&v, &run(&mut s, &mut g, 1, vec![ev]));
    assert_eq!(g.get(id, v.name), Some(text("ultimo")));
    assert_eq!(g.get(id, v.goal), Some(text("nuevo")));
    assert_eq!(g.get(id, v.max_steps), Some(Value::Int(9)));
    assert_eq!(g.get(id, v.timeout), Some(Value::Int(4)));
    assert_eq!(g.get(id, v.policy), Some(text("scripted")));
    assert_eq!(g.get(id, mood), Some(text("alegre")));
    let tools = g.related(id, v.rel_tool);
    assert_eq!(tools.len(), 1);
    assert_eq!(g.get(tools[0], v.tool_name), Some(text("b")));

    // El último valor decide también si el evento es válido.
    let first_bad = OnyxEvent::Domain(DomainEvent {
        kind: v.ev_spawn,
        subject: None,
        data: vec![
            (v.name, Value::Int(1)),
            (v.name, text("ok")),
            (v.goal, text("g")),
        ],
    });
    let id = spawned_id(&v, &run(&mut s, &mut g, 2, vec![first_bad]));
    assert_eq!(g.get(id, v.name), Some(text("ok")));
    let last_bad = OnyxEvent::Domain(DomainEvent {
        kind: v.ev_spawn,
        subject: None,
        data: vec![
            (v.name, text("ok")),
            (v.name, Value::Int(1)),
            (v.goal, text("g")),
        ],
    });
    assert_eq!(
        outcomes(&v, &run(&mut s, &mut g, 3, vec![last_bad])),
        [Outcome::Rejected("invalid_spec".into(), None)]
    );
}

#[test]
fn invalid_budget_is_rejected_as_invalid_spec() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let before = g.export();
    let cases = [
        (Value::Int(0), Value::Int(3)),
        (Value::Int(-3), Value::Int(3)),
        (text("7"), Value::Int(3)),
        (Value::Int(5), Value::Int(0)),
        (Value::Int(5), Value::Int(i64::MIN)),
        (Value::Int(5), Value::Float(2.0)),
    ];
    for (max, timeout) in cases {
        let ev = OnyxEvent::Domain(DomainEvent {
            kind: v.ev_spawn,
            subject: None,
            data: vec![
                (v.name, text("b")),
                (v.goal, text("g")),
                (v.max_steps, max.clone()),
                (v.timeout, timeout.clone()),
            ],
        });
        let out = run(&mut s, &mut g, 1, vec![ev]);
        assert_eq!(
            outcomes(&v, &out),
            [Outcome::Rejected("invalid_spec".into(), None)],
            "{max:?} {timeout:?}"
        );
        assert_eq!(
            domain(&out[0]).data,
            vec![(v.reason, text("invalid_spec")), (v.name, text("b"))],
            "{max:?} {timeout:?}"
        );
    }
    for spec in [
        AgentSpec::new("z", "g").with_max_steps(0),
        AgentSpec::new("z", "g").with_timeout_ticks(0),
    ] {
        let out = run(&mut s, &mut g, 1, vec![spawn_event(&v, &spec, None)]);
        assert_eq!(
            outcomes(&v, &out),
            [Outcome::Rejected("invalid_spec".into(), None)],
            "{spec:?}"
        );
    }
    assert_eq!(
        g.export(),
        before,
        "un presupuesto inválido no toca el grafo"
    );
}

#[test]
fn rejected_event_includes_the_name_when_present() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(8));
    let parent = spawned_id(
        &v,
        &run(
            &mut s,
            &mut g,
            1,
            vec![spawn_event(&v, &AgentSpec::new("p", "g"), None)],
        ),
    );
    let ok = spawn_event(&v, &AgentSpec::new("ok", "g"), Some(parent));
    let bad = OnyxEvent::Domain(DomainEvent {
        kind: v.ev_spawn,
        subject: Some(parent),
        data: vec![
            (v.name, text("bad")),
            (v.goal, text("g")),
            (v.policy, text("llm")),
        ],
    });
    let out = run(&mut s, &mut g, 2, vec![ok, bad]);
    let o = outcomes(&v, &out);
    assert!(
        matches!(
            o[..],
            [Outcome::Spawned(_), Outcome::Rejected(ref r, Some(p))]
                if r == "invalid_policy" && p == parent
        ),
        "{o:?}"
    );
    assert_eq!(
        domain(&out[1]).data,
        vec![(v.reason, text("invalid_policy")), (v.name, text("bad"))]
    );

    let anon = OnyxEvent::Domain(DomainEvent {
        kind: v.ev_spawn,
        subject: Some(parent),
        data: vec![(v.goal, text("g"))],
    });
    let out = run(&mut s, &mut g, 3, vec![anon]);
    assert_eq!(domain(&out[0]).data, vec![(v.reason, text("invalid_spec"))]);

    let numbered = OnyxEvent::Domain(DomainEvent {
        kind: v.ev_spawn,
        subject: None,
        data: vec![(v.name, Value::Int(1)), (v.goal, text("g"))],
    });
    let out = run(&mut s, &mut g, 4, vec![numbered]);
    assert_eq!(domain(&out[0]).data, vec![(v.reason, text("invalid_spec"))]);
}

#[test]
fn a_full_quota_rejects_as_max_agents_even_if_the_spec_is_invalid() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, config(1));
    let _ = spawned_id(
        &v,
        &run(
            &mut s,
            &mut g,
            1,
            vec![spawn_event(&v, &AgentSpec::new("a", "g"), None)],
        ),
    );
    let ev = OnyxEvent::Domain(DomainEvent {
        kind: v.ev_spawn,
        subject: None,
        data: vec![(v.name, text("tarde"))],
    });
    let out = run(&mut s, &mut g, 2, vec![ev]);
    assert_eq!(
        outcomes(&v, &out),
        [Outcome::Rejected("max_agents".into(), None)]
    );
    assert_eq!(
        domain(&out[0]).data,
        vec![(v.reason, text("max_agents")), (v.name, text("tarde"))],
        "el cupo se comprueba antes que la spec; el nombre viaja igual"
    );
    assert_eq!(agents(&g, &v).len(), 1);
}
