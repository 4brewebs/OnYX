//! Los campos de control de `OperationLog` frente al resto del soul: extras de
//! agente (`spawn_event` y evento crudo ante `AgentSpawner`), cupo, padres,
//! herramientas, `pending_requests` y `AgentView`. Solo API pública del soul.
//! No prueba despacho ni recuperación de efectos externos: aún no existen.
use std::num::NonZeroU64;

use onyx_soul_agents::{
    AgentBirth, AgentConfig, AgentGraph, AgentHost, AgentSpawner, AgentSpec, AgentState, Attr,
    AttrOnly, AttrRegistry, ConceptId, Ctx, DetRng, DomainEvent, EventBus, ExecutionId, GraphView,
    OnyxError, OnyxEvent, OnyxSystem, OperationKey, OperationLog, OperationState, PlatformDriver,
    SurfaceBinding, SystemEvent, ToolDescriptor, ToolOutcome, Value, Vocab, pending_requests,
    spawn_event,
};

/// Campos de control documentados en `operation.rs` (`<prefix>.operation.*`).
const FIELDS: [&str; 10] = [
    "schema",
    "execution",
    "birth",
    "request",
    "tool",
    "args",
    "route",
    "state",
    "outcome_kind",
    "outcome_text",
];

fn field(g: &mut AgentGraph, prefix: &str, f: &str) -> Attr {
    g.attr(&format!("{prefix}.operation.{f}"))
}

/// `birth` es un nacimiento lógico de prueba, no un `ConceptId`.
fn key(birth: u64, request: u64) -> OperationKey {
    OperationKey::new(
        ExecutionId::new("run-1").unwrap(),
        AgentBirth::new(NonZeroU64::new(birth).unwrap()),
        NonZeroU64::new(request).unwrap(),
    )
}
fn desc(tool: &str, args: &str) -> ToolDescriptor {
    ToolDescriptor::new(tool, args, "ext/route").unwrap()
}
fn text(s: &str) -> Value {
    Value::Text(s.into())
}

/// Spawner inicializado sobre `g` con el vocabulario de `prefix`.
fn spawner(g: &mut AgentGraph, prefix: &str, max_agents: usize) -> (AgentSpawner, Vocab) {
    let config = AgentConfig {
        vocab_prefix: prefix.into(),
        max_agents,
        ..AgentConfig::default()
    };
    let mut s = AgentSpawner::new(config);
    s.init(&mut AttrOnly(g)).unwrap();
    (s, Vocab::resolve_with(g, prefix))
}

fn run(s: &mut AgentSpawner, g: &mut AgentGraph, events: Vec<OnyxEvent>) -> Vec<OnyxEvent> {
    let mut inbox = vec![OnyxEvent::System(SystemEvent::Tick { n: 1 })];
    inbox.extend(events);
    let mut outbox = Vec::new();
    let mut rng = DetRng::from_seed_and_tick(0, 0);
    s.run(&mut Ctx {
        graph: g,
        inbox: &inbox,
        outbox: &mut outbox,
        rng: &mut rng,
    });
    outbox
}

/// Agente creado, o motivo del rechazo.
fn spawned(v: &Vocab, out: &[OnyxEvent]) -> Result<ConceptId, String> {
    match out {
        [OnyxEvent::Domain(d)] if d.kind == v.ev_spawned => Ok(d.subject.unwrap()),
        [OnyxEvent::Domain(d)] if d.kind == v.ev_rejected => Err(d
            .data
            .iter()
            .find_map(|(k, x)| match x {
                Value::Text(r) if *k == v.reason => Some(r.clone()),
                _ => None,
            })
            .unwrap()),
        other => panic!("salida inesperada: {other:?}"),
    }
}

fn raw_spawn(v: &Vocab, subject: Option<ConceptId>, extra: Vec<(Attr, Value)>) -> OnyxEvent {
    let mut data = vec![(v.name, text("a")), (v.goal, text("g"))];
    data.extend(extra);
    OnyxEvent::Domain(DomainEvent {
        kind: v.ev_spawn,
        subject,
        data,
    })
}

fn event_data(e: &OnyxEvent) -> &[(Attr, Value)] {
    match e {
        OnyxEvent::Domain(d) => &d.data,
        other => panic!("{other:?}"),
    }
}

// ---------- conjunto reservado ----------

#[test]
fn every_operation_control_field_is_reserved_under_its_own_prefix_only() {
    let mut g = AgentGraph::new();
    let agent = Vocab::resolve(&mut g);
    let team = Vocab::resolve_with(&mut g, "team");
    for f in FIELDS {
        let mine = field(&mut g, "agent", f);
        let theirs = field(&mut g, "team", f);
        assert!(agent.is_reserved(mine), "agent.operation.{f}");
        assert!(agent.operations.is_reserved(mine), "agent.operation.{f}");
        assert!(team.is_reserved(theirs), "team.operation.{f}");
        assert!(
            !agent.is_reserved(theirs),
            "team.operation.{f} no es de agent"
        );
        assert!(!team.operations.is_reserved(mine));
    }
    // Vecinos que no son campos de control.
    for name in [
        "agent.operation",
        "agent.operations.state",
        "app.operation.state",
    ] {
        let a = g.attr(name);
        assert!(!agent.is_reserved(a), "{name}");
    }
    // El estado de agente sigue reservado, pero no es un campo de operación:
    // un registro de operación no se confunde con un agente.
    assert!(agent.is_reserved(agent.state));
    assert!(!agent.operations.is_reserved(agent.state));
    assert!(!agent.operations.is_reserved(agent.tool));
    assert!(!agent.operations.is_reserved(agent.args));
}

