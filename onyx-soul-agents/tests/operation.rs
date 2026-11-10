//! Registro de operaciones de herramienta por la API pública del soul.
//!
//! Solo cubre el registro en el grafo: identidad, petición exacta, resultado
//! conocido, conflicto y persistencia. Aún no hay runtime ni driver que lo
//! use, así que nada de esto prueba la recuperación de efectos externos.
use std::num::NonZeroU64;

use onyx_soul_agents::{
    AgentGraph, Attr, AttrRegistry, ConceptId, ExecutionId, Graph, GraphView, OperationError,
    OperationKey, OperationLog, OperationState, ToolDescriptor, ToolOperation, ToolOutcome, Value,
    Vocab,
};

type Fingerprint = Vec<(u64, Vec<(String, Value)>, Vec<(String, ConceptId)>)>;

/// Todo lo observable del grafo: conceptos vivos, propiedades y relaciones.
fn fingerprint(g: &dyn GraphView) -> Fingerprint {
    g.query(&[])
        .into_iter()
        .map(|id| (id.0, g.props(id), g.links(id)))
        .collect()
}

fn exec(s: &str) -> ExecutionId {
    ExecutionId::new(s).unwrap()
}
fn key(execution: &str, agent: u64, request: u64) -> OperationKey {
    OperationKey::new(
        exec(execution),
        ConceptId(agent),
        NonZeroU64::new(request).unwrap(),
    )
}
fn desc(tool: &str, args: &str, route: &str) -> ToolDescriptor {
    ToolDescriptor::new(tool, args, route).unwrap()
}
fn ok(s: &str) -> ToolOutcome {
    ToolOutcome::Success(s.into())
}

fn setup() -> (AgentGraph, OperationLog) {
    let mut g = AgentGraph::new();
    let log = OperationLog::resolve(&mut g);
    (g, log)
}

/// Ids de los conceptos de registro: los que llevan atributos `<prefix>.operation.*`.
fn record_attrs(g: &dyn GraphView, id: ConceptId) -> Vec<(String, Value)> {
    g.props(id)
        .into_iter()
        .filter(|(n, _)| n.starts_with("agent.operation."))
        .collect()
}

fn state_of(g: &AgentGraph, log: &OperationLog, k: &OperationKey) -> OperationState {
    log.find(g, k).unwrap().expect("record").state
}

// ---------- validación de construcción ----------

#[test]
fn empty_identity_tool_or_route_are_rejected_but_empty_args_are_a_value() {
    assert_eq!(ExecutionId::new(""), Err(OperationError::EmptyExecutionId));
    assert_eq!(
        ToolDescriptor::new("", "a", "r"),
        Err(OperationError::EmptyTool)
    );
    assert_eq!(
        ToolDescriptor::new("t", "a", ""),
        Err(OperationError::EmptyRoute)
    );
    let d = ToolDescriptor::new("t", "", "r").unwrap();
    assert_eq!((d.tool(), d.args(), d.route()), ("t", "", "r"));
    // Sin recorte: un espacio es un nombre/ruta válido y distinto.
    assert_eq!(ExecutionId::new(" ").unwrap().as_str(), " ");
}

// ---------- identidad ----------

#[test]
fn identity_keeps_full_u64_and_every_component_distinguishes_records() {
    let (mut g, log) = setup();
    let d = desc("t", "a", "r");
    let max = key("e", u64::MAX, u64::MAX);
    // Vecinos que solo coinciden tras truncar a i64/u32 o tras ignorar un componente.
    let keys = [
        max.clone(),
        key("e", u64::MAX >> 1, u64::MAX),          // i64::MAX
        key("e", (u64::MAX >> 32) << 32, u64::MAX), // mismos 32 bits bajos que 0
        key("e", u64::MAX, u64::MAX >> 1),
        key("e", u64::MAX, (1 << 32) + 1), // mismos 32 bits bajos que request 1
        key("e", u64::MAX, 1),
        key("e2", u64::MAX, u64::MAX),
        key("E", u64::MAX, u64::MAX),
    ];
    for k in &keys {
        let op = log.record(&mut g, k.clone(), d.clone()).unwrap();
        assert_eq!(&op.key, k);
        assert_eq!(op.state, OperationState::Pending);
    }
    let entries = log.entries(&g).unwrap();
    assert_eq!(entries.len(), keys.len());
    let read: Vec<_> = entries.iter().map(|o| o.key.clone()).collect();
    for k in &keys {
        assert!(read.contains(k), "{k:?} perdido");
    }
    let found = log.find(&g, &max).unwrap().unwrap();
    assert_eq!(found.key.agent(), ConceptId(u64::MAX));
    assert_eq!(found.key.request().get(), u64::MAX);
    assert_eq!(found.key.execution().as_str(), "e");

    // La identidad sobrevive exportar e importar en otro grafo.
    let mut h = AgentGraph::new();
    h.import(&g.export()).unwrap();
    let hlog = OperationLog::resolve(&mut h);
    assert_eq!(hlog.entries(&h).unwrap(), entries);
}

