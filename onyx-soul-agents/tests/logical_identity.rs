//! Raíz de ejecución explícita y nacimiento lógico de agentes (unidad 1–4).
//!
//! Solo API pública del soul. Lo que aquí pasa no habilita efectos externos:
//! no hay receptor, despachador único, deduplicación por envío ni TR2. Un
//! contador igual no prueba que una continuación sea el mismo trabajo.
//!
//! Pendiente fuera de esta unidad (sin prueba verde aquí a propósito):
//! snapshot entre las operaciones 1 y 2 del mismo agente (rebobina `request`),
//! rama divergente con descriptor igual, receptor sin inventario y segundo
//! despachador. `execution_root` puede dar `Ok` en esos casos; eso no es una
//! garantía de esta unidad.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::rc::Rc;

use onyx_soul_agents::{
    AgentBirth, AgentConfig, AgentGraph, AgentHost, AgentIdentity, AgentSpawner, AgentSpec, Attr,
    AttrOnly, AttrRegistry, ConceptId, Ctx, DefaultMemory, DetRng, DomainEvent, EventBus,
    ExecutionConfig, ExecutionId, Graph, GraphView, OnyxError, OnyxEvent, OnyxSystem,
    OperationError, OperationKey, PlatformDriver, ROOT_NAMESPACE, RootError, SnapshotStore,
    SurfaceBinding, SystemEvent, ToolDescriptor, Value, Vocab, begin_event, execution_root,
    spawn_event,
};

// ---------- utilidades ----------

type Fingerprint = Vec<(u64, Vec<(String, Value)>, Vec<(String, ConceptId)>)>;

fn fingerprint(g: &dyn GraphView) -> Fingerprint {
    g.query(&[])
        .into_iter()
        .map(|id| (id.0, g.props(id), g.links(id)))
        .collect()
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn exec(id: &str, compat: &str) -> ExecutionConfig {
    ExecutionConfig::new(ExecutionId::new(id).unwrap(), compat).unwrap()
}

fn cfg(prefix: &str, execution: Option<ExecutionConfig>) -> AgentConfig {
    AgentConfig {
        vocab_prefix: prefix.into(),
        max_agents: 8,
        default_max_steps: 5,
        default_timeout_ticks: 3,
        memory_keep_last: 0,
        execution,
    }
}

fn run1() -> AgentConfig {
    cfg("agent", Some(exec("run-1", "app-v1")))
}

fn nz(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).unwrap()
}

/// Atributo fijo `onyx.agents.<name>`.
fn fixed(g: &mut AgentGraph, name: &str) -> Attr {
    g.attr(&format!("{ROOT_NAMESPACE}.{name}"))
}

const ROOT_FIELDS: [&str; 6] = [
    "root.schema",
    "root.execution",
    "root.prefix",
    "root.config",
    "root.compat",
    "root.next_birth",
];
const AGENT_FIELDS: [&str; 2] = ["agent.execution", "agent.birth"];

/// Grafo con un spawner real y su vocabulario.
struct World {
    g: AgentGraph,
    s: AgentSpawner,
    v: Vocab,
    c: AgentConfig,
}

impl World {
    fn new(c: AgentConfig) -> Self {
        Self::on(AgentGraph::new(), c)
    }
    fn on(mut g: AgentGraph, c: AgentConfig) -> Self {
        let mut s = AgentSpawner::new(c.clone());
        s.init(&mut AttrOnly(&mut g)).unwrap();
        let v = Vocab::resolve_with(&mut g, &c.vocab_prefix);
        Self { g, s, v, c }
    }
    /// Otro spawner (otra config) sobre el mismo grafo.
    fn with_config(self, c: AgentConfig) -> Self {
        Self::on(self.g, c)
    }
    fn run(&mut self, events: Vec<OnyxEvent>) -> Vec<OnyxEvent> {
        let mut inbox = vec![OnyxEvent::System(SystemEvent::Tick { n: 1 })];
        inbox.extend(events);
        let mut outbox = Vec::new();
        let mut rng = DetRng::from_seed_and_tick(0, 0);
        self.s.run(&mut Ctx {
            graph: &mut self.g,
            inbox: &inbox,
            outbox: &mut outbox,
            rng: &mut rng,
        });
        outbox
    }
    fn reason(&self, d: &DomainEvent) -> String {
        d.data
            .iter()
            .find_map(|(k, x)| match x {
                Value::Text(r) if *k == self.v.reason => Some(r.clone()),
                _ => None,
            })
            .expect("motivo")
    }
    /// Raíz creada, o motivo del rechazo.
    fn begin(&mut self) -> Result<ConceptId, String> {
        let ev = begin_event(&self.v);
        match self.run(vec![ev]).as_slice() {
            [OnyxEvent::Domain(d)] if d.kind == self.v.ev_begun => Ok(d.subject.unwrap()),
            [OnyxEvent::Domain(d)] if d.kind == self.v.ev_begin_rejected => Err(self.reason(d)),
            other => panic!("salida de begin inesperada: {other:?}"),
        }
    }
    fn spawn_ev(&mut self, ev: OnyxEvent) -> Result<ConceptId, String> {
        match self.run(vec![ev]).as_slice() {
            [OnyxEvent::Domain(d)] if d.kind == self.v.ev_spawned => Ok(d.subject.unwrap()),
            [OnyxEvent::Domain(d)] if d.kind == self.v.ev_rejected => Err(self.reason(d)),
            other => panic!("salida de spawn inesperada: {other:?}"),
        }
    }
    fn spawn(&mut self, name: &str) -> Result<ConceptId, String> {
        let ev = spawn_event(&self.v, &AgentSpec::new(name, "g"), None);
        self.spawn_ev(ev)
    }
    fn spawn_raw(&mut self, extra: Vec<(Attr, Value)>) -> Result<ConceptId, String> {
        let mut data = vec![(self.v.name, text("a")), (self.v.goal, text("g"))];
        data.extend(extra);
        self.spawn_ev(OnyxEvent::Domain(DomainEvent {
            kind: self.v.ev_spawn,
            subject: None,
            data,
        }))
    }
    fn root(&self) -> Result<onyx_soul_agents::ExecutionRoot, RootError> {
        execution_root(&self.g, &self.v, &self.c)
    }
    fn next_birth(&self) -> u64 {
        self.root().unwrap().next_birth.get().get()
    }
    fn birth(&self, id: ConceptId) -> u64 {
        self.v
            .root
            .identity(&self.g, id)
            .unwrap()
            .expect("identidad")
            .birth
            .get()
            .get()
    }
    fn set_fixed(&mut self, id: ConceptId, name: &str, value: Value) {
        let a = fixed(&mut self.g, name);
        self.g.set(id, a, value);
    }
}

