//! Suite pública de conformidad del grafo de core aplicada a `AgentGraph`.
//! La feature `conformance` de core solo se activa aquí, como dependencia de desarrollo.
use onyx_core::{Graph, conformance::graph};
use onyx_soul_agents::AgentGraph;

macro_rules! contract_cases {
    ($($case:ident),* $(,)?) => {$ (
        #[test]
        fn $case() { graph::$case(AgentGraph::new); }
    )*};
}
contract_cases!(
    registry,
    creation_and_queries,
    set_and_missing_references,
    ordering_and_relations,
    destruction,
    values_roundtrip,
    import_remaps_attributes,
    high_water,
    continuation,
    float_bits,
    cyclic_roundtrip
);

#[test]
fn rejected_import_is_atomic() {
    graph::rejected_import_is_atomic(
        AgentGraph::new,
        b"not an AgentGraph snapshot",
        AgentGraph::export,
    );
}

#[test]
fn modified_import_preserves_state_on_error() {
    // El export propio sirve de sonda porque incluye high_water y todos los nombres
    // del registro. Es una sonda de este soul, no un formato ni bytes canónicos.
    graph::modified_import_preserves_state_on_error(AgentGraph::new, AgentGraph::export);
}

#[test]
fn unrelate_preserves_target_and_unrelated_edges() {
    use onyx_soul_agents::{AttrRegistry, GraphView, Value};

    let mut g = AgentGraph::new();
    let rel = g.attr("rel");
    let other = g.attr("other");
    let mark = g.attr("mark");
    let source = g.create();
    let target = g.create();
    let witness = g.create();
    g.set(target, mark, Value::Int(7));
    g.set(witness, mark, Value::Concept(target));
    g.relate(source, rel, target);
    g.relate(source, other, target);
    g.relate(witness, rel, target);
    g.relate(target, rel, witness);
    for _ in 0..2 {
        g.unrelate(source, rel, target);
        assert!(g.exists(target), "unrelate solo elimina la arista");
        assert_eq!(g.get(target, mark), Some(Value::Int(7)));
        assert_eq!(g.get(witness, mark), Some(Value::Concept(target)));
        assert!(g.related(source, rel).is_empty());
        assert_eq!(g.related(source, other), vec![target]);
        assert_eq!(g.related(witness, rel), vec![target]);
        assert_eq!(g.related(target, rel), vec![witness]);
    }
    g.relate(source, rel, target);
    assert_eq!(g.related(source, rel), vec![target]);
}