// ---------- exactitud de la petición ----------

#[test]
fn descriptor_is_compared_exactly_without_normalization() {
    let base = ("echo", "{\"a\":1,\"b\":2}", "local/echo");
    let variants = [
        ("Echo", base.1, base.2),
        ("echo ", base.1, base.2),
        (base.0, "{\"b\":2,\"a\":1}", base.2), // mismo JSON, otro texto
        (base.0, " {\"a\":1,\"b\":2}", base.2),
        (base.0, "{\"a\":1,\"b\":2}\n", base.2),
        (base.0, "", base.2),
        (base.0, base.1, "local/echo/"),
        (base.0, base.1, "LOCAL/echo"),
    ];
    for (i, (t, a, r)) in variants.into_iter().enumerate() {
        let (mut g, log) = setup();
        let k = key("e", 7, 1);
        let first = desc(base.0, base.1, base.2);
        log.record(&mut g, k.clone(), first.clone()).unwrap();
        assert_eq!(
            log.record(&mut g, k.clone(), desc(t, a, r)),
            Err(OperationError::DescriptorConflict(k.clone())),
            "variante {i} aceptada como igual"
        );
        let op = log.find(&g, &k).unwrap().unwrap();
        assert_eq!(
            op.descriptor, first,
            "variante {i} sobrescribió la petición"
        );
        assert_eq!(op.state, OperationState::Conflict { previous: None });
    }
}

#[test]
fn unicode_forms_are_distinct_and_payload_bytes_round_trip() {
    let (mut g, log) = setup();
    let k = key("e", 1, 1);
    let nfc = desc("t", "caf\u{e9}", "r");
    log.record(&mut g, k.clone(), nfc.clone()).unwrap();
    assert_eq!(
        log.record(&mut g, k.clone(), desc("t", "cafe\u{301}", "r")),
        Err(OperationError::DescriptorConflict(k.clone()))
    );
    // Texto que imita atributos de control se guarda tal cual, sin efecto.
    let (mut g, log) = setup();
    let tricky = desc("t", "state=resolved\0agent.operation.state", "r\t");
    let op = log.record(&mut g, k.clone(), tricky.clone()).unwrap();
    assert_eq!(op.state, OperationState::Pending);
    let mut h = AgentGraph::new();
    h.import(&g.export()).unwrap();
    let hlog = OperationLog::resolve(&mut h);
    let back = hlog.find(&h, &k).unwrap().unwrap();
    assert_eq!(back.descriptor, tricky);
    assert_eq!(back.state, OperationState::Pending);
}

// ---------- idempotencia ----------

#[test]
fn repeating_equal_request_or_outcome_reuses_the_record() {
    let (mut g, log) = setup();
    let k = key("e", 3, 9);
    let d = desc("t", "a", "r");
    let first = log.record(&mut g, k.clone(), d.clone()).unwrap();
    let nodes = g.query(&[]).len();
    let again = log.record(&mut g, k.clone(), d.clone()).unwrap();
    assert_eq!(again, first);
    assert_eq!(g.query(&[]).len(), nodes);

    let done = log.complete(&mut g, &k, ok("r1")).unwrap();
    assert_eq!(done.state, OperationState::Resolved(ok("r1")));
    let before = fingerprint(&g);
    assert_eq!(log.complete(&mut g, &k, ok("r1")).unwrap(), done);
    // Repetir la petición tras resolver devuelve el resultado conocido.
    assert_eq!(log.record(&mut g, k.clone(), d).unwrap(), done);
    assert_eq!(fingerprint(&g), before, "repetir no debe mutar");
    assert_eq!(log.entries(&g).unwrap(), vec![done]);
}