fn identity(execution: &str, birth: u64) -> AgentIdentity {
    AgentIdentity {
        execution: ExecutionId::new(execution).unwrap(),
        birth: AgentBirth::new(nz(birth)),
    }
}

// ---------- AgentBirth y configuración ----------

#[test]
fn agent_birth_starts_at_one_and_displays_canonical_decimal() {
    assert_eq!(AgentBirth::FIRST.get().get(), 1);
    assert_eq!(
        AgentBirth::new(nz(u64::MAX)).to_string(),
        u64::MAX.to_string()
    );
    assert!(AgentBirth::new(nz(2)) > AgentBirth::FIRST);
    assert_eq!(
        ExecutionConfig::new(ExecutionId::new("e").unwrap(), ""),
        Err(RootError::EmptyCompat)
    );
    let e = exec("e", " ");
    assert_eq!((e.execution().as_str(), e.compat()), ("e", " "));
}

#[test]
fn execution_params_go_together_and_round_trip() {
    let with = run1();
    assert_eq!(AgentConfig::from_params(&with.to_params()).unwrap(), with);
    let legacy = cfg("agent", None);
    let names: Vec<_> = legacy.to_params().into_iter().map(|(k, _)| k).collect();
    assert!(!names.iter().any(|n| n.starts_with("execution")));
    let base = legacy.to_params();
    for extra in [
        vec![("execution_id".to_string(), text("e"))],
        vec![("execution_compat".to_string(), text("c"))],
        vec![
            ("execution_id".to_string(), text("")),
            ("execution_compat".to_string(), text("c")),
        ],
        vec![
            ("execution_id".to_string(), text("e")),
            ("execution_compat".to_string(), text("")),
        ],
        vec![
            ("execution_id".to_string(), Value::Int(1)),
            ("execution_compat".to_string(), text("c")),
        ],
    ] {
        let mut p = base.clone();
        p.extend(extra.clone());
        assert!(
            matches!(AgentConfig::from_params(&p), Err(OnyxError::Format(_))),
            "{extra:?}"
        );
    }
}

// ---------- begin explícito ----------

#[test]
fn begin_on_empty_graph_creates_the_single_root() {
    let mut w = World::new(run1());
    let root = w.begin().unwrap();
    let r = w.root().unwrap();
    assert_eq!(r.concept, root);
    assert_eq!(r.execution.as_str(), "run-1");
    assert_eq!(r.next_birth, AgentBirth::FIRST);
    assert_eq!(w.g.query(&[]), vec![root], "begin crea solo la raíz");
}

#[test]
fn begin_is_rejected_without_config_on_non_empty_graph_or_with_a_root() {
    // Sin configuración de ejecución.
    let mut w = World::new(cfg("agent", None));
    assert_eq!(w.begin(), Err("execution_not_configured".into()));
    assert!(w.g.query(&[]).is_empty());

    // Grafo con cualquier concepto, aunque no sea un agente.
    let mut w = World::new(run1());
    w.g.create();
    let before = fingerprint(&w.g);
    assert_eq!(w.begin(), Err("graph_not_empty".into()));
    assert_eq!(fingerprint(&w.g), before);

    // Raíz existente: nunca se repara ni se sustituye.
    let mut w = World::new(run1());
    w.begin().unwrap();
    w.spawn("a").unwrap();
    let before = fingerprint(&w.g);
    assert_eq!(w.begin(), Err("graph_not_empty".into()));
    assert_eq!(fingerprint(&w.g), before);
}

#[derive(Clone, Default)]
struct Shared(Rc<RefCell<BTreeMap<String, Vec<u8>>>>);
impl SnapshotStore for Shared {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        self.0.borrow_mut().insert(key.into(), bytes.to_vec());
        Ok(())
    }
    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> {
        self.0.borrow().get(key).cloned().ok_or(OnyxError::Missing)
    }
}

