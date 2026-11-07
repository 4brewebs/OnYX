//! Conformidad observable de Graph. Cada caso recibe una fábrica independiente.
use crate::{ConceptId, Graph, Value};

type State = Vec<(ConceptId, Vec<(String, Value)>, Vec<(String, ConceptId)>)>;

fn state(g: &impl Graph) -> State {
    g.query(&[])
        .into_iter()
        .map(|id| (id, g.props(id), g.links(id)))
        .collect()
}

/// El registro conserva nombres exactos y no confunde nombres distintos.
pub fn registry<G: Graph>(factory: impl Fn() -> G) {
    let mut g = factory();
    let names = ["", "value", "VALUE", "rel", "ñ", "a\0b"];
    let ids: Vec<_> = names.iter().map(|name| g.attr(name)).collect();
    for (i, name) in names.iter().enumerate() {
        assert_eq!(g.attr(name), ids[i]);
        for other in &ids[..i] {
            assert_ne!(*other, ids[i]);
        }
    }
    assert!(g.query(&[]).is_empty());
    let x = g.create();
    g.set(x, ids[0], Value::Bool(false));
    let before = state(&g);
    g.attr("unused-after-create");
    assert_eq!(state(&g), before);
}

/// IDs crecientes, conceptos vacíos, consulta AND y separación de relaciones.
pub fn creation_and_queries<G: Graph>(factory: impl Fn() -> G) {
    let mut g = factory();
    let a = g.attr("a");
    let b = g.attr("b");
    let x = g.create();
    let y = g.create();
    let empty = g.create();
    assert!(x.0 < y.0 && y.0 < empty.0);
    assert!(g.exists(empty));
    assert_eq!(g.get(empty, a), None);
    g.set(y, a, Value::Bool(false));
    g.set(x, a, Value::Int(0));
    g.set(y, b, Value::Text("".into()));
    g.relate(empty, a, x);
    assert_eq!(g.query(&[]), vec![x, y, empty]);
    assert_eq!(g.query(&[a]), vec![x, y]);
    assert_eq!(g.query(&[a, a]), vec![x, y]);
    assert_eq!(g.query(&[b, a]), vec![y]);
    assert!(g.props(empty).is_empty());
    g.destroy(empty);
    let fresh = g.create();
    assert!(fresh.0 > empty.0);
    assert_eq!(g.query(&[]), vec![x, y, fresh]);
}

/// `set` no crea propietarios ni conserva referencias a destinos muertos.
pub fn set_and_missing_references<G: Graph>(factory: impl Fn() -> G) {
    let mut g = factory();
    let a = g.attr("a");
    let owner = g.create();
    let target = g.create();
    let empty = g.create();
    let missing = g.create();
    g.destroy(missing);
    let never_issued = ConceptId(missing.0.checked_add(1).unwrap());
    g.set(empty, a, Value::Concept(missing));
    assert_eq!(g.get(empty, a), None);
    for value in [
        Value::Bool(false),
        Value::Int(0),
        Value::Float(-0.0),
        Value::Text("".into()),
        Value::Concept(target),
    ] {
        g.set(owner, a, value.clone());
        assert_eq!(g.get(owner, a), Some(value));
    }
    g.set(owner, a, Value::Text("owned".into()));
    let Some(Value::Text(mut owned)) = g.get(owner, a) else {
        panic!("Text")
    };
    owned.push_str(" changed");
    assert_eq!(g.get(owner, a), Some(Value::Text("owned".into())));
    g.set(owner, a, Value::Int(7));
    g.set(owner, a, Value::Concept(never_issued));
    assert_eq!(g.get(owner, a), Some(Value::Int(7)));
    g.set(never_issued, a, Value::Int(99));
    assert!(!g.exists(never_issued));
    g.set(owner, a, Value::Concept(missing));
    assert_eq!(g.get(owner, a), Some(Value::Int(7)));
    g.set(missing, a, Value::Int(99));
    assert!(!g.exists(missing));
    assert_eq!(g.get(missing, a), None);
    assert!(g.props(missing).is_empty() && g.links(missing).is_empty());
    assert!(g.related(missing, a).is_empty());
    g.set(owner, a, Value::Concept(target));
    assert_eq!(g.get(owner, a), Some(Value::Concept(target)));
    assert!(g.links(owner).is_empty());
    g.set(owner, a, Value::Concept(owner));
    assert_eq!(g.get(owner, a), Some(Value::Concept(owner)));
    g.destroy(target);
    g.set(owner, a, Value::Concept(target));
    assert_eq!(g.get(owner, a), Some(Value::Concept(owner)));
}

