//! Multi-agent application that uses `onyx-soul-agents` as its soul.
//!
//! Demonstrates:
//! - stages and systems registered via the Loader
//! - tick-indexed snapshots
//! - agent spawn via events
//! - an external decider with `RecordingBridge` (record/replay)

use onyx_soul_agents::{
    AgentBus, AgentGraph, AgentSnapshotStore, AgentSpec, AgentView, EventBus, FACTORIES, Graph,
    InMemorySystemRegistry, RecordingBridge, Reply, RustLoader, Supervisor, Vocab, spawn_event,
    stage, tick,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut graph = AgentGraph::new();
    let mut store = AgentSnapshotStore::new("snapshots");
    let mut bus = AgentBus::new();
    let mut registry = InMemorySystemRegistry::new();
    let mut loader = RustLoader::new();
    let mut systems = loader.load_app(&mut registry, &FACTORIES, &[stage("agents")], "agents")?;

    let mut supervisor = Supervisor::new(systems.len());
    supervisor.init_all(&mut systems, &mut graph);
    for i in 0..systems.len() {
        assert!(
            supervisor.start(i),
            "el stage de agentes debe poder arrancar el sistema {i}"
        );
    }

    let vocab = Vocab::resolve(&mut graph);
    let mut driver = RecordingBridge::new(vocab, |req: &AgentView| {
        Some(Reply::done(
            req.agent,
            req.request,
            format!("done at request {}", req.request),
        ))
    });

    let spec_a = AgentSpec::new("alice", "research topic A")
        .with_tools(["search"])
        .with_max_steps(8)
        .with_timeout_ticks(16);
    let spec_b = AgentSpec::new("bob", "summarize topic B")
        .with_tools(["read"])
        .with_max_steps(8)
        .with_timeout_ticks(16);
    bus.push(spawn_event(&vocab, &spec_a, None));
    bus.push(spawn_event(&vocab, &spec_b, None));

    for n in 0..12u64 {
        store.save_tick(n, &graph.export())?;
        tick(
            n,
            1,
            &mut driver,
            &mut graph,
            &mut store,
            &mut bus,
            &mut systems,
            supervisor.states_mut(),
            &[],
        );
        println!("tick {n}: agents={}", graph.agent_count(&vocab));
    }

    println!("recorded {} replies", driver.transcript().entries.len());
    Ok(())
}