struct Quiet;
impl PlatformDriver for Quiet {
    fn poll(&mut self, _: &mut dyn EventBus) {}
    fn present(&mut self, _: &dyn GraphView, _: &[SurfaceBinding]) {}
}

fn host(c: AgentConfig, store: &Shared) -> AgentHost {
    AgentHost::builder(1)
        .config(c)
        .store(Box::new(store.clone()))
        .build()
        .unwrap()
}

#[test]
fn begin_and_snapshot_in_the_same_tick_capture_the_root_and_resume_without_begin() {
    let store = Shared::default();
    let mut a = host(run1(), &store);
    a.begin_execution();
    a.request_snapshot("s0");
    a.run(&mut Quiet, 1);
    assert!(a.execution_root().is_ok());

    // Reanudar: host nuevo, sin begin. La raíz viene del snapshot.
    let mut b = host(run1(), &store);
    assert_eq!(b.execution_root(), Err(RootError::Missing));
    b.request_restore("s0");
    b.run(&mut Quiet, 1);
    let r = b.execution_root().unwrap();
    assert_eq!(r.next_birth, AgentBirth::FIRST);
    b.spawn(&AgentSpec::new("a", "g"));
    b.run(&mut Quiet, 1);
    assert_eq!(b.agents().len(), 1);
    assert_eq!(b.execution_root().unwrap().next_birth.get().get(), 2);
}

// ---------- reanudar nunca inicializa ----------

#[test]
fn spawn_without_root_is_rejected_and_never_creates_one() {
    for prefix in ["agent", "team"] {
        let mut w = World::new(cfg(prefix, Some(exec("run-1", "c"))));
        assert_eq!(w.root(), Err(RootError::Missing), "grafo vacío");
        assert_eq!(w.spawn("a"), Err("no_execution_root".into()));
        assert!(w.g.query(&[]).is_empty(), "sin creación perezosa");
        // También con conceptos ajenos en el grafo.
        w.g.create();
        assert_eq!(w.spawn("a"), Err("no_execution_root".into()));
        assert_eq!(w.g.query(&[]).len(), 1);
    }
}

#[test]
fn execution_root_without_config_is_not_configured_even_with_a_root() {
    let mut w = World::new(run1());
    w.begin().unwrap();
    let legacy = cfg("agent", None);
    assert_eq!(
        execution_root(&w.g, &w.v, &legacy),
        Err(RootError::NotConfigured)
    );
}

#[test]
fn duplicate_root_is_rejected_by_validation_and_spawn_without_consuming() {
    let mut w = World::new(run1());
    let first = w.begin().unwrap();
    let copy = w.g.create();
    for (n, v) in w.g.props(first) {
        let a = w.g.attr(&n);
        w.g.set(copy, a, v);
    }
    let before = fingerprint(&w.g);
    assert_eq!(w.root(), Err(RootError::Duplicate(vec![first, copy])));
    assert_eq!(w.spawn("a"), Err("duplicate_execution_root".into()));
    assert_eq!(fingerprint(&w.g), before);
}

#[test]
fn corrupt_root_fields_are_rejected_without_repair() {
    let mut cases: Vec<(&str, Option<Value>)> = ROOT_FIELDS
        .iter()
        .skip(1) // sin schema el concepto no es una raíz: ver la prueba de Missing
        .map(|f| (*f, Some(Value::Int(1))))
        .collect();
    for v in ["root.v2", "", "ROOT.V1"] {
        cases.push(("root.schema", Some(text(v))));
    }
    cases.push(("root.execution", Some(text(""))));
    for v in ["01", "+1", "0", "", " 1", "18446744073709551616", "-1"] {
        cases.push(("root.next_birth", Some(text(v))));
    }
    for (field, value) in cases {
        let mut w = World::new(run1());
        let root = w.begin().unwrap();
        w.set_fixed(root, field, value.clone().unwrap());
        let before = fingerprint(&w.g);
        assert!(
            matches!(w.root(), Err(RootError::Corrupt { concept, .. }) if concept == root),
            "{field}={value:?}: {:?}",
            w.root()
        );
        assert_eq!(
            w.spawn("a"),
            Err("corrupt_execution_root".into()),
            "{field}"
        );
        assert_eq!(fingerprint(&w.g), before, "{field}");
    }
}

#[test]
fn missing_root_field_is_rejected_and_a_root_without_schema_is_missing() {
    for field in ROOT_FIELDS {
        let mut w = World::new(run1());
        let root = w.begin().unwrap();
        // No hay unset: copia la raíz sin ese campo.
        let props: Vec<_> =
            w.g.props(root)
                .into_iter()
                .filter(|(n, _)| *n != format!("{ROOT_NAMESPACE}.{field}"))
                .collect();
        w.g.destroy(root);
        let copy = w.g.create();
        for (n, v) in props {
            let a = w.g.attr(&n);
            w.g.set(copy, a, v);
        }
        let got = w.root();
        if field == "root.schema" {
            assert_eq!(got, Err(RootError::Missing));
        } else {
            assert!(
                matches!(got, Err(RootError::Corrupt { concept, .. }) if concept == copy),
                "{field}: {got:?}"
            );
        }
    }
}

// ---------- compatibilidad ----------

