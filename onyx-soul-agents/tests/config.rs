//! `AgentConfig`: lectura de `SystemSpec::params` y fábrica del spawner.
use onyx_soul_agents::{
    AgentConfig, AgentGraph, AgentSpec, AttrOnly, AttrRegistry, Ctx, DEFAULT_PREFIX, DetRng,
    OnyxError, OnyxEvent, SystemSpec, Value, Vocab, make_spawner, spawn_event,
};

fn p(key: &str, value: Value) -> (String, Value) {
    (key.into(), value)
}

#[test]
fn empty_params_give_defaults() {
    let c = AgentConfig::from_params(&[]).unwrap();
    assert_eq!(c, AgentConfig::default());
    assert_eq!(c.vocab_prefix, DEFAULT_PREFIX);
    assert_eq!(c.max_agents, 64);
    assert_eq!(c.default_max_steps, 16);
    assert_eq!(c.default_timeout_ticks, 8);
    assert_eq!(c.memory_keep_last, 0);
}

#[test]
fn to_params_roundtrips() {
    let c = AgentConfig {
        vocab_prefix: "team".into(),
        max_agents: 3,
        default_max_steps: 5,
        default_timeout_ticks: 2,
        memory_keep_last: 7,
    };
    assert_eq!(AgentConfig::from_params(&c.to_params()).unwrap(), c);
}

#[test]
fn unknown_keys_are_ignored_and_last_duplicate_wins() {
    let c = AgentConfig::from_params(&[
        p("app.color", Value::Text("rojo".into())),
        p("max_agents", Value::Int(2)),
        p("max_agents", Value::Int(9)),
    ])
    .unwrap();
    assert_eq!(c.max_agents, 9);
}

#[test]
fn invalid_values_are_rejected() {
    let cases = [
        p("vocab_prefix", Value::Text(String::new())),
        p("vocab_prefix", Value::Text("a b".into())),
        p("vocab_prefix", Value::Int(1)),
        p("max_agents", Value::Int(0)),
        p("max_agents", Value::Text("3".into())),
        p("default_max_steps", Value::Int(-1)),
        p("default_timeout_ticks", Value::Int(0)),
        p("memory_keep_last", Value::Int(-1)),
    ];
    for case in cases {
        let key = case.0.clone();
        match AgentConfig::from_params(&[case]) {
            Err(OnyxError::Format(msg)) => assert!(msg.contains(&key), "{msg}"),
            other => panic!("{key}: expected Format error, got {other:?}"),
        }
    }
    // 0 sí es válido para memory_keep_last (sin poda).
    let c = AgentConfig::from_params(&[p("memory_keep_last", Value::Int(0))]).unwrap();
    assert_eq!(c.memory_keep_last, 0);
}

#[test]
fn app_attrs_live_under_the_prefix() {
    let mut g = AgentGraph::new();
    let c = AgentConfig {
        vocab_prefix: "team".into(),
        ..AgentConfig::default()
    };
    assert_eq!(c.attr(&mut g, "mood"), g.attr("team.mood"));
    assert_ne!(c.attr(&mut g, "mood"), g.attr("agent.mood"));
}

#[test]
fn spawner_factory_uses_the_config() {
    let spec = |params| SystemSpec {
        kind: "agent.spawner".into(),
        params,
        persistent: false,
    };
    assert!(make_spawner(&spec(vec![])).is_ok());
    assert!(make_spawner(&spec(vec![p("max_agents", Value::Int(0))])).is_err());
    assert!(make_spawner(&spec(vec![p("vocab_prefix", Value::Text(" ".into()))])).is_err());
}

#[test]
fn spawner_factory_applies_max_agents() {
    let mut graph = AgentGraph::new();
    let spec = SystemSpec {
        kind: "agent.spawner".into(),
        params: vec![p("max_agents", Value::Int(1))],
        persistent: false,
    };
    let mut spawner = make_spawner(&spec).unwrap();
    spawner.init(&mut AttrOnly(&mut graph)).unwrap();
    let vocab = Vocab::resolve(&mut graph);
    let inbox = [
        spawn_event(&vocab, &AgentSpec::new("a", "uno")),
        spawn_event(&vocab, &AgentSpec::new("b", "dos")),
    ];
    let mut outbox = Vec::new();
    let mut rng = DetRng::from_seed_and_tick(0, 0);
    spawner.run(&mut Ctx {
        graph: &mut graph,
        inbox: &inbox,
        outbox: &mut outbox,
        rng: &mut rng,
    });
    let kinds: Vec<_> = outbox
        .iter()
        .map(|e| match e {
            OnyxEvent::Domain(d) => d.kind,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(kinds, [vocab.ev_spawned, vocab.ev_rejected]);
}

#[test]
fn to_params_saturates_counts_instead_of_wrapping() {
    let c = AgentConfig {
        max_agents: usize::MAX,
        memory_keep_last: usize::MAX,
        ..AgentConfig::default()
    };
    let params = c.to_params();
    for key in ["max_agents", "memory_keep_last"] {
        let v = &params.iter().find(|(k, _)| k == key).unwrap().1;
        assert_eq!(*v, Value::Int(i64::MAX), "{key}");
    }
    // Lo escrito vuelve a leerse sin error.
    assert!(AgentConfig::from_params(&params).is_ok());
}
