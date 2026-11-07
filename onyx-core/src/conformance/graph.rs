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

/// El soul suministra bytes inválidos para su propio formato, sin imponer un formato genérico.
pub fn rejected_import_is_atomic<G: Graph>(factory: impl Fn() -> G, invalid: &[u8]) {
    let mut g = factory();
    let z = g.attr("z");
    let a = g.attr("a");
    let x = g.create();
    let y = g.create();
    g.set(x, a, Value::Int(1));
    g.relate(x, z, y);
    g.destroy(y);
    let before = state(&g);
    assert!(g.import(invalid).is_err());
    assert_eq!(state(&g), before);
    assert_eq!(g.attr("z"), z);
    assert_eq!(g.attr("a"), a);
    assert!(g.create().0 > y.0);
}
