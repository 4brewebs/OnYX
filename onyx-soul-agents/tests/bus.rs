//! `AgentBus` cumple la semántica FIFO del puerto `EventBus` de core.

use onyx_soul_agents::AgentBus;
use onyx_soul_agents::{EventBus, OnyxEvent, SystemEvent};

fn tick(n: u64) -> OnyxEvent {
    OnyxEvent::System(SystemEvent::Tick { n })
}

fn ticks(events: &[OnyxEvent]) -> Vec<u64> {
    events
        .iter()
        .map(|e| match e {
            OnyxEvent::System(SystemEvent::Tick { n }) => *n,
            other => panic!("evento inesperado: {other:?}"),
        })
        .collect()
}

#[test]
fn drain_returns_every_pushed_event_in_order_and_empties_the_bus() {
    let mut bus = AgentBus::new();
    assert!(bus.is_empty());

    for n in 1..=3 {
        bus.push(tick(n));
    }
    assert!(!bus.is_empty());
    assert_eq!(ticks(&bus.drain()), [1, 2, 3]);
    assert!(bus.is_empty());
    assert!(bus.drain().is_empty());

    bus.push(tick(4));
    assert_eq!(ticks(&bus.drain()), [4]);
}