// ---------- divergencias y conflicto pegajoso ----------

#[test]
fn different_outcome_blocks_and_keeps_the_first_outcome() {
    for second in [
        ok("r1 "),
        ok("R1"),
        ToolOutcome::Failure("r1".into()),
        ok(""),
    ] {
        let (mut g, log) = setup();
        let k = key("e", 1, 1);
        let d = desc("t", "a", "r");
        log.record(&mut g, k.clone(), d.clone()).unwrap();
        log.complete(&mut g, &k, ok("r1")).unwrap();
        assert_eq!(
            log.complete(&mut g, &k, second.clone()),
            Err(OperationError::OutcomeConflict(k.clone())),
            "{second:?}"
        );
        let op = log.find(&g, &k).unwrap().unwrap();
        assert_eq!(
            op.state,
            OperationState::Conflict {
                previous: Some(ok("r1"))
            }
        );
        assert_eq!(op.descriptor, d);
    }
}

#[test]
fn failure_is_a_known_outcome_and_differs_from_success() {
    let (mut g, log) = setup();
    let k = key("e", 1, 1);
    log.record(&mut g, k.clone(), desc("t", "a", "r")).unwrap();
    let fail = ToolOutcome::Failure("boom".into());
    let op = log.complete(&mut g, &k, fail.clone()).unwrap();
    assert_eq!(op.state, OperationState::Resolved(fail.clone()));
    assert_eq!(
        log.complete(&mut g, &k, ok("boom")),
        Err(OperationError::OutcomeConflict(k.clone()))
    );
    assert_eq!(
        state_of(&g, &log, &k),
        OperationState::Conflict {
            previous: Some(fail)
        }
    );
}

#[test]
fn descriptor_conflict_after_resolution_keeps_request_and_outcome() {
    let (mut g, log) = setup();
    let k = key("e", 1, 1);
    let d = desc("t", "a", "r");
    log.record(&mut g, k.clone(), d.clone()).unwrap();
    log.complete(&mut g, &k, ok("r1")).unwrap();
    assert_eq!(
        log.record(&mut g, k.clone(), desc("t", "b", "r")),
        Err(OperationError::DescriptorConflict(k.clone()))
    );
    let op = log.find(&g, &k).unwrap().unwrap();
    assert_eq!(op.descriptor, d);
    assert_eq!(
        op.state,
        OperationState::Conflict {
            previous: Some(ok("r1"))
        }
    );
}

#[test]
fn conflict_is_sticky_for_every_operation_and_across_snapshots() {
    let (mut g, log) = setup();
    let k = key("e", 1, 1);
    let d = desc("t", "a", "r");
    log.record(&mut g, k.clone(), d.clone()).unwrap();
    log.mark_uncertain(&mut g, &k).unwrap();
    assert_eq!(
        log.record(&mut g, k.clone(), desc("t", "a2", "r")),
        Err(OperationError::DescriptorConflict(k.clone()))
    );
    let conflict = OperationState::Conflict { previous: None };
    assert_eq!(state_of(&g, &log, &k), conflict);

    let mut h = AgentGraph::new();
    h.import(&g.export()).unwrap();
    let hlog = OperationLog::resolve(&mut h);
    for (gr, lg) in [(&mut g, &log), (&mut h, &hlog)] {
        let before = fingerprint(gr);
        // Ni la petición original ni un resultado la desbloquean.
        assert_eq!(
            lg.record(gr, k.clone(), d.clone()),
            Err(OperationError::Blocked(k.clone()))
        );
        assert_eq!(
            lg.complete(gr, &k, ok("late")),
            Err(OperationError::Blocked(k.clone()))
        );
        assert_eq!(
            lg.mark_uncertain(gr, &k),
            Err(OperationError::Blocked(k.clone()))
        );
        assert_eq!(fingerprint(gr), before, "Blocked no debe mutar");
        assert_eq!(state_of(gr, lg, &k), conflict);
    }
}