#[test]
fn changing_execution_prefix_config_or_compat_is_incompatible() {
    let base = run1();
    let mut changed: Vec<(&str, AgentConfig)> = vec![
        ("execution", cfg("agent", Some(exec("run-2", "app-v1")))),
        ("compat", cfg("agent", Some(exec("run-1", "app-v2")))),
    ];
    let mut c = base.clone();
    c.max_agents = 9;
    changed.push(("config", c));
    let mut c = base.clone();
    c.default_max_steps = 6;
    changed.push(("config", c));
    let mut c = base.clone();
    c.default_timeout_ticks = 4;
    changed.push(("config", c));
    let mut c = base.clone();
    c.memory_keep_last = 1;
    changed.push(("config", c));
    for (field, other) in changed {
        let mut w = World::new(base.clone());
        w.begin().unwrap();
        w.spawn("a").unwrap();
        let mut w = w.with_config(other.clone());
        let before = fingerprint(&w.g);
        assert!(
            matches!(w.root(), Err(RootError::Incompatible { field: f, .. }) if f == field),
            "{field}: {:?}",
            w.root()
        );
        assert_eq!(w.spawn("b"), Err("incompatible_execution_root".into()));
        assert_eq!(fingerprint(&w.g), before);
        // Control: la config original sigue siendo válida sobre el mismo grafo.
        assert!(execution_root(&w.g, &w.v, &base).is_ok());
    }
}

#[test]
fn a_different_prefix_sees_the_fixed_root_and_rejects_it() {
    let mut w = World::new(run1());
    w.begin().unwrap();
    let mut team = run1();
    team.vocab_prefix = "team".into();
    let mut t = w.with_config(team);
    assert!(
        matches!(
            t.root(),
            Err(RootError::Incompatible {
                field: "prefix",
                ..
            })
        ),
        "{:?}",
        t.root()
    );
    assert_eq!(t.spawn("b"), Err("incompatible_execution_root".into()));
    // Un begin con el otro prefijo tampoco crea una segunda raíz.
    assert_eq!(t.begin(), Err("graph_not_empty".into()));
    let schema = fixed(&mut t.g, "root.schema");
    assert_eq!(t.g.query(&[schema]).len(), 1);
}

#[test]
fn large_usize_config_is_exact_on_the_direct_path_and_refused_by_the_host() {
    // Vía directa: la raíz guarda usize::MAX sin saturar.
    let mut big = run1();
    big.max_agents = usize::MAX;
    let mut w = World::new(big.clone());
    w.begin().unwrap();
    assert!(w.root().is_ok());
    w.spawn("a").unwrap();
    // La misma config saturada a i64::MAX (lo que da un blueprint) no coincide.
    let saturated = AgentConfig::from_params(&big.to_params()).unwrap();
    assert_eq!(saturated.max_agents as u64, i64::MAX as u64);
    assert!(matches!(
        execution_root(&w.g, &w.v, &saturated),
        Err(RootError::Incompatible {
            field: "config",
            ..
        })
    ));
    // Host: con execution, una config que no vuelve exacta es un error...
    // Solo los campos usize pueden superar i64::MAX.
    for field in 0..2 {
        let mut c = run1();
        match field {
            0 => c.max_agents = usize::MAX,
            _ => c.memory_keep_last = usize::MAX,
        }
        assert!(
            matches!(
                AgentHost::builder(1).config(c).build(),
                Err(OnyxError::Format(_))
            ),
            "campo {field}"
        );
    }
    // ...y sin execution sigue el comportamiento heredado (satura y construye).
    let mut legacy = cfg("agent", None);
    legacy.max_agents = usize::MAX;
    assert!(AgentHost::builder(1).config(legacy).build().is_ok());
}

#[test]
fn host_blueprint_replaced_runtime_and_direct_spawner_share_one_config() {
    let c = cfg("team", Some(exec("run-h", "app-h")));
    let mut h = AgentHost::builder(1)
        .config(c.clone())
        .memory(Box::new(DefaultMemory::new(2)))
        .build()
        .unwrap();
    h.begin_execution();
    h.run(&mut Quiet, 1);
    h.spawn(&AgentSpec::new("a", "g"));
    h.run(&mut Quiet, 2);
    let r = h.execution_root().unwrap();
    assert_eq!(
        r.next_birth.get().get(),
        2,
        "el spawner del blueprint creó el agente"
    );
    // La raíz que escribió el host es la que valida la config original fuera del host.
    let v = h.vocab();
    assert_eq!(execution_root(h.graph(), &v, &c), Ok(r.clone()));
    // Y un spawner directo con esa config continúa la misma ejecución.
    let mut g = AgentGraph::new();
    g.import(&h.graph().export()).unwrap();
    let mut w = World::on(g, c.clone());
    let b = w.spawn("b").unwrap();
    assert_eq!(w.birth(b), 2);
    // Control: otra config del host no valida esa raíz.
    let mut other = c.clone();
    other.default_max_steps += 1;
    assert!(execution_root(h.graph(), &v, &other).is_err());
    // Un segundo begin del host no sustituye la raíz.
    h.begin_execution();
    h.run(&mut Quiet, 1);
    assert_eq!(h.execution_root().unwrap().concept, r.concept);
}

// ---------- nacimientos: asignación y no consumo ----------