#[test]
fn vocab_operations_is_the_same_log_as_resolving_with_the_prefix() {
    for prefix in ["agent", "team"] {
        let mut g = AgentGraph::new();
        let v = Vocab::resolve_with(&mut g, prefix);
        let direct = OperationLog::resolve_with(&mut g, prefix);
        let k = key(7, 1);
        v.operations
            .record(&mut g, k.clone(), desc("t", "a"))
            .unwrap();
        let other = if prefix == "agent" { "team" } else { "agent" };
        let foreign = OperationLog::resolve_with(&mut g, other);
        assert_eq!(
            direct.entries(&g).unwrap(),
            v.operations.entries(&g).unwrap()
        );
        assert_eq!(direct.entries(&g).unwrap().len(), 1, "{prefix}");
        assert!(foreign.entries(&g).unwrap().is_empty(), "{prefix}");
    }
}

// ---------- extras de agente ----------

#[test]
fn spawn_event_drops_each_operation_field_and_keeps_app_extras() {
    for prefix in ["agent", "team"] {
        let mut g = AgentGraph::new();
        let v = Vocab::resolve_with(&mut g, prefix);
        let note = g.attr("app.note");
        for f in FIELDS {
            let a = field(&mut g, prefix, f);
            let spec = AgentSpec::new("a", "g")
                .with_extra(a, text("forged"))
                .with_extra(note, text("kept"));
            let ev = spawn_event(&v, &spec, None);
            let data = event_data(&ev);
            assert!(
                data.iter().all(|(k, _)| *k != a),
                "{prefix}.operation.{f} pasó al evento"
            );
            assert!(data.contains(&(note, text("kept"))));
        }
    }
}

#[test]
fn raw_spawn_never_copies_an_operation_field_onto_the_agent() {
    for prefix in ["agent", "team"] {
        for f in FIELDS {
            let mut g = AgentGraph::new();
            let (mut s, v) = spawner(&mut g, prefix, 8);
            let a = field(&mut g, prefix, f);
            let note = g.attr("app.note");
            let out = run(
                &mut s,
                &mut g,
                vec![raw_spawn(
                    &v,
                    None,
                    vec![(a, text("forged")), (note, text("kept"))],
                )],
            );
            let id = spawned(&v, &out).unwrap();
            assert_eq!(g.get(id, a), None, "{prefix}.operation.{f} copiado");
            assert_eq!(g.get(id, note), Some(text("kept")));
            assert_eq!(v.operations.entries(&g).unwrap(), vec![]);
        }
    }
}

/// Copia literal de los campos de un registro real como extras.
fn forged_record(g: &mut AgentGraph, prefix: &str, id: ConceptId) -> Vec<(Attr, Value)> {
    let mut out = Vec::new();
    for f in FIELDS {
        let a = field(g, prefix, f);
        out.push((a, g.get(id, a).expect("campo presente")));
    }
    out
}

#[test]
fn a_complete_forged_record_in_extras_cannot_duplicate_or_corrupt_the_log() {
    for prefix in ["agent", "team"] {
        for via_helper in [false, true] {
            let mut g = AgentGraph::new();
            let (mut s, v) = spawner(&mut g, prefix, 8);
            let k = key(42, 3);
            let op = v
                .operations
                .record(&mut g, k.clone(), desc("t", "a"))
                .unwrap();
            v.operations
                .complete(&mut g, &k, ToolOutcome::Success("r".into()))
                .unwrap();
            let before = v.operations.entries(&g).unwrap();
            let extra = forged_record(&mut g, prefix, op.id);
            let ev = if via_helper {
                let mut spec = AgentSpec::new("a", "g");
                spec.extra = extra;
                spawn_event(&v, &spec, None)
            } else {
                raw_spawn(&v, None, extra)
            };
            let id = spawned(&v, &run(&mut s, &mut g, vec![ev])).unwrap();
            // Sin el filtro, el agente sería un segundo registro con la misma clave.
            assert_eq!(v.operations.entries(&g), Ok(before.clone()), "{prefix}");
            assert_eq!(
                v.operations
                    .complete(&mut g, &k, ToolOutcome::Success("r".into())),
                Ok(before[0].clone())
            );
            assert_eq!(g.get(id, v.state), Some(text(AgentState::Idle.as_str())));
        }
    }
}

// ---------- cupo, padres y herramientas ----------