#[test]
fn second_different_descriptor_on_conflict_keeps_the_first_descriptor() {
    let (mut g, log) = setup();
    let k = key("e", 1, 1);
    let d = desc("t", "a", "r");
    log.record(&mut g, k.clone(), d.clone()).unwrap();
    log.complete(&mut g, &k, ok("r1")).unwrap();
    let _ = log.record(&mut g, k.clone(), desc("t", "b", "r"));
    let before = fingerprint(&g);
    // Contrato actual: otra divergencia devuelve DescriptorConflict (no Blocked)
    // y reescribe el mismo estado de conflicto, sin cambiar nada observable.
    assert_eq!(
        log.record(&mut g, k.clone(), desc("t", "c", "r")),
        Err(OperationError::DescriptorConflict(k.clone()))
    );
    assert_eq!(fingerprint(&g), before);
    let op = log.find(&g, &k).unwrap().unwrap();
    assert_eq!(op.descriptor, d);
    assert_eq!(
        op.state,
        OperationState::Conflict {
            previous: Some(ok("r1"))
        }
    );
}

// ---------- incertidumbre ----------

#[test]
fn uncertainty_belongs_to_one_operation_and_never_hides_a_known_result() {
    let (mut g, log) = setup();
    let d = desc("t", "a", "r");
    let a = key("e", 5, 1);
    let b = key("e", 5, 2); // mismo agente
    let c = key("e2", 5, 1); // misma pareja agente/petición, otra ejecución
    for k in [&a, &b, &c] {
        log.record(&mut g, k.clone(), d.clone()).unwrap();
    }
    assert_eq!(
        log.mark_uncertain(&mut g, &a).unwrap().state,
        OperationState::Uncertain
    );
    assert_eq!(state_of(&g, &log, &b), OperationState::Pending);
    assert_eq!(state_of(&g, &log, &c), OperationState::Pending);
    // Repetir es idempotente.
    let before = fingerprint(&g);
    log.mark_uncertain(&mut g, &a).unwrap();
    assert_eq!(fingerprint(&g), before);

    // Una observación tardía resuelve la incierta.
    let late = log.complete(&mut g, &a, ok("late")).unwrap();
    assert_eq!(late.state, OperationState::Resolved(ok("late")));
    // Marcar incierta una resuelta no pierde el resultado.
    let before = fingerprint(&g);
    assert_eq!(
        log.mark_uncertain(&mut g, &a).unwrap().state,
        OperationState::Resolved(ok("late"))
    );
    assert_eq!(fingerprint(&g), before);
    assert_eq!(state_of(&g, &log, &b), OperationState::Pending);
}

// ---------- vida del agente y memoria ----------

#[test]
fn destroying_the_agent_and_its_memory_keeps_the_record_usable() {
    let mut g = AgentGraph::new();
    let v = Vocab::resolve(&mut g);
    let log = OperationLog::resolve(&mut g);
    let agent = g.spawn_agent(&v, "a", "g");
    let mut mems = Vec::new();
    for i in 0..3 {
        let m = g.create();
        g.set(m, v.mem_seq, Value::Int(i));
        g.set(m, v.mem_text, Value::Text(format!("call {i}")));
        g.relate(agent, v.rel_memory, m);
        mems.push(m);
    }
    let k = OperationKey::new(exec("e"), agent, NonZeroU64::new(1).unwrap());
    let d = desc("t", "a", "r");
    let rec = log.record(&mut g, k.clone(), d.clone()).unwrap();
    let k2 = OperationKey::new(exec("e"), agent, NonZeroU64::new(2).unwrap());
    log.record(&mut g, k2.clone(), d.clone()).unwrap();
    log.complete(&mut g, &k2, ok("done")).unwrap();
    let before = log.entries(&g).unwrap();

    // Poda de memoria como la del runtime (destroy de entradas) y luego el agente.
    for m in &mems[..2] {
        g.destroy(*m);
    }
    assert_eq!(log.entries(&g).unwrap(), before);
    g.destroy(agent);
    assert!(!g.exists(agent));
    assert_eq!(log.entries(&g).unwrap(), before);
    // Ningún atributo del registro apunta al agente: nada que destroy pueda borrar.
    for op in &before {
        assert!(g.links(op.id).is_empty());
        assert!(
            record_attrs(&g, op.id)
                .iter()
                .all(|(_, v)| !matches!(v, Value::Concept(_)))
        );
    }
    // Un resultado tardío llega aunque el agente ya no exista.
    let done = log.complete(&mut g, &k, ok("late")).unwrap();
    assert_eq!(done.id, rec.id);
    assert_eq!(done.key.agent(), agent);
    assert_eq!(
        log.find(&g, &k2).unwrap().unwrap().state,
        OperationState::Resolved(ok("done"))
    );
}

