//! Extras de agente frente a los atributos de herramienta y memoria del
//! vocabulario: ni `spawn_event` ni el evento crudo ante `AgentSpawner` deben
//! copiarlos. Si se copian, un agente puede hacerse pasar por herramienta.
use onyx_soul_agents::{
    AgentConfig, AgentGraph, AgentHost, AgentSpawner, AgentSpec, Attr, AttrOnly, AttrRegistry,
    ConceptId, Ctx, DetRng, DomainEvent, EventBus, GraphView, OnyxEvent, OnyxSystem,
    PlatformDriver, SurfaceBinding, SystemEvent, Value, Vocab, spawn_event,
};

const PREFIXES: [&str; 2] = ["agent", "team"];

/// Atributos de concepto herramienta y de entrada de memoria.
fn tool_and_memory(v: &Vocab) -> [(&'static str, Attr); 5] {
    [
        ("tool_name", v.tool_name),
        ("mem_seq", v.mem_seq),
        ("mem_kind", v.mem_kind),
        ("mem_text", v.mem_text),
        ("mem_tick", v.mem_tick),
    ]
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn config(prefix: &str) -> AgentConfig {
    AgentConfig {
        vocab_prefix: prefix.into(),
        ..AgentConfig::default()
    }
}

struct Quiet;
impl PlatformDriver for Quiet {
    fn poll(&mut self, _: &mut dyn EventBus) {}
    fn present(&mut self, _: &dyn GraphView, _: &[SurfaceBinding]) {}
}

fn data(e: &OnyxEvent) -> &[(Attr, Value)] {
    match e {
        OnyxEvent::Domain(d) => &d.data,
        other => panic!("{other:?}"),
    }
}

/// Spawner inicializado y un `run` con `Tick { n }`; devuelve el agente creado.
fn spawn_raw(
    g: &mut AgentGraph,
    s: &mut AgentSpawner,
    v: &Vocab,
    extra: Vec<(Attr, Value)>,
) -> ConceptId {
    let mut payload = vec![(v.name, text("a")), (v.goal, text("g"))];
    payload.extend(extra);
    let inbox = vec![
        OnyxEvent::System(SystemEvent::Tick { n: 1 }),
        OnyxEvent::Domain(DomainEvent {
            kind: v.ev_spawn,
            subject: None,
            data: payload,
        }),
    ];
    let mut outbox = Vec::new();
    let mut rng = DetRng::from_seed_and_tick(0, 0);
    s.run(&mut Ctx {
        graph: g,
        inbox: &inbox,
        outbox: &mut outbox,
        rng: &mut rng,
    });
    match outbox.as_slice() {
        [OnyxEvent::Domain(d)] if d.kind == v.ev_spawned => d.subject.unwrap(),
        other => panic!("spawn no creado: {other:?}"),
    }
}

// ---------- helper spawn_event (rojo en 3ec48b8) ----------

#[test]
fn spawn_event_drops_tool_and_memory_attributes_from_extras() {
    let mut leaked = Vec::new();
    for prefix in PREFIXES {
        let mut g = AgentGraph::new();
        let v = Vocab::resolve_with(&mut g, prefix);
        for (name, attr) in tool_and_memory(&v) {
            let spec = AgentSpec::new("a", "g").with_extra(attr, text("forged"));
            if data(&spawn_event(&v, &spec, None))
                .iter()
                .any(|(k, _)| *k == attr)
            {
                leaked.push(format!("{prefix}: v.{name}"));
            }
        }
    }
    assert_eq!(leaked, Vec::<String>::new(), "extras copiados al evento");
}

// ---------- evento crudo ante AgentSpawner (rojo en 3ec48b8) ----------

#[test]
fn raw_spawn_does_not_copy_tool_and_memory_attributes_onto_the_agent() {
    let mut leaked = Vec::new();
    for prefix in PREFIXES {
        for i in 0..5 {
            let mut g = AgentGraph::new();
            let mut s = AgentSpawner::new(config(prefix));
            s.init(&mut AttrOnly(&mut g)).unwrap();
            let v = Vocab::resolve_with(&mut g, prefix);
            let (name, attr) = tool_and_memory(&v)[i];
            let id = spawn_raw(&mut g, &mut s, &v, vec![(attr, text("forged"))]);
            if g.get(id, attr).is_some() {
                leaked.push(format!("{prefix}: v.{name}"));
            }
        }
    }
    assert_eq!(leaked, Vec::<String>::new(), "extras copiados al agente");
}

// ---------- agente impostor como herramienta (rojo en 3ec48b8) ----------

fn impostor_and_victim(prefix: &str) -> (AgentHost, ConceptId, ConceptId) {
    let mut host = AgentHost::builder(1)
        .config(config(prefix))
        .build()
        .unwrap();
    let v = host.vocab();
    host.spawn(&AgentSpec::new("impostor", "g").with_extra(v.tool_name, text("search")));
    host.run(&mut Quiet, 1);
    host.spawn(&AgentSpec::new("victim", "g").with_tools(["search"]));
    host.run(&mut Quiet, 1);
    let by_name = |n: &str| {
        host.agents()
            .into_iter()
            .find(|id| host.graph().get(*id, v.name) == Some(text(n)))
            .unwrap_or_else(|| panic!("falta el agente {n}"))
    };
    let (imp, vic) = (by_name("impostor"), by_name("victim"));
    (host, imp, vic)
}

#[test]
fn an_agent_with_a_tool_name_extra_is_not_linked_as_a_tool_default_prefix() {
    let (host, impostor, victim) = impostor_and_victim("agent");
    let v = host.vocab();
    let tools = host.graph().related(victim, v.rel_tool);
    assert_eq!(tools.len(), 1, "una herramienta search");
    assert_ne!(
        tools[0], impostor,
        "la víctima usa al agente impostor como herramienta"
    );
    assert_eq!(
        host.graph().get(tools[0], v.state),
        None,
        "la herramienta es un agente"
    );
}

#[test]
fn an_agent_with_a_tool_name_extra_is_not_linked_as_a_tool_custom_prefix() {
    let (host, impostor, victim) = impostor_and_victim("team");
    let v = host.vocab();
    let tools = host.graph().related(victim, v.rel_tool);
    assert_eq!(tools.len(), 1, "una herramienta search");
    assert_ne!(
        tools[0], impostor,
        "la víctima usa al agente impostor como herramienta"
    );
    assert_eq!(
        host.graph().get(tools[0], v.state),
        None,
        "la herramienta es un agente"
    );
}

// ---------- controles (verde) ----------

#[test]
fn app_extras_still_reach_the_agent_on_both_paths() {
    for prefix in PREFIXES {
        let mut g = AgentGraph::new();
        let mut s = AgentSpawner::new(config(prefix));
        s.init(&mut AttrOnly(&mut g)).unwrap();
        let v = Vocab::resolve_with(&mut g, prefix);
        let mood = config(prefix).attr(&mut g, "mood");
        let other = g.attr("app.level");
        let spec = AgentSpec::new("a", "g")
            .with_extra(mood, text("calm"))
            .with_extra(other, Value::Int(7));
        let ev = spawn_event(&v, &spec, None);
        assert!(data(&ev).contains(&(mood, text("calm"))));
        assert!(data(&ev).contains(&(other, Value::Int(7))));
        let id = spawn_raw(
            &mut g,
            &mut s,
            &v,
            vec![(mood, text("calm")), (other, Value::Int(7))],
        );
        assert_eq!(g.get(id, mood), Some(text("calm")));
        assert_eq!(g.get(id, other), Some(Value::Int(7)));
    }
}

#[test]
fn a_real_tool_is_shared_and_never_an_agent() {
    for prefix in PREFIXES {
        let mut host = AgentHost::builder(1)
            .config(config(prefix))
            .build()
            .unwrap();
        let v = host.vocab();
        host.spawn(&AgentSpec::new("a", "g").with_tools(["search"]));
        host.spawn(&AgentSpec::new("b", "g").with_tools(["search"]));
        host.run(&mut Quiet, 1);
        let agents = host.agents();
        assert_eq!(agents.len(), 2);
        let ta = host.graph().related(agents[0], v.rel_tool);
        assert_eq!(ta, host.graph().related(agents[1], v.rel_tool));
        assert_eq!(ta.len(), 1);
        assert!(!agents.contains(&ta[0]));
        assert_eq!(host.view(agents[0]).tools, vec!["search".to_string()]);
    }
}
