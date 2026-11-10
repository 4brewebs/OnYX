//! Utilidades internas compartidas por los sistemas.

use onyx_core::{Attr, ConceptId, DomainEvent, GraphView, OnyxEvent, SystemEvent, Value};

/// El tick actual llega en el inbox como `SystemEvent::Tick`.
/// El core no entrega el número de tick de otra forma a los sistemas.
pub(crate) fn current_tick(inbox: &[OnyxEvent]) -> Option<u64> {
    inbox.iter().find_map(|e| match e {
        OnyxEvent::System(SystemEvent::Tick { n }) => Some(*n),
        _ => None,
    })
}

pub(crate) fn int<G: GraphView + ?Sized>(g: &G, id: ConceptId, attr: Attr) -> Option<i64> {
    match g.get(id, attr) {
        Some(Value::Int(i)) => Some(i),
        _ => None,
    }
}

pub(crate) fn text<G: GraphView + ?Sized>(g: &G, id: ConceptId, attr: Attr) -> Option<String> {
    match g.get(id, attr) {
        Some(Value::Text(s)) => Some(s),
        _ => None,
    }
}

pub(crate) fn domain(
    kind: Attr,
    subject: Option<ConceptId>,
    data: Vec<(Attr, Value)>,
) -> OnyxEvent {
    OnyxEvent::Domain(DomainEvent {
        kind,
        subject,
        data,
    })
}

pub(crate) fn fault(msg: String) -> OnyxEvent {
    OnyxEvent::System(SystemEvent::Fault(msg))
}

/// Valor de `key` en el evento. Si la clave se repite, gana la última
/// aparición, igual que en [`crate::AgentConfig::from_params`].
pub(crate) fn data(d: &DomainEvent, key: Attr) -> Option<&Value> {
    d.data.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v)
}