/// Propiedades ordenadas por nombre y relaciones dirigidas sin duplicados.
pub fn ordering_and_relations<G: Graph>(factory: impl Fn() -> G) {
    let mut g = factory();
    let z = g.attr("z");
    let a = g.attr("a");
    let x = g.create();
    let y = g.create();
    let t = g.create();
    let dead = g.create();
    g.destroy(dead);
    g.set(x, z, Value::Int(2));
    g.set(x, a, Value::Int(1));
    assert_eq!(
        g.props(x),
        vec![("a".into(), Value::Int(1)), ("z".into(), Value::Int(2))]
    );
    g.relate(x, z, t);
    g.relate(x, a, t);
    g.relate(x, a, y);
    g.relate(x, a, y);
    g.relate(x, a, x);
    g.relate(x, a, dead);
    g.relate(dead, a, x);
    assert_eq!(g.related(x, a), vec![x, y, t]);
    assert!(g.related(y, a).is_empty());
    assert_eq!(
        g.links(x),
        vec![
            ("a".into(), x),
            ("a".into(), y),
            ("a".into(), t),
            ("z".into(), t)
        ]
    );
    g.unrelate(x, a, y);
    g.unrelate(x, a, y);
    g.unrelate(dead, a, x);
    g.unrelate(x, a, dead);
    assert_eq!(g.related(x, a), vec![x, t]);
    assert_eq!(g.related(x, z), vec![t]);
    assert_eq!(g.get(x, a), Some(Value::Int(1)));
    g.set(x, a, Value::Int(9));
    assert_eq!(g.related(x, a), vec![x, t]);
}

/// Destroy limpia referencias entrantes/salientes y propiedades Concept, no dueños.
pub fn destruction<G: Graph>(factory: impl Fn() -> G) {
    let mut g = factory();
    let rel = g.attr("rel");
    let p = g.attr("p");
    let keep = g.attr("keep");
    let second_reference = g.attr("second-reference");
    let x = g.create();
    let victim = g.create();
    let y = g.create();
    for from in [x, victim, y] {
        for to in [x, victim, y] {
            g.relate(from, rel, to);
        }
    }
    g.set(x, p, Value::Concept(victim));
    g.set(y, p, Value::Concept(victim));
    g.set(y, second_reference, Value::Concept(victim));
    g.set(victim, p, Value::Concept(victim));
    g.set(x, keep, Value::Int(5));
    g.destroy(victim);
    g.destroy(victim);
    assert_eq!(g.query(&[]), vec![x, y]);
    assert!(!g.exists(victim));
    assert_eq!(g.get(x, p), None);
    assert_eq!(g.get(y, p), None);
    assert_eq!(g.get(y, second_reference), None);
    assert_eq!(g.get(x, keep), Some(Value::Int(5)));
    assert_eq!(g.related(x, rel), vec![x, y]);
    assert_eq!(g.related(y, rel), vec![x, y]);
    assert!(g.props(victim).is_empty() && g.links(victim).is_empty());
    assert!(g.create().0 > y.0);
}

/// Export/import conserva todos los valores, incluido el patrón exacto de Float.
pub fn values_roundtrip<G: Graph>(factory: impl Fn() -> G) {
    let mut source = factory();
    let x = source.create();
    let target = source.create();
    let mut values = vec![
        Value::Bool(false),
        Value::Bool(true),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::Int(9_007_199_254_740_993),
        Value::Int(0),
        Value::Text(String::new()),
        Value::Text("á🌍\0\n".into()),
        Value::Concept(target),
    ];
    for bits in [
        0,
        1u64 << 63,
        1,
        0x8000_0000_0000_0001,
        0x0010_0000_0000_0000,
        0x7fef_ffff_ffff_ffff,
        0xffef_ffff_ffff_ffff,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0001,
        0x7ff8_0000_0000_0002,
        0x7ff0_0000_0000_0001,
        0xfff8_0000_0000_0001,
    ] {
        values.push(Value::Float(f64::from_bits(bits)));
    }
    for (i, v) in values.iter().enumerate() {
        let a = source.attr(&format!("v{i:03}"));
        source.set(x, a, v.clone());
        assert_eq!(source.get(x, a), Some(v.clone()));
    }
    let before = state(&source);
    let bytes = source.export();
    assert_eq!(state(&source), before);
    let mut restored = factory();
    restored.import(&bytes).unwrap();
    assert_eq!(state(&restored), before);
}