// ---------- guardar, restaurar y continuar ----------

type Step = fn(&mut AgentGraph, &OperationLog) -> String;

fn steps() -> Vec<Step> {
    vec![
        |g, l| {
            format!(
                "{:?}",
                l.record(g, key("x", u64::MAX, 1), desc("t", "a", "r"))
            )
        },
        |g, l| format!("{:?}", l.record(g, key("x", 2, 1), desc("t", "b", "r"))),
        |g, l| format!("{:?}", l.mark_uncertain(g, &key("x", u64::MAX, 1))),
        |g, l| format!("{:?}", l.complete(g, &key("x", 2, 1), ok("b1"))),
        // --- frontera de guardado ---
        |g, l| format!("{:?}", l.complete(g, &key("x", u64::MAX, 1), ok("late"))),
        |g, l| format!("{:?}", l.record(g, key("x", 2, 1), desc("t", "b", "r"))),
        |g, l| format!("{:?}", l.complete(g, &key("x", 2, 1), ok("b2"))),
        |g, l| format!("{:?}", l.complete(g, &key("x", 2, 1), ok("b1"))),
        |g, l| format!("{:?}", l.record(g, key("x", 3, 1), desc("t", "c", "r"))),
        |g, l| format!("{:?}", l.mark_uncertain(g, &key("x", 9, 9))),
    ]
}

fn observe(g: &AgentGraph, l: &OperationLog) -> (Vec<ToolOperation>, Fingerprint) {
    (l.entries(g).unwrap(), fingerprint(g))
}

#[test]
fn save_restore_continue_equals_uninterrupted_run() {
    let cut = 4;
    let (mut g, log) = setup();
    let straight: Vec<_> = steps().iter().map(|s| s(&mut g, &log)).collect();
    let straight_end = observe(&g, &log);

    for target_has_other_names in [false, true] {
        let (mut a, alog) = setup();
        let mut out: Vec<_> = steps()[..cut].iter().map(|s| s(&mut a, &alog)).collect();
        let bytes = a.export();
        drop(a);

        let mut b = AgentGraph::new();
        if target_has_other_names {
            // Otro orden de registro de nombres en el receptor: Attr distintos.
            let _: Attr = b.attr("app.unrelated");
            let _ = Vocab::resolve(&mut b);
        }
        b.import(&bytes).unwrap();
        let blog = OperationLog::resolve(&mut b);
        out.extend(steps()[cut..].iter().map(|s| s(&mut b, &blog)));
        assert_eq!(out, straight, "otros nombres: {target_has_other_names}");
        // Mismos ids, nombres de atributo y valores: los Attr del receptor no se filtran.
        assert_eq!(observe(&b, &blog), straight_end);
    }
    // El final contiene lo que el registro promete, no solo «igual a sí mismo».
    let end = &straight_end.0;
    let by = |k: OperationKey| end.iter().find(|o| o.key == k).unwrap().state.clone();
    assert_eq!(
        by(key("x", u64::MAX, 1)),
        OperationState::Resolved(ok("late"))
    );
    assert_eq!(
        by(key("x", 2, 1)),
        OperationState::Conflict {
            previous: Some(ok("b1"))
        }
    );
    assert_eq!(by(key("x", 3, 1)), OperationState::Pending);
    assert_eq!(end.len(), 3);
}

