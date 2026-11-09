//! Monitor headless: contadores, superficies y handle tras `wrapping`.

use onyx_soul_agents::{
    AgentBus, AgentGraph, AgentView, AttrRegistry, EventBus, HeadlessDriver, OnyxEvent,
    PlatformDriver, RecordingBridge, SurfaceBinding, SurfaceId, SystemEvent, Vocab,
};

#[test]
fn headless_records_surfaces_and_replaces_them_on_each_present() {
    let mut graph = AgentGraph::new();
    let view = graph.attr("view.main");
    let first = [SurfaceBinding {
        surface: SurfaceId(7),
        view,
        focused: true,
    }];
    let mut driver = HeadlessDriver::new();
    let mut bus = AgentBus::new();
    driver.poll(&mut bus);
    driver.present(&graph, &first);
    assert_eq!(driver.poll_count(), 1);
    assert_eq!(driver.present_count(), 1);
    assert_eq!(driver.last_surfaces(), first);

    driver.present(&graph, &[]);
    assert_eq!(driver.present_count(), 2);
    assert!(
        driver.last_surfaces().is_empty(),
        "cada present sustituye el slice, no lo acumula"
    );
}

#[test]
fn handle_survives_wrapping_and_sees_the_inner_present() {
    let mut graph = AgentGraph::new();
    let vocab = Vocab::resolve(&mut graph);
    let view = graph.attr("view.main");
    let surfaces = [SurfaceBinding {
        surface: SurfaceId(1),
        view,
        focused: false,
    }];
    let headless = HeadlessDriver::new();
    let handle = headless.handle();
    let mut bridge = RecordingBridge::new(vocab, |_: &AgentView| None).wrapping(headless);
    let mut bus = AgentBus::new();
    bridge.poll(&mut bus);
    bridge.present(&graph, &surfaces);
    assert_eq!(handle.poll_count(), 1);
    assert_eq!(handle.present_count(), 1);
    assert_eq!(handle.last_surfaces(), surfaces);
}

#[test]
fn handle_injects_events_through_wrapping_on_the_next_poll() {
    let mut graph = AgentGraph::new();
    let vocab = Vocab::resolve(&mut graph);
    let headless = HeadlessDriver::new();
    let handle = headless.handle();
    let mut bridge = RecordingBridge::new(vocab, |_: &AgentView| None).wrapping(headless);
    handle.push_event(OnyxEvent::System(SystemEvent::Tick { n: 42 }));
    let mut bus = AgentBus::new();
    bridge.poll(&mut bus);
    let inbox = bus.drain();
    assert!(
        inbox
            .iter()
            .any(|event| matches!(event, OnyxEvent::System(SystemEvent::Tick { n: 42 }))),
        "el handle tiene que encolar aunque el driver ya esté dentro de wrapping"
    );
}

#[test]
fn two_handles_share_the_same_counts() {
    let mut driver = HeadlessDriver::new();
    let first = driver.handle();
    let second = driver.handle();
    let mut bus = AgentBus::new();
    driver.poll(&mut bus);
    assert_eq!(first.poll_count(), 1);
    assert_eq!(second.poll_count(), 1);
}