#[test]
fn births_are_assigned_in_order_with_the_agent_in_the_same_run() {
    let mut w = World::new(run1());
    w.begin().unwrap();
    let a = w.spawn("a").unwrap();
    assert_eq!(w.next_birth(), 2);
    let b = w.spawn("b").unwrap();
    assert_eq!((w.birth(a), w.birth(b)), (1, 2));
    assert_eq!(w.next_birth(), 3);
    assert_eq!(
        w.v.root.identity(&w.g, a).unwrap(),
        Some(identity("run-1", 1))
    );
}

#[test]
fn rejected_spawns_do_not_consume_a_birth_and_keep_rejection_order() {
    let mut c = run1();
    c.max_agents = 1;
    let mut w = World::new(c.clone());
    // Sin raíz: el orden es cupo -> spec -> raíz.
    assert_eq!(
        w.spawn_raw(vec![(w.v.max_steps, Value::Int(0))]),
        Err("invalid_spec".into())
    );
    assert_eq!(w.spawn("a"), Err("no_execution_root".into()));
    let mut w = World::new(c);
    w.begin().unwrap();
    let before = fingerprint(&w.g);
    let bad_policy = (w.v.policy, text("nope"));
    assert_eq!(w.spawn_raw(vec![bad_policy]), Err("invalid_policy".into()));
    assert_eq!(
        w.spawn_raw(vec![(w.v.timeout, Value::Int(0))]),
        Err("invalid_spec".into())
    );
    assert_eq!(fingerprint(&w.g), before, "rechazo sin escritura");
    let a = w.spawn("a").unwrap();
    assert_eq!(w.birth(a), 1, "los rechazos no consumieron");
    // Cupo lleno antes que la raíz: no consume.
    let before = fingerprint(&w.g);
    assert_eq!(w.spawn("b"), Err("max_agents".into()));
    assert_eq!(fingerprint(&w.g), before);
    assert_eq!(w.next_birth(), 2);
}

#[test]
fn destroy_does_not_return_a_birth() {
    let mut w = World::new(run1());
    w.begin().unwrap();
    let a = w.spawn("a").unwrap();
    w.g.destroy(a);
    assert_eq!(w.v.root.locate(&w.g, &identity("run-1", 1)), Ok(None));
    assert!(w.root().is_ok());
    let b = w.spawn("b").unwrap();
    assert_eq!(w.birth(b), 2);
    assert_eq!(w.next_birth(), 3);
}

#[test]
fn counter_exhaustion_never_assigns_u64_max_and_leaves_no_partial_state() {
    let mut w = World::new(run1());
    let root = w.begin().unwrap();
    w.set_fixed(root, "root.next_birth", text(&(u64::MAX - 1).to_string()));
    let a = w.spawn("a").unwrap();
    assert_eq!(w.birth(a), u64::MAX - 1);
    assert_eq!(w.next_birth(), u64::MAX);
    let before = fingerprint(&w.g);
    assert_eq!(w.spawn("b"), Err("birth_exhausted".into()));
    assert_eq!(fingerprint(&w.g), before, "ni agente ni contador parcial");
    let next = w.g.create();
    assert_eq!(
        next.0,
        a.0 + 1,
        "el spawn agotado no creó un concepto oculto"
    );
}

// ---------- extras no fijan atributos fijos ----------

#[test]
fn spawn_extras_cannot_set_any_fixed_attribute_on_either_path() {
    for exec_mode in [true, false] {
        for name in ROOT_FIELDS.iter().chain(AGENT_FIELDS.iter()) {
            for helper in [true, false] {
                let c = if exec_mode {
                    run1()
                } else {
                    cfg("agent", None)
                };
                let mut w = World::new(c);
                if exec_mode {
                    w.begin().unwrap();
                }
                let attr = fixed(&mut w.g, name);
                let forged = text("99");
                let id = if helper {
                    let spec = AgentSpec::new("a", "g").with_extra(attr, forged.clone());
                    let ev = spawn_event(&w.v, &spec, None);
                    w.spawn_ev(ev).unwrap()
                } else {
                    w.spawn_raw(vec![(attr, forged.clone())]).unwrap()
                };
                let ctx = format!("{name} helper={helper} execution={exec_mode}");
                assert!(w.v.is_reserved(attr), "{ctx}");
                assert_ne!(w.g.get(id, attr), Some(forged), "{ctx}");
                if exec_mode {
                    assert_eq!(w.birth(id), 1, "{ctx}");
                    assert_eq!(w.next_birth(), 2, "{ctx}");
                } else {
                    assert_eq!(w.v.root.identity(&w.g, id), Ok(None), "{ctx}");
                }
            }
        }
    }
}

// ---------- modo heredado ----------

#[test]
fn legacy_spawner_without_root_writes_no_identity() {
    let mut w = World::new(cfg("agent", None));
    let a = w.spawn("a").unwrap();
    assert_eq!(w.v.root.identity(&w.g, a), Ok(None));
    assert!(
        w.g.props(a)
            .iter()
            .all(|(n, _)| !n.starts_with(ROOT_NAMESPACE))
    );
}

#[test]
fn legacy_spawner_with_a_root_rejects_without_writing() {
    let mut w = World::new(run1());
    w.begin().unwrap();
    w.spawn("a").unwrap();
    let mut w = w.with_config(cfg("agent", None));
    let before = fingerprint(&w.g);
    assert_eq!(w.spawn("b"), Err("execution_not_configured".into()));
    assert_eq!(fingerprint(&w.g), before);
}