#[test]
fn operations_are_not_agents_for_quota_parenting_or_tool_lookup() {
    let mut g = AgentGraph::new();
    let (mut s, v) = spawner(&mut g, "agent", 1);
    let mut ops = Vec::new();
    for r in 1..=4 {
        let k = key(9, r);
        let op = v
            .operations
            .record(&mut g, k.clone(), desc("search", "q"))
            .unwrap();
        if r % 2 == 0 {
            v.operations.mark_uncertain(&mut g, &k).unwrap();
        }
        ops.push(op);
    }
    let before = v.operations.entries(&g).unwrap();
    let spec = AgentSpec::new("a", "g").with_tools(["search"]);
    // El sujeto es un registro de operación: no es un padre válido.
    let ev = spawn_event(&v, &spec, Some(ops[0].id));
    let id = spawned(&v, &run(&mut s, &mut g, vec![ev])).expect("cupo libre pese a 4 operaciones");
    assert!(g.related(id, v.rel_parent).is_empty(), "padre = operación");
    let tools = g.related(id, v.rel_tool);
    assert_eq!(tools.len(), 1);
    assert!(
        ops.iter().all(|o| o.id != tools[0]),
        "herramienta = operación"
    );
    assert_eq!(g.get(tools[0], v.tool_name), Some(text("search")));
    for op in &ops {
        assert!(g.links(op.id).is_empty());
    }
    assert_eq!(v.operations.entries(&g).unwrap(), before);
    // Control: el cupo sí ve al agente real.
    let again = spawn_event(&v, &AgentSpec::new("b", "g"), None);
    assert_eq!(
        spawned(&v, &run(&mut s, &mut g, vec![again])),
        Err("max_agents".into())
    );
}

// ---------- pending_requests y AgentView por el host ----------

struct Quiet;
impl PlatformDriver for Quiet {
    fn poll(&mut self, _: &mut dyn EventBus) {}
    fn present(&mut self, _: &dyn GraphView, _: &[SurfaceBinding]) {}
}

/// Sistema de la app que registra operaciones para cada agente vivo.
struct Recorder {
    v: Option<Vocab>,
}
impl OnyxSystem for Recorder {
    fn name(&self) -> &str {
        "op_recorder"
    }
    fn init(&mut self, r: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.v = Some(Vocab::resolve_with(r, "team"));
        Ok(())
    }
    fn run(&mut self, ctx: &mut Ctx) {
        let v = self.v.unwrap();
        for agent in ctx.graph.query(&[v.state]) {
            for (r, state) in [(1, "pending"), (2, "uncertain"), (3, "resolved")] {
                // Agente heredado sin raíz: la prueba usa su número como
                // nacimiento de prueba; aquí solo importa no mezclar registros.
                let k = key(agent.0, r);
                let found = v.operations.find(&*ctx.graph, &k).unwrap();
                if found.is_some() {
                    continue;
                }
                let args = format!("SECRET-ARGS-{state}");
                v.operations
                    .record(ctx.graph, k.clone(), desc("search", &args))
                    .unwrap();
                match state {
                    "uncertain" => drop(v.operations.mark_uncertain(ctx.graph, &k).unwrap()),
                    "resolved" => drop(
                        v.operations
                            .complete(ctx.graph, &k, ToolOutcome::Success("SECRET-RESULT".into()))
                            .unwrap(),
                    ),
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn operations_are_not_listed_as_agents_requests_or_view_context() {
    let config = AgentConfig {
        vocab_prefix: "team".into(),
        ..AgentConfig::default()
    };
    let mut host = AgentHost::builder(1)
        .config(config)
        .system(Box::new(Recorder { v: None }))
        .build()
        .unwrap();
    host.spawn(&AgentSpec::new("a", "g").with_tools(["search"]));
    host.run(&mut Quiet, 3);
    let v = host.vocab();
    let agents = host.agents();
    assert_eq!(agents.len(), 1, "las operaciones no son agentes");
    let agent = agents[0];
    let ops = v.operations.entries(host.graph()).unwrap();
    assert_eq!(ops.len(), 3, "el sistema registró sus operaciones");
    assert_eq!(
        ops.iter().map(|o| o.state.clone()).collect::<Vec<_>>(),
        vec![
            OperationState::Pending,
            OperationState::Uncertain,
            OperationState::Resolved(ToolOutcome::Success("SECRET-RESULT".into()))
        ]
    );
    // Control: el agente externo sí está esperando decisión.
    assert_eq!(host.agent_state(agent), Some(AgentState::Thinking));
    let requests = pending_requests(host.graph(), &v);
    assert_eq!(
        requests.len(),
        1,
        "una operación pendiente no es una petición"
    );
    assert_eq!(requests[0].agent, agent);
    let view = host.view(agent);
    assert_eq!(requests[0], view);
    let leaked = |s: &str| s.contains("SECRET") || s.contains(".operation.");
    assert!(!view.memory.iter().any(|(k, t)| leaked(k) || leaked(t)));
    assert!(
        !view
            .props
            .iter()
            .any(|(n, x)| { leaked(n) || matches!(x, Value::Text(t) if leaked(t)) })
    );
    assert_eq!(view.tools, vec!["search".to_string()]);
    assert!(!host.is_settled());
}