/// Los números de Attr del emisor no se imponen al registro receptor.
pub fn import_remaps_attributes<G: Graph>(factory: impl Fn() -> G) {
    let mut source = factory();
    let a = source.attr("a");
    let z = source.attr("z");
    source.attr("unused");
    let x = source.create();
    let y = source.create();
    source.set(x, a, Value::Text("name".into()));
    source.set(y, z, Value::Concept(x));
    source.relate(x, z, y);
    let mut receiver = factory();
    let rz = receiver.attr("z");
    let only = receiver.attr("receiver-only");
    let ra = receiver.attr("a");
    for _ in 0..3 {
        receiver.create();
    }
    let extra = receiver.create();
    receiver.set(extra, only, Value::Int(5));
    receiver.import(&source.export()).unwrap();
    assert_eq!(receiver.attr("z"), rz);
    assert_eq!(receiver.attr("a"), ra);
    assert_eq!(receiver.attr("receiver-only"), only);
    assert_eq!(receiver.get(x, ra), Some(Value::Text("name".into())));
    assert_eq!(receiver.get(x, rz), None);
    assert_eq!(receiver.related(x, rz), vec![y]);
    assert_eq!(receiver.get(y, rz), Some(Value::Concept(x)));
    assert_eq!(state(&receiver), state(&source));
    let unused = receiver.attr("unused");
    assert_ne!(unused, only);
    assert_eq!(receiver.attr("unused"), unused);
}

/// Importar no rebobina el máximo ID del proceso ni olvida IDs destruidos del snapshot.
pub fn high_water<G: Graph>(factory: impl Fn() -> G) {
    let mut source = factory();
    let mut source_high = source.create();
    for _ in 0..8 {
        source_high = source.create();
    }
    for id in source.query(&[]) {
        source.destroy(id);
    }
    let bytes = source.export();
    let mut fresh = factory();
    fresh.import(&bytes).unwrap();
    assert!(fresh.query(&[]).is_empty());
    assert!(fresh.create().0 > source_high.0);
    let mut receiver = factory();
    let mut high = receiver.create();
    for _ in 0..16 {
        high = receiver.create();
    }
    receiver.destroy(high);
    receiver.import(&bytes).unwrap();
    assert!(receiver.query(&[]).is_empty());
    assert!(receiver.create().0 > high.0.max(source_high.0));
}

/// Guardar/restaurar/continuar equivale al mismo guion sin interrupción en configuración compatible.
pub fn continuation<G: Graph>(factory: impl Fn() -> G) {
    fn step(g: &mut impl Graph, owner: ConceptId) {
        let p = g.attr("counter");
        let rel = g.attr("next");
        let Some(Value::Int(n)) = g.get(owner, p) else {
            panic!("counter")
        };
        g.set(owner, p, Value::Int(n + 1));
        let new = g.create();
        g.relate(owner, rel, new);
        g.set(new, p, Value::Concept(owner));
    }
    let mut uninterrupted = factory();
    let owner = uninterrupted.create();
    let p = uninterrupted.attr("counter");
    uninterrupted.set(owner, p, Value::Int(0));
    step(&mut uninterrupted, owner);
    let bytes = uninterrupted.export();
    let mut restored = factory();
    restored.import(&bytes).unwrap();
    for _ in 0..3 {
        step(&mut uninterrupted, owner);
        step(&mut restored, owner);
    }
    assert_eq!(state(&restored), state(&uninterrupted));
}

/// El soul suministra bytes inválidos y una sonda de su registro completo y máximo ID.
/// La sonda no debe modificar el grafo. Su representación no se impone a otros souls;
/// no se comparan snapshots ni identificadores entre instancias.
pub fn rejected_import_is_atomic<G: Graph, S: std::fmt::Debug + PartialEq>(
    factory: impl Fn() -> G,
    invalid: &[u8],
    observe: impl Fn(&G) -> S,
) {
    let mut g = factory();
    let z = g.attr("z");
    let a = g.attr("a");
    let x = g.create();
    let y = g.create();
    g.set(x, a, Value::Int(1));
    g.relate(x, z, y);
    g.destroy(y);
    let before = state(&g);
    let private_before = observe(&g);
    assert!(g.import(invalid).is_err());
    assert_eq!(state(&g), before);
    assert_eq!(observe(&g), private_before);
    assert_eq!(g.attr("z"), z);
    assert_eq!(g.attr("a"), a);
    assert!(g.create().0 > y.0);
}

