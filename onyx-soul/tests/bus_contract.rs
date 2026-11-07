use onyx_core::{
    Attr, ConceptId, DomainEvent, EventBus, IngressEvent, OnyxEvent, SystemEvent, Value,
};
use onyx_soul::InMemoryBus;

#[test]
fn new_and_default_buses_are_empty_and_can_be_drained_repeatedly() {
    for mut bus in [InMemoryBus::new(), InMemoryBus::default()] {
        assert!(bus.is_empty());
        assert!(bus.drain().is_empty());
        assert!(bus.drain().is_empty());
        assert!(bus.is_empty());
    }
}

#[test]
fn drain_preserves_fifo_and_payloads_without_replaying_any_event() {
    let mut bus = InMemoryBus::new();
    let port: &mut dyn EventBus = &mut bus;
    port.push(OnyxEvent::System(SystemEvent::Tick { n: 42 }));
    port.push(OnyxEvent::Ingress(IngressEvent::Raw {
        source: "device".into(),
        bytes: vec![0, 255],
    }));
    port.push(OnyxEvent::Domain(DomainEvent {
        kind: Attr(7),
        subject: Some(ConceptId(3)),
        data: vec![(Attr(2), Value::Text("payload".into()))],
    }));
    assert!(!port.is_empty());
    let events = port.drain();
    assert!(port.is_empty());
    assert!(port.drain().is_empty());
    assert!(matches!(events.as_slice(), [
        OnyxEvent::System(SystemEvent::Tick { n: 42 }),
        OnyxEvent::Ingress(IngressEvent::Raw { source, bytes }),
        OnyxEvent::Domain(DomainEvent { kind: Attr(7), subject: Some(ConceptId(3)), data }),
    ] if source == "device" && bytes == &[0, 255]
        && matches!(data.as_slice(), [(Attr(2), Value::Text(text))] if text == "payload")));
}

#[test]
fn pushes_after_drain_belong_only_to_the_next_batch() {
    let mut bus = InMemoryBus::new();
    bus.push(OnyxEvent::System(SystemEvent::Start("worker".into())));
    let first = bus.drain();
    bus.push(OnyxEvent::System(SystemEvent::Stop("worker".into())));
    bus.push(OnyxEvent::System(SystemEvent::Tick { n: 1 }));
    let second = bus.drain();
    assert!(
        matches!(first.as_slice(), [OnyxEvent::System(SystemEvent::Start(name))] if name == "worker")
    );
    assert!(matches!(second.as_slice(), [
        OnyxEvent::System(SystemEvent::Stop(name)),
        OnyxEvent::System(SystemEvent::Tick { n: 1 }),
    ] if name == "worker"));
    assert!(bus.is_empty());
    assert!(bus.drain().is_empty());
}
