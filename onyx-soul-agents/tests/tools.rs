//! `ToolSystem` y `ToolExecutor`: ejecución determinista dentro del cerebro.
//! Solo se usa la API pública del soul.

use onyx_soul_agents::{
    AgentConfig, AgentGraph, AgentRuntime, AgentSpawner, AgentSpec, AgentState, Attr, AttrOnly,
    AttrRegistry, ConceptId, Ctx, DetRng, DomainEvent, Graph, GraphView, OnyxEvent, OnyxSystem,
    Reply, SystemEvent, TOOLS_NAME, ToolCall, ToolCtx, ToolExecutor, ToolSystem, Value, Vocab,
    spawn_event,
};

struct Echo(&'static str);

impl ToolExecutor for Echo {
    fn name(&self) -> &str {
        self.0
    }

    fn call(&mut self, call: &ToolCall, _: &mut ToolCtx) -> Result<String, String> {
        Ok(format!("{}:{}:{}", self.0, call.request, call.args))
    }
}

struct Boom;

impl ToolExecutor for Boom {
    fn name(&self) -> &str {
        "boom"
    }

    fn call(&mut self, _: &ToolCall, _: &mut ToolCtx) -> Result<String, String> {
        Err("boom failed".into())
    }
}

struct Named(&'static str, &'static str);

impl ToolExecutor for Named {
    fn name(&self) -> &str {
        self.0
    }

    fn call(&mut self, _: &ToolCall, _: &mut ToolCtx) -> Result<String, String> {
        Ok(self.1.into())
    }
}

struct Scratch {
    mark: Option<Attr>,
}

impl Scratch {
    fn new() -> Self {
        Self { mark: None }
    }
}

impl ToolExecutor for Scratch {
    fn name(&self) -> &str {
        "scratch"
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) {
        self.mark = Some(attrs.attr("test.mark"));
    }

    fn call(&mut self, call: &ToolCall, ctx: &mut ToolCtx) -> Result<String, String> {
        let mark = self.mark.expect("init debía registrar test.mark");
        match call.args.as_str() {
            "write" => {
                ctx.graph.set(call.agent, mark, Value::Text("seen".into()));
                Ok("wrote".into())
            }
            "read" => match ctx.graph.get(call.agent, mark) {
                Some(Value::Text(s)) => Ok(s.clone()),
                _ => Ok("empty".into()),
            },
            other => Err(format!("unknown {other}")),
        }
    }
}

struct Inited {
    n: u32,
}

impl ToolExecutor for Inited {
    fn name(&self) -> &str {
        "inited"
    }

    fn init(&mut self, _: &mut dyn AttrRegistry) {
        self.n = self.n.saturating_add(1);
    }

    fn call(&mut self, _: &ToolCall, _: &mut ToolCtx) -> Result<String, String> {
        Ok(self.n.to_string())
    }
}

struct Dice;

impl ToolExecutor for Dice {
    fn name(&self) -> &str {
        "dice"
    }

    fn call(&mut self, _: &ToolCall, ctx: &mut ToolCtx) -> Result<String, String> {
        Ok(ctx.rng.gen_range(0, 100).to_string())
    }
}

struct Reporter;

impl ToolExecutor for Reporter {
    fn name(&self) -> &str {
        "reporter"
    }

    fn call(&mut self, _: &ToolCall, ctx: &mut ToolCtx) -> Result<String, String> {
        Ok(ctx.tick.to_string())
    }
}

struct MirrorTool;

impl ToolExecutor for MirrorTool {
    fn name(&self) -> &str {
        "search"
    }

    fn call(&mut self, call: &ToolCall, _: &mut ToolCtx) -> Result<String, String> {
        Ok(call.tool.clone())
    }
}

struct Harness {
    g: AgentGraph,
    v: Vocab,
    tools: ToolSystem,
    n: u64,
    seed: u64,
}

fn cfg() -> AgentConfig {
    AgentConfig::default()
}

impl Harness {
    fn new(tools: ToolSystem) -> Self {
        let mut g = AgentGraph::new();
        let mut tools = tools;
        tools.init(&mut AttrOnly(&mut g)).unwrap();
        let v = Vocab::resolve_with(&mut g, &cfg().vocab_prefix);
        Self {
            g,
            v,
            tools,
            n: 0,
            seed: 1,
        }
    }

    fn with(tool: Box<dyn ToolExecutor>) -> Self {
        Self::new(ToolSystem::new(cfg()).with_tool(tool))
    }

    fn agent(&mut self) -> ConceptId {
        self.g.create()
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
        self.tools.run(&mut Ctx {
            graph: &mut self.g,
            inbox,
            outbox: &mut outbox,
            rng: &mut rng,
        });
        outbox
    }
}

fn call(
    v: &Vocab,
    agent: Option<ConceptId>,
    request: Value,
    tool: Option<Value>,
    extra: Vec<(Attr, Value)>,
) -> OnyxEvent {
    let mut data = vec![(v.request, request)];
    if let Some(tool) = tool {
        data.push((v.tool, tool));
    }
    data.extend(extra);
    OnyxEvent::Domain(DomainEvent {
        kind: v.ev_tool_call,
        subject: agent,
        data,
    })
}

fn echo_call(v: &Vocab, agent: ConceptId, request: i64, tool: &str, args: &str) -> OnyxEvent {
    call(
        v,
        Some(agent),
        Value::Int(request),
        Some(Value::Text(tool.into())),
        vec![(v.args, Value::Text(args.into()))],
    )
}

fn payload(d: &DomainEvent, key: Attr) -> Option<&Value> {
    d.data
        .iter()
        .rev()
        .find(|(k, _)| *k == key)
        .map(|(_, val)| val)
}

fn results(v: &Vocab, out: &[OnyxEvent]) -> Vec<(ConceptId, i64, bool, String)> {
    out.iter()
        .filter_map(|e| match e {
            OnyxEvent::Domain(d) if d.kind == v.ev_tool_result => {
                let agent = d.subject?;
                let request = match payload(d, v.request) {
                    Some(Value::Int(n)) => *n,
                    _ => return None,
                };
                let ok = match payload(d, v.ok) {
                    Some(Value::Bool(b)) => *b,
                    _ => return None,
                };
                let text = match payload(d, v.text) {
                    Some(Value::Text(t)) => t.clone(),
                    _ => String::new(),
                };
                Some((agent, request, ok, text))
            }
            _ => None,
        })
        .collect()
}

fn faults(out: &[OnyxEvent]) -> Vec<String> {
    out.iter()
        .filter_map(|e| match e {
            OnyxEvent::System(SystemEvent::Fault(m)) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_registered_tool_emits_an_ok_result_with_the_call_fields() {
    let mut h = Harness::with(Box::new(Echo("search")));
    let id = h.agent();
    let out = h.run(vec![echo_call(&h.v, id, 3, "search", "q=onyx")]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert_eq!(
        results(&h.v, &out),
        [(id, 3, true, "search:3:q=onyx".into())]
    );
}

#[test]
fn an_executor_error_emits_ok_false() {
    let mut h = Harness::with(Box::new(Boom));
    let id = h.agent();
    let out = h.run(vec![echo_call(&h.v, id, 1, "boom", "now")]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert_eq!(results(&h.v, &out), [(id, 1, false, "boom failed".into())]);
}

#[test]
fn a_tool_without_executor_is_ignored() {
    let mut h = Harness::with(Box::new(Echo("search")));
    let id = h.agent();
    let out = h.run(vec![echo_call(&h.v, id, 1, "remote", "go")]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert!(results(&h.v, &out).is_empty(), "{out:?}");
}

#[test]
fn an_empty_tool_name_is_unknown_not_malformed() {
    let mut h = Harness::with(Box::new(Echo("search")));
    let id = h.agent();
    let out = h.run(vec![echo_call(&h.v, id, 1, "", "")]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert!(results(&h.v, &out).is_empty(), "{out:?}");
}

#[test]
fn malformed_tool_calls_emit_a_fault() {
    let mut h = Harness::with(Box::new(Echo("search")));
    let id = h.agent();
    let v = h.v;
    let out = h.run(vec![
        call(
            &v,
            None,
            Value::Int(1),
            Some(Value::Text("search".into())),
            vec![],
        ),
        call(&v, Some(id), Value::Int(1), None, vec![]),
        call(
            &v,
            Some(id),
            Value::Text("1".into()),
            Some(Value::Text("search".into())),
            vec![],
        ),
        call(&v, Some(id), Value::Int(1), Some(Value::Int(0)), vec![]),
    ]);
    assert_eq!(faults(&out).len(), 4, "{out:?}");
    assert!(
        faults(&out).iter().all(|m| m == "malformed tool call"),
        "{out:?}"
    );
    assert!(results(&h.v, &out).is_empty(), "{out:?}");
}

#[test]
fn missing_or_non_text_args_default_to_empty() {
    let mut h = Harness::with(Box::new(Echo("search")));
    let id = h.agent();
    let v = h.v;
    let without = h.run(vec![call(
        &v,
        Some(id),
        Value::Int(1),
        Some(Value::Text("search".into())),
        vec![],
    )]);
    assert_eq!(results(&h.v, &without), [(id, 1, true, "search:1:".into())]);

    let as_int = h.run(vec![call(
        &v,
        Some(id),
        Value::Int(2),
        Some(Value::Text("search".into())),
        vec![(v.args, Value::Int(7))],
    )]);
    assert_eq!(results(&h.v, &as_int), [(id, 2, true, "search:2:".into())]);
}

#[test]
fn duplicate_keys_on_a_tool_call_are_last_wins() {
    let mut h = Harness::with(Box::new(Echo("search")));
    let id = h.agent();
    let v = h.v;
    let out = h.run(vec![OnyxEvent::Domain(DomainEvent {
        kind: v.ev_tool_call,
        subject: Some(id),
        data: vec![
            (v.request, Value::Int(1)),
            (v.tool, Value::Text("remote".into())),
            (v.args, Value::Text("first".into())),
            (v.request, Value::Int(9)),
            (v.tool, Value::Text("search".into())),
            (v.args, Value::Text("second".into())),
        ],
    })]);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert_eq!(
        results(&h.v, &out),
        [(id, 9, true, "search:9:second".into())],
        "subject no se duplica; request, tool y args sí, y gana la última"
    );
}

#[test]
fn the_first_registered_executor_wins_for_a_repeated_name() {
    let tools = ToolSystem::new(cfg())
        .with_tool(Box::new(Named("echo", "first")))
        .with_tool(Box::new(Named("echo", "second")));
    let mut h = Harness::new(tools);
    let id = h.agent();
    let out = h.run(vec![echo_call(&h.v, id, 1, "echo", "")]);
    assert_eq!(results(&h.v, &out), [(id, 1, true, "first".into())]);
}

#[test]
fn several_calls_are_handled_in_inbox_order() {
    let mut h = Harness::with(Box::new(Echo("search")));
    let a = h.agent();
    let b = h.agent();
    let out = h.run(vec![
        echo_call(&h.v, a, 1, "search", "one"),
        echo_call(&h.v, b, 2, "search", "two"),
    ]);
    assert_eq!(
        results(&h.v, &out),
        [
            (a, 1, true, "search:1:one".into()),
            (b, 2, true, "search:2:two".into())
        ]
    );
}

#[test]
fn an_executor_sees_graph_writes_from_an_earlier_call() {
    let mut h = Harness::with(Box::new(Scratch::new()));
    let id = h.agent();
    let out = h.run(vec![
        echo_call(&h.v, id, 1, "scratch", "write"),
        echo_call(&h.v, id, 2, "scratch", "read"),
    ]);
    assert_eq!(
        results(&h.v, &out),
        [(id, 1, true, "wrote".into()), (id, 2, true, "seen".into())]
    );
}

#[test]
fn executor_rng_is_deterministic_for_the_same_seed_and_tick() {
    let mut a = Harness::with(Box::new(Dice));
    let mut b = Harness::with(Box::new(Dice));
    a.seed = 42;
    b.seed = 42;
    let id_a = a.agent();
    let id_b = b.agent();
    let out_a = a.run(vec![echo_call(&a.v, id_a, 1, "dice", "")]);
    let out_b = b.run(vec![echo_call(&b.v, id_b, 1, "dice", "")]);
    assert_eq!(results(&a.v, &out_a), results(&b.v, &out_b));
    assert_ne!(
        results(&a.v, &out_a)[0].3,
        "",
        "el ejecutor debía producir un número"
    );
}

#[test]
fn the_executor_receives_the_tool_name_from_the_call() {
    let mut h = Harness::with(Box::new(MirrorTool));
    let id = h.agent();
    let out = h.run(vec![echo_call(&h.v, id, 1, "search", "ignored")]);
    assert_eq!(results(&h.v, &out), [(id, 1, true, "search".into())]);
}

#[test]
fn executor_rng_differs_across_seeds() {
    let mut a = Harness::with(Box::new(Dice));
    let mut b = Harness::with(Box::new(Dice));
    a.seed = 1;
    b.seed = 2;
    let id_a = a.agent();
    let id_b = b.agent();
    let va = a.v;
    let vb = b.v;
    let calls_a: Vec<_> = (1..=4)
        .map(|i| echo_call(&va, id_a, i, "dice", ""))
        .collect();
    let calls_b: Vec<_> = (1..=4)
        .map(|i| echo_call(&vb, id_b, i, "dice", ""))
        .collect();
    let out_a = a.run(calls_a);
    let out_b = b.run(calls_b);
    let seq_a: Vec<_> = results(&va, &out_a).into_iter().map(|r| r.3).collect();
    let seq_b: Vec<_> = results(&vb, &out_b).into_iter().map(|r| r.3).collect();
    assert_eq!(seq_a.len(), 4, "{seq_a:?}");
    assert_eq!(seq_b.len(), 4, "{seq_b:?}");
    assert_ne!(
        seq_a, seq_b,
        "distintas semillas deben producir distintas secuencias"
    );
}

#[test]
fn init_runs_again_when_the_system_is_reinitialized() {
    let mut h = Harness::with(Box::new(Inited { n: 0 }));
    let id = h.agent();
    let first = h.run(vec![echo_call(&h.v, id, 1, "inited", "")]);
    assert_eq!(results(&h.v, &first), [(id, 1, true, "1".into())]);
    h.tools.init(&mut AttrOnly(&mut h.g)).unwrap();
    let second = h.run(vec![echo_call(&h.v, id, 2, "inited", "")]);
    assert_eq!(
        results(&h.v, &second),
        [(id, 2, true, "2".into())],
        "el tick vuelve a llamar init tras un import; aquí se simula esa segunda llamada"
    );
}

#[test]
fn tick_in_tool_ctx_comes_from_the_inbox_or_zero() {
    let mut h = Harness::with(Box::new(Reporter));
    let id = h.agent();
    let with_tick = h.run(vec![echo_call(&h.v, id, 1, "reporter", "")]);
    assert_eq!(results(&h.v, &with_tick), [(id, 1, true, "1".into())]);

    let without = h.run_inbox(&[echo_call(&h.v, id, 2, "reporter", "")]);
    assert_eq!(
        results(&h.v, &without),
        [(id, 2, true, "0".into())],
        "sin Tick el ejecutor ve tick 0; la llamada no se descarta"
    );
}

#[test]
fn a_negative_request_is_preserved() {
    let mut h = Harness::with(Box::new(Echo("search")));
    let id = h.agent();
    let out = h.run(vec![echo_call(&h.v, id, -3, "search", "x")]);
    assert_eq!(results(&h.v, &out), [(id, -3, true, "search:-3:x".into())]);
}

struct Integrated {
    g: AgentGraph,
    v: Vocab,
    spawner: AgentSpawner,
    runtime: AgentRuntime,
    tools: ToolSystem,
    n: u64,
    seed: u64,
}

impl Integrated {
    fn new() -> Self {
        let cfg = AgentConfig {
            default_max_steps: 8,
            default_timeout_ticks: 4,
            ..AgentConfig::default()
        };
        let mut g = AgentGraph::new();
        let mut spawner = AgentSpawner::new(cfg.clone());
        let mut runtime = AgentRuntime::new(cfg.clone());
        let mut tools = ToolSystem::new(cfg.clone()).with_tool(Box::new(Echo("search")));
        spawner.init(&mut AttrOnly(&mut g)).unwrap();
        runtime.init(&mut AttrOnly(&mut g)).unwrap();
        tools.init(&mut AttrOnly(&mut g)).unwrap();
        let v = Vocab::resolve_with(&mut g, &cfg.vocab_prefix);
        Self {
            g,
            v,
            spawner,
            runtime,
            tools,
            n: 0,
            seed: 1,
        }
    }

    fn run(&mut self, extra: Vec<OnyxEvent>) -> Vec<OnyxEvent> {
        self.n += 1;
        let mut inbox = vec![OnyxEvent::System(SystemEvent::Tick { n: self.n })];
        inbox.extend(extra);
        let mut outbox = Vec::new();
        let mut rng = DetRng::from_seed_and_tick(self.seed, self.n);
        self.spawner.run(&mut Ctx {
            graph: &mut self.g,
            inbox: &inbox,
            outbox: &mut outbox,
            rng: &mut rng,
        });
        self.runtime.run(&mut Ctx {
            graph: &mut self.g,
            inbox: &inbox,
            outbox: &mut outbox,
            rng: &mut rng,
        });
        self.tools.run(&mut Ctx {
            graph: &mut self.g,
            inbox: &inbox,
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

    fn state(&self, id: ConceptId) -> AgentState {
        match self.g.get(id, self.v.state) {
            Some(Value::Text(s)) => AgentState::parse(&s).expect("estado conocido"),
            other => panic!("sin estado: {other:?}"),
        }
    }

    fn memory(&self, id: ConceptId) -> Vec<(String, String)> {
        self.g
            .related(id, self.v.rel_memory)
            .into_iter()
            .map(|m| {
                let kind = match self.g.get(m, self.v.mem_kind) {
                    Some(Value::Text(s)) => s,
                    other => panic!("mem_kind: {other:?}"),
                };
                let text = match self.g.get(m, self.v.mem_text) {
                    Some(Value::Text(s)) => s,
                    other => panic!("mem_text: {other:?}"),
                };
                (kind, text)
            })
            .collect()
    }
}

fn tool_calls(v: &Vocab, out: &[OnyxEvent]) -> Vec<(ConceptId, String, String)> {
    out.iter()
        .filter_map(|e| match e {
            OnyxEvent::Domain(d) if d.kind == v.ev_tool_call => {
                let agent = d.subject?;
                let tool = match payload(d, v.tool) {
                    Some(Value::Text(t)) => t.clone(),
                    _ => return None,
                };
                let args = match payload(d, v.args) {
                    Some(Value::Text(t)) => t.clone(),
                    _ => String::new(),
                };
                Some((agent, tool, args))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_runtime_tool_call_is_answered_two_ticks_later() {
    let mut w = Integrated::new();
    let id = w.spawn(&AgentSpec::new("alice", "g").with_tools(["search"]));
    let out = w.run(vec![Reply::call(id, 1, "search hello world").to_event()]);
    assert_eq!(
        tool_calls(&w.v, &out),
        [(id, "search".into(), "hello world".into())]
    );
    assert!(
        results(&w.v, &out).is_empty(),
        "mismo inbox: tools aún no ve el call"
    );
    assert_eq!(w.state(id), AgentState::Acting);
    assert_eq!(w.memory(id), [("call".into(), "search hello world".into())]);

    let pending: Vec<_> = out
        .into_iter()
        .filter(|e| matches!(e, OnyxEvent::Domain(d) if d.kind == w.v.ev_tool_call))
        .collect();
    let out = w.run(pending);
    assert_eq!(
        results(&w.v, &out),
        [(id, 1, true, "search:1:hello world".into())]
    );
    assert_eq!(
        w.state(id),
        AgentState::Acting,
        "el resultado llega el tick siguiente"
    );

    let pending: Vec<_> = out
        .into_iter()
        .filter(|e| matches!(e, OnyxEvent::Domain(d) if d.kind == w.v.ev_tool_result))
        .collect();
    let out = w.run(pending);
    assert!(faults(&out).is_empty(), "{out:?}");
    assert_eq!(
        w.memory(id),
        [
            ("call".into(), "search hello world".into()),
            ("observe".into(), "search:1:hello world".into())
        ]
    );
    assert_eq!(w.state(id), AgentState::Thinking);
}

#[test]
fn factories_do_not_include_the_tool_system() {
    assert!(
        onyx_soul_agents::FACTORIES
            .iter()
            .all(|(kind, _)| *kind != TOOLS_NAME),
        "ToolSystem lo aporta la app, no el loader"
    );
}