#[test]
fn importing_into_the_same_graph_keeps_the_resolved_log_valid() {
    let (mut g, log) = setup();
    let k = key("e", 1, 1);
    log.record(&mut g, k.clone(), desc("t", "a", "r")).unwrap();
    let bytes = g.export();
    log.complete(&mut g, &k, ok("after")).unwrap();
    g.import(&bytes).unwrap();
    assert_eq!(state_of(&g, &log, &k), OperationState::Pending);
    log.complete(&mut g, &k, ok("again")).unwrap();
    assert_eq!(
        state_of(&g, &log, &k),
        OperationState::Resolved(ok("again"))
    );
}

#[test]
fn logs_with_different_prefixes_do_not_see_each_other() {
    let mut g = AgentGraph::new();
    let a = OperationLog::resolve(&mut g);
    let b = OperationLog::resolve_with(&mut g, "app");
    let k = key("e", 1, 1);
    a.record(&mut g, k.clone(), desc("t", "a", "r")).unwrap();
    assert!(b.entries(&g).unwrap().is_empty());
    b.record(&mut g, k.clone(), desc("t", "other", "r"))
        .unwrap();
    assert_eq!(a.find(&g, &k).unwrap().unwrap().descriptor.args(), "a");
    assert_eq!(b.find(&g, &k).unwrap().unwrap().descriptor.args(), "other");
}

// ---------- datos corruptos y claves repetidas ----------

/// Graba una operación resuelta y devuelve el grafo, el log y su id.
fn one_resolved() -> (AgentGraph, OperationLog, OperationKey, ConceptId) {
    let (mut g, log) = setup();
    let k = key("e", 4, 2);
    let op = log.record(&mut g, k.clone(), desc("t", "a", "r")).unwrap();
    log.complete(&mut g, &k, ok("r1")).unwrap();
    (g, log, k, op.id)
}

fn assert_rejected(g: &mut AgentGraph, log: &OperationLog, k: &OperationKey, id: ConceptId) {
    let before = fingerprint(g);
    let corrupt =
        |e: &OperationError| matches!(e, OperationError::Corrupt { concept, .. } if *concept == id);
    assert!(log.entries(g).is_err_and(|e| corrupt(&e)));
    assert!(log.find(g, k).is_err_and(|e| corrupt(&e)));
    // Escrituras también fallan sin reparar ni crear nada.
    assert!(
        log.record(g, k.clone(), desc("t", "a", "r"))
            .is_err_and(|e| corrupt(&e))
    );
    assert!(
        log.record(g, key("e", 99, 1), desc("t", "a", "r"))
            .is_err_and(|e| corrupt(&e))
    );
    assert!(log.complete(g, k, ok("r1")).is_err_and(|e| corrupt(&e)));
    assert!(log.mark_uncertain(g, k).is_err_and(|e| corrupt(&e)));
    assert_eq!(fingerprint(g), before);
}

fn attr_named(g: &mut AgentGraph, field: &str) -> Attr {
    g.attr(&format!("agent.operation.{field}"))
}

#[test]
fn every_missing_or_non_text_field_is_rejected() {
    let (g0, _, _, id) = one_resolved();
    let fields = record_attrs(&g0, id);
    assert!(fields.len() >= 6, "registro con campos de control");
    for (name, _) in fields {
        for replacement in [None, Some(Value::Int(1))] {
            let (mut g, log, k, id) = one_resolved();
            let attr = g.attr(&name);
            match &replacement {
                Some(v) => g.set(id, attr, v.clone()),
                None => {
                    // No hay unset en el puerto: copia el registro sin ese campo.
                    let props = record_attrs(&g, id);
                    g.destroy(id);
                    let id2 = g.create();
                    for (n, v) in props.into_iter().filter(|(n, _)| *n != name) {
                        let a = g.attr(&n);
                        g.set(id2, a, v);
                    }
                    // Sin schema el concepto no es un registro: desaparece, no corrompe.
                    if name.ends_with(".schema") {
                        assert!(log.entries(&g).unwrap().is_empty());
                        continue;
                    }
                    assert_rejected(&mut g, &log, &k, id2);
                    continue;
                }
            }
            assert_rejected(&mut g, &log, &k, id);
        }
    }
}

