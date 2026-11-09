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