// ---------- snapshot antes del nacimiento ----------

#[test]
fn restoring_before_a_birth_on_the_same_graph_repeats_the_birth_with_a_new_concept_id() {
    let mut w = World::new(run1());
    w.begin().unwrap();
    let s0 = w.g.export();
    let first = w.spawn("a").unwrap();
    let first_id = w.v.root.identity(&w.g, first).unwrap().unwrap();
    w.g.import(&s0).unwrap();
    assert_eq!(w.next_birth(), 1);
    let again = w.spawn("a").unwrap();
    assert_ne!(again, first, "el grafo no reutiliza ConceptId");
    let again_id = w.v.root.identity(&w.g, again).unwrap().unwrap();
    assert_eq!(again_id, first_id);
    assert_eq!(w.v.root.locate(&w.g, &first_id), Ok(Some(again)));
    // La clave de operación no depende del ConceptId.
    let k = |b: AgentBirth| OperationKey::new(ExecutionId::new("run-1").unwrap(), b, nz(1));
    assert_eq!(k(first_id.birth), k(again_id.birth));
}

#[test]
fn restoring_before_a_birth_on_a_new_graph_repeats_concept_id_and_birth() {
    let mut w = World::new(run1());
    w.begin().unwrap();
    let s0 = w.g.export();
    let first = w.spawn("a").unwrap();
    let mut g = AgentGraph::new();
    g.import(&s0).unwrap();
    let mut n = World::on(g, run1());
    let again = n.spawn("a").unwrap();
    assert_eq!(again, first);
    assert_eq!(n.birth(again), w.birth(first));
}

/// Generador determinista propio: secuencias de spawn válido, inválido,
/// destroy y restauración de capturas anteriores en el mismo grafo, contra
/// un modelo del contador.
#[test]
fn generated_histories_match_a_model_of_the_birth_counter() {
    for seed in 1..=40u64 {
        let mut x = seed;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut c = run1();
        c.max_agents = 4;
        let mut w = World::new(c);
        w.begin().unwrap();
        let mut model_next = 1u64;
        let mut model_live: BTreeMap<u64, ()> = BTreeMap::new();
        let mut snaps: Vec<(Vec<u8>, u64, BTreeMap<u64, ()>)> = Vec::new();
        for step in 0..60 {
            let ctx = format!("seed {seed} paso {step}");
            match next() % 5 {
                0 | 1 => {
                    let r = w.spawn("a");
                    if model_live.len() < 4 {
                        let id = r.expect(&ctx);
                        assert_eq!(w.birth(id), model_next, "{ctx}");
                        model_live.insert(model_next, ());
                        model_next += 1;
                    } else {
                        assert_eq!(r, Err("max_agents".into()), "{ctx}");
                    }
                }
                2 => {
                    let r = w.spawn_raw(vec![(w.v.max_steps, Value::Int(-1))]);
                    // El cupo se comprueba antes que la spec.
                    let want = if model_live.len() < 4 {
                        "invalid_spec"
                    } else {
                        "max_agents"
                    };
                    assert_eq!(r, Err(want.into()), "{ctx}");
                }
                3 => {
                    let agents = w.g.query(&[w.v.state]);
                    if !agents.is_empty() {
                        let id = agents[(next() as usize) % agents.len()];
                        model_live.remove(&w.birth(id));
                        w.g.destroy(id);
                    }
                }
                _ => {
                    if !snaps.is_empty() && next() % 2 == 0 {
                        let (bytes, n, live) = snaps[(next() as usize) % snaps.len()].clone();
                        w.g.import(&bytes).unwrap();
                        model_next = n;
                        model_live = live;
                    } else {
                        snaps.push((w.g.export(), model_next, model_live.clone()));
                    }
                }
            }
            assert_eq!(w.next_birth(), model_next, "{ctx}");
            let mut births: Vec<u64> =
                w.g.query(&[w.v.state])
                    .into_iter()
                    .map(|id| w.birth(id))
                    .collect();
            births.sort();
            assert_eq!(
                births,
                model_live.keys().copied().collect::<Vec<_>>(),
                "{ctx}"
            );
        }
    }
}

// ---------- validación de identidades ----------

fn world_with(births: &[&str]) -> (World, Vec<ConceptId>) {
    let mut w = World::new(run1());
    w.begin().unwrap();
    let ids = births.iter().map(|n| w.spawn(n).unwrap()).collect();
    (w, ids)
}

fn assert_read_only_err(w: &mut World, expected: RootError, reason: &str) {
    let before = fingerprint(&w.g);
    assert_eq!(w.root(), Err(expected));
    assert_eq!(fingerprint(&w.g), before, "execution_root no muta");
    assert_eq!(w.spawn("z"), Err(reason.into()));
    assert_eq!(fingerprint(&w.g), before, "el spawn rechazado no muta");
}

#[test]
fn an_agent_without_birth_is_rejected() {
    let (mut w, _) = world_with(&["a"]);
    let v = w.v;
    let bare = w.g.spawn_agent(&v, "bare", "g");
    assert_read_only_err(&mut w, RootError::MissingBirth(bare), "missing_birth");
}