/// Comprueba bits directamente, sin delegar el oráculo numérico a Value::eq.
pub fn float_bits<G: Graph>(factory: impl Fn() -> G) {
    fn bits(value: Option<Value>) -> u64 {
        match value {
            Some(Value::Float(f)) => f.to_bits(),
            _ => panic!("expected Float"),
        }
    }
    let mut patterns = vec![
        0,
        1,
        0x000f_ffff_ffff_ffff,
        0x0010_0000_0000_0000,
        0x3ff0_0000_0000_0000,
        0x3ff0_0000_0000_0001,
        0x7fef_ffff_ffff_ffff,
        0x7ff0_0000_0000_0000,
        0x7ff8_0000_0000_0000,
        0x7ff8_0000_0000_0001,
        0x7ff8_dead_beef_0001,
        0x7ff0_0000_0000_0001,
    ];
    let positive = patterns.clone();
    patterns.extend(positive.into_iter().map(|p| p | (1u64 << 63)));
    let names: Vec<_> = (0..patterns.len())
        .map(|i| format!("float{i:03}"))
        .collect();
    let mut source = factory();
    let owner = source.create();
    for (name, &pattern) in names.iter().zip(&patterns) {
        let attr = source.attr(name);
        source.set(owner, attr, Value::Float(f64::from_bits(pattern)));
        assert_eq!(bits(source.get(owner, attr)), pattern);
    }
    let replace = source.attr("replace");
    for pattern in [0, 1u64 << 63, 0x7ff8_0000_0000_0001, 0x7ff8_0000_0000_0002] {
        source.set(owner, replace, Value::Float(f64::from_bits(pattern)));
        assert_eq!(bits(source.get(owner, replace)), pattern);
    }
    for _ in 0..2 {
        let bytes = source.export();
        let mut receiver = factory();
        for name in names.iter().rev() {
            receiver.attr(name);
        }
        receiver.import(&bytes).unwrap();
        let props = receiver.props(owner);
        for (name, &pattern) in names.iter().zip(&patterns) {
            let attr = receiver.attr(name);
            assert_eq!(bits(receiver.get(owner, attr)), pattern);
            let value = props.iter().find(|(n, _)| n == name).unwrap().1.clone();
            assert_eq!(bits(Some(value)), pattern);
        }
        let replacement = receiver.attr("replace");
        assert_eq!(
            bits(receiver.get(owner, replacement)),
            0x7ff8_0000_0000_0002
        );
        source = receiver;
    }
}

/// Autorref, ciclos y espacios propiedad/relación sobreviven import repetido.
pub fn cyclic_roundtrip<G: Graph>(factory: impl Fn() -> G) {
    let mut source = factory();
    let a = source.attr("a");
    let x = source.create();
    let y = source.create();
    source.set(x, a, Value::Concept(x));
    source.set(y, a, Value::Concept(x));
    source.relate(x, a, x);
    source.relate(x, a, y);
    source.relate(y, a, x);
    let before = state(&source);
    let bytes = source.export();
    let mut receiver = factory();
    receiver.attr("unrelated");
    let ra = receiver.attr("a");
    for _ in 0..2 {
        receiver.import(&bytes).unwrap();
        assert_eq!(state(&receiver), before);
        assert_eq!(receiver.attr("a"), ra);
        assert_eq!(receiver.related(x, ra), vec![x, y]);
    }
}

/// Truncar o cambiar bytes no garantiza invalidez. Se exige ausencia de pánico y,
/// solo ante Err, conservación del observable, Attr conocidos y estado de la sonda.
/// La sonda debe cubrir registro completo y máximo ID sin modificar el grafo.
pub fn modified_import_preserves_state_on_error<G: Graph, S: std::fmt::Debug + PartialEq>(
    factory: impl Fn() -> G,
    observe: impl Fn(&G) -> S,
) {
    fn check<G: Graph, S: std::fmt::Debug + PartialEq>(
        factory: &impl Fn() -> G,
        bytes: &[u8],
        observe: &impl Fn(&G) -> S,
    ) {
        let mut receiver = factory();
        let known: Vec<_> = ["z", "a", "receiver-only"]
            .into_iter()
            .map(|name| (name, receiver.attr(name)))
            .collect();
        let owner = receiver.create();
        let deleted = receiver.create();
        receiver.set(owner, known[1].1, Value::Int(42));
        receiver.relate(owner, known[0].1, owner);
        receiver.destroy(deleted);
        let before = state(&receiver);
        let private_before = observe(&receiver);
        if receiver.import(bytes).is_err() {
            assert_eq!(state(&receiver), before);
            assert_eq!(observe(&receiver), private_before);
            for (name, attr) in known {
                assert_eq!(receiver.attr(name), attr);
            }
            assert!(receiver.create().0 > deleted.0);
        }
    }
    let mut source = factory();
    let incoming = source.attr("incoming-name");
    let owner = source.create();
    source.set(owner, incoming, Value::Text("snapshot".into()));
    let bytes = source.export();
    for end in 0..bytes.len() {
        check(&factory, &bytes[..end], &observe);
    }
    for index in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[index] ^= 0xff;
        check(&factory, &changed, &observe);
    }
}
