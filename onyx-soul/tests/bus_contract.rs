use onyx_core::{
    Attr, AttrRegistry, ConceptId, DomainEvent, EventBus, IngressEvent, OnyxEvent, SystemEvent,
    Value,
};
use onyx_soul::InMemoryBus;

#[derive(Default)]
struct TestRegistry(Vec<String>);

impl AttrRegistry for TestRegistry {
    fn attr(&mut self, name: &str) -> Attr {
        let index = match self.0.iter().position(|existing| existing == name) {
            Some(index) => index,
            None => {
                self.0.push(name.into());
                self.0.len() - 1
            }
        };
        Attr::from_raw(u32::try_from(index).expect("registro de prueba pequeño"))
    }
}

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
    let mut registry = TestRegistry::default();
    let kind = registry.attr("event.kind");
    let payload = registry.attr("event.payload");
    let mut bus = InMemoryBus::new();
    let port: &mut dyn EventBus = &mut bus;
    port.push(OnyxEvent::System(SystemEvent::Tick { n: 42 }));
    port.push(OnyxEvent::Ingress(IngressEvent::Raw {
        source: "device".into(),
        bytes: vec![0, 255],
    }));
    port.push(OnyxEvent::Domain(DomainEvent {
        kind,
        subject: Some(ConceptId(3)),
        data: vec![(payload, Value::Text("payload".into()))],
    }));
    assert!(!port.is_empty());
    let events = port.drain();
    assert!(port.is_empty());
    assert!(port.drain().is_empty());
    assert!(matches!(events.as_slice(), [
        OnyxEvent::System(SystemEvent::Tick { n: 42 }),
        OnyxEvent::Ingress(IngressEvent::Raw { source, bytes }),
        OnyxEvent::Domain(DomainEvent { kind: received_kind, subject: Some(ConceptId(3)), data }),
    ] if *received_kind == kind && source == "device" && bytes == &[0, 255]
        && matches!(data.as_slice(), [(received_attr, Value::Text(text))] if *received_attr == payload && text == "payload")));
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