#[test]
fn malformed_values_are_rejected_not_repaired() {
    let cases: &[(&str, &str)] = &[
        ("schema", "tool.v2"),
        ("execution", ""),
        ("agent", "18446744073709551616"), // u64::MAX + 1
        ("agent", "-1"),
        ("agent", "04"), // no canónico: otra clave tras normalizar
        ("agent", " 4"),
        ("agent", "+4"),
        ("request", "0"),
        ("request", "02"),
        ("request", "x"),
        ("tool", ""),
        ("route", ""),
        ("state", "done"),
        ("state", "pending"),     // pending con resultado conocido
        ("outcome_kind", "none"), // none con texto
        ("outcome_kind", "Success"),
    ];
    for (field, value) in cases {
        let (mut g, log, k, id) = one_resolved();
        let attr = attr_named(&mut g, field);
        g.set(id, attr, Value::Text((*value).into()));
        // La clave buscada puede no coincidir ya; usa la original igualmente.
        let before = fingerprint(&g);
        assert!(
            matches!(log.entries(&g), Err(OperationError::Corrupt { concept, .. }) if concept == id),
            "{field}={value:?} aceptado"
        );
        assert!(log.record(&mut g, k.clone(), desc("t", "a", "r")).is_err());
        assert_eq!(fingerprint(&g), before, "{field}={value:?} reparado");
    }
    // Pendiente con texto de resultado pero sin tipo: no se descarta en silencio.
    let (mut g, log) = setup();
    let k = key("e", 4, 3);
    let op = log.record(&mut g, k.clone(), desc("t", "a", "r")).unwrap();
    let text = attr_named(&mut g, "outcome_text");
    g.set(op.id, text, Value::Text("lost".into()));
    assert_rejected(&mut g, &log, &k, op.id);
    // Resolved sin resultado también es corrupto.
    let (mut g, log, k, id) = one_resolved();
    let kind = attr_named(&mut g, "outcome_kind");
    let text = attr_named(&mut g, "outcome_text");
    g.set(id, kind, Value::Text("none".into()));
    g.set(id, text, Value::Text(String::new()));
    assert_rejected(&mut g, &log, &k, id);
}

#[test]
fn duplicated_key_is_rejected_by_reads_and_writes() {
    let (mut g, log, k, id) = one_resolved();
    let copy = g.create();
    for (n, v) in record_attrs(&g, id) {
        let a = g.attr(&n);
        g.set(copy, a, v);
    }
    let before = fingerprint(&g);
    let dup = Some(OperationError::DuplicateKey(k.clone()));
    assert_eq!(log.entries(&g).err(), dup);
    assert_eq!(log.find(&g, &k).err(), dup);
    assert_eq!(log.find(&g, &key("e", 1, 1)).err(), dup);
    assert_eq!(
        log.record(&mut g, k.clone(), desc("t", "a", "r")).err(),
        dup
    );
    assert_eq!(log.complete(&mut g, &k, ok("r1")).err(), dup);
    assert_eq!(log.mark_uncertain(&mut g, &k).err(), dup);
    assert_eq!(fingerprint(&g), before);
}

// ---------- lecturas y errores no mutan ----------

#[test]
fn missing_key_errors_do_not_mutate_and_documented_conflicts_do() {
    let (mut g, log) = setup();
    let k = key("e", 1, 1);
    let absent = key("e", 1, 2);
    log.record(&mut g, k.clone(), desc("t", "a", "r")).unwrap();
    let before = fingerprint(&g);
    assert_eq!(log.find(&g, &absent), Ok(None));
    assert_eq!(
        log.complete(&mut g, &absent, ok("x")),
        Err(OperationError::Missing(absent.clone()))
    );
    assert_eq!(
        log.mark_uncertain(&mut g, &absent),
        Err(OperationError::Missing(absent.clone()))
    );
    let _ = log.entries(&g).unwrap();
    assert_eq!(fingerprint(&g), before);

    // Excepción documentada: la divergencia sí persiste el conflicto.
    let _ = log.record(&mut g, k.clone(), desc("t", "b", "r"));
    assert_ne!(fingerprint(&g), before);
    assert_eq!(g.query(&[]).len(), before.len(), "sin conceptos nuevos");
}