#[test]
fn duplicate_live_births_are_rejected() {
    let (mut w, ids) = world_with(&["a", "b"]);
    w.set_fixed(ids[1], "agent.birth", text("1"));
    assert_read_only_err(
        &mut w,
        RootError::DuplicateBirth(AgentBirth::FIRST),
        "duplicate_birth",
    );
}

#[test]
fn an_agent_birth_at_or_above_next_birth_is_ahead() {
    for b in ["2", "3", &u64::MAX.to_string()] {
        let (mut w, ids) = world_with(&["a"]);
        w.set_fixed(ids[0], "agent.birth", text(b));
        let birth = AgentBirth::new(nz(b.parse().unwrap()));
        assert_read_only_err(
            &mut w,
            RootError::BirthAhead {
                concept: ids[0],
                birth,
            },
            "birth_ahead",
        );
    }
}

#[test]
fn partial_malformed_or_stateless_identities_are_corrupt() {
    let cases: Vec<(&str, Value)> = vec![
        ("agent.birth", text("01")),
        ("agent.birth", text("0")),
        ("agent.birth", text("+1")),
        ("agent.birth", Value::Int(1)),
        ("agent.execution", text("")),
        ("agent.execution", Value::Int(1)),
    ];
    for (field, value) in cases {
        let (mut w, ids) = world_with(&["a"]);
        w.set_fixed(ids[0], field, value.clone());
        let got = w.root();
        assert!(
            matches!(&got, Err(RootError::Corrupt { concept, .. }) if *concept == ids[0]),
            "{field}={value:?}: {got:?}"
        );
        assert_eq!(w.spawn("z"), Err("corrupt_execution_root".into()));
    }
    // Identidad parcial: solo uno de los dos campos.
    for only in AGENT_FIELDS {
        let (mut w, _) = world_with(&[]);
        let v = w.v;
        let id = w.g.spawn_agent(&v, "half", "g");
        let value = if only == "agent.birth" { "1" } else { "run-1" };
        w.set_fixed(id, only, text(value));
        assert!(
            matches!(w.root(), Err(RootError::Corrupt { concept, .. }) if concept == id),
            "{only}: {:?}",
            w.root()
        );
    }
    // Identidad en un concepto que no es agente.
    let (mut w, _) = world_with(&["a"]);
    let stray = w.g.create();
    w.set_fixed(stray, "agent.execution", text("run-1"));
    w.set_fixed(stray, "agent.birth", text("1"));
    assert_eq!(
        w.root(),
        Err(RootError::Corrupt {
            concept: stray,
            field: "agent.state"
        })
    );
}

#[test]
fn an_agent_of_another_execution_is_foreign() {
    let (mut w, ids) = world_with(&["a"]);
    w.set_fixed(ids[0], "agent.execution", text("run-2"));
    assert_read_only_err(
        &mut w,
        RootError::ForeignExecution(ids[0]),
        "foreign_execution",
    );
}

fn op_key(execution: &str, birth: u64, request: u64) -> OperationKey {
    OperationKey::new(
        ExecutionId::new(execution).unwrap(),
        AgentBirth::new(nz(birth)),
        nz(request),
    )
}

#[test]
fn historical_operations_of_destroyed_agents_are_valid_below_next_birth() {
    let (mut w, ids) = world_with(&["a", "b"]);
    let log = w.v.operations;
    for r in 1..=3 {
        log.record(
            &mut w.g,
            op_key("run-1", 1, r),
            ToolDescriptor::new("t", "x", "r").unwrap(),
        )
        .unwrap();
    }
    w.g.destroy(ids[0]);
    assert!(w.root().is_ok(), "operaciones históricas del nacimiento 1");
    assert_eq!(log.entries(&w.g).unwrap().len(), 3);
    let c = w.spawn("c").unwrap();
    assert_eq!(w.birth(c), 3);
}

#[test]
fn operations_ahead_or_foreign_are_rejected_even_without_their_agent() {
    for (execution, birth, expected) in [
        ("run-1", 3u64, "birth_ahead"),
        ("run-1", u64::MAX, "birth_ahead"),
        ("run-2", 1, "foreign_execution"),
    ] {
        let (mut w, ids) = world_with(&["a", "b"]);
        w.g.destroy(ids[0]);
        let op =
            w.v.operations
                .record(
                    &mut w.g,
                    op_key(execution, birth, 1),
                    ToolDescriptor::new("t", "x", "r").unwrap(),
                )
                .unwrap();
        let err = if expected == "birth_ahead" {
            RootError::BirthAhead {
                concept: op.id,
                birth: AgentBirth::new(nz(birth)),
            }
        } else {
            RootError::ForeignExecution(op.id)
        };
        assert_read_only_err(&mut w, err, expected);
    }
}

#[test]
fn a_corrupt_operation_log_blocks_the_root() {
    let (mut w, _) = world_with(&["a"]);
    let op =
        w.v.operations
            .record(
                &mut w.g,
                op_key("run-1", 1, 1),
                ToolDescriptor::new("t", "x", "r").unwrap(),
            )
            .unwrap();
    let schema = w.g.attr("agent.operation.schema");
    w.g.set(op.id, schema, text("tool.v1"));
    assert_read_only_err(
        &mut w,
        RootError::Operation(OperationError::Corrupt {
            concept: op.id,
            field: "schema",
        }),
        "invalid_operation_log",
    );
}

