use onyx_core::{Graph, conformance::graph};
use onyx_soul::WorldGraph;

macro_rules! contract_cases {
    ($($case:ident),* $(,)?) => {$ (
        #[test]
        fn $case() { graph::$case(WorldGraph::new); }
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
        WorldGraph::new,
        b"not a WorldGraph snapshot",
        WorldGraph::export,
    );
}

#[test]
fn modified_import_preserves_state_on_error() {
    // El codec privado de WorldGraph incluye todos los nombres y el máximo ID.
    // Esta sonda es de este soul, no un requisito de formato o bytes canónicos.
    graph::modified_import_preserves_state_on_error(WorldGraph::new, WorldGraph::export);
}