#[test]
fn locate_finds_only_the_live_concept_with_the_full_identity() {
    let (w, ids) = world_with(&["a", "b"]);
    assert_eq!(
        w.v.root.locate(&w.g, &identity("run-1", 2)),
        Ok(Some(ids[1]))
    );
    assert_eq!(w.v.root.locate(&w.g, &identity("run-2", 2)), Ok(None));
    assert_eq!(w.v.root.locate(&w.g, &identity("run-1", 3)), Ok(None));
}

// ---------- locate sin respuestas ambiguas ----------

#[test]
fn locate_rejects_a_duplicate_identity_whichever_concept_comes_first() {
    // Duplicado después del primer coincidente (id mayor) y antes (id menor).
    for (dup_index, birth) in [(1usize, 1u64), (0, 2)] {
        let (mut w, ids) = world_with(&["a", "b"]);
        w.set_fixed(ids[dup_index], "agent.birth", text(&birth.to_string()));
        let before = fingerprint(&w.g);
        assert_eq!(
            w.v.root.locate(&w.g, &identity("run-1", birth)),
            Err(RootError::DuplicateBirth(AgentBirth::new(nz(birth)))),
            "duplicado en ids[{dup_index}]"
        );
        assert_eq!(fingerprint(&w.g), before, "locate no muta");
    }
    // También si la copia está en un concepto que no es agente.
    let (mut w, _) = world_with(&["a"]);
    let stray = w.g.create();
    w.set_fixed(stray, "agent.execution", text("run-1"));
    w.set_fixed(stray, "agent.birth", text("1"));
    assert_eq!(
        w.v.root.locate(&w.g, &identity("run-1", 1)),
        Err(RootError::DuplicateBirth(AgentBirth::FIRST))
    );
}

#[test]
fn locate_rejects_a_malformed_identity_on_another_concept_before_or_after_the_match() {
    let malformed: [(&str, Option<Value>, Option<Value>); 5] = [
        ("birth no canónico", Some(text("run-1")), Some(text("01"))),
        ("birth no texto", Some(text("run-1")), Some(Value::Int(9))),
        ("execution vacía", Some(text("")), Some(text("9"))),
        ("solo execution", Some(text("run-1")), None),
        ("solo birth", None, Some(text("9"))),
    ];
    for (label, execution, birth) in malformed {
        for after in [false, true] {
            // `before`: el concepto malformado tiene id menor que el buscado.
            let mut w = World::new(run1());
            w.begin().unwrap();
            // Concepto ajeno sin atributos hasta después del spawn real.
            let bad_early = if after { None } else { Some(w.g.create()) };
            let target = w.spawn("a").unwrap();
            let bad = bad_early.unwrap_or_else(|| w.g.create());
            if let Some(e) = &execution {
                w.set_fixed(bad, "agent.execution", e.clone());
            }
            if let Some(b) = &birth {
                w.set_fixed(bad, "agent.birth", b.clone());
            }
            let before = fingerprint(&w.g);
            let got = w.v.root.locate(&w.g, &identity("run-1", 1));
            assert!(
                matches!(&got, Err(RootError::Corrupt { concept, .. }) if *concept == bad),
                "{label} after={after}: {got:?} (buscado {target:?})"
            );
            assert_eq!(fingerprint(&w.g), before, "{label}");
        }
    }
}

#[test]
fn the_same_birth_in_different_executions_is_not_ambiguous_for_locate() {
    let (mut w, ids) = world_with(&["a"]);
    let v = w.v;
    let other = w.g.spawn_agent(&v, "other", "g");
    w.set_fixed(other, "agent.execution", text("run-2"));
    w.set_fixed(other, "agent.birth", text("1"));
    assert_eq!(
        w.v.root.locate(&w.g, &identity("run-1", 1)),
        Ok(Some(ids[0]))
    );
    assert_eq!(
        w.v.root.locate(&w.g, &identity("run-2", 1)),
        Ok(Some(other))
    );
    // locate no valida la raíz: la ejecución ajena la rechaza execution_root.
    assert_eq!(w.root(), Err(RootError::ForeignExecution(other)));
}

#[test]
fn locate_ok_does_not_imply_a_valid_root_or_config() {
    // Precondición documentada: locate no comprueba raíz, configuración ni
    // prefijo; execution_root sigue siendo necesario.
    let (w, ids) = world_with(&["a"]);
    let mut other = run1();
    other.default_max_steps += 1;
    assert!(execution_root(&w.g, &w.v, &other).is_err());
    assert_eq!(
        w.v.root.locate(&w.g, &identity("run-1", 1)),
        Ok(Some(ids[0]))
    );
    // Sin raíz en absoluto.
    let mut g = AgentGraph::new();
    let v = Vocab::resolve(&mut g);
    let a = g.spawn_agent(&v, "a", "g");
    let ex = fixed(&mut g, "agent.execution");
    let bi = fixed(&mut g, "agent.birth");
    g.set(a, ex, text("run-1"));
    g.set(a, bi, text("1"));
    assert_eq!(execution_root(&g, &v, &run1()), Err(RootError::Missing));
    assert_eq!(v.root.locate(&g, &identity("run-1", 1)), Ok(Some(a)));
}
