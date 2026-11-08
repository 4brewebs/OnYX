//! Multi-agent application that uses `onyx-soul-agents` as its soul.
//!
//! Demonstrates:
//! - stages and systems registered via the Loader
//! - tick-indexed snapshots
//! - agent spawn via events
//! - a simple scripted responder that drives the agents

use onyx_core::{
    BlueprintLoader, InMemorySystemRegistry, PlatformDriver, RustLoader, SystemRegistry,
};
use onyx_soul_agents::{
    spawn_event, AgentBus, AgentDriver, AgentGraph, AgentSnapshotStore, AgentSpec, FACTORIES, Reply,
    Vocab, stage,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut graph = AgentGraph::new();
    let mut store = AgentSnapshotStore::new("snapshots");
    let mut bus = AgentBus::new();
    let mut driver = AgentDriver::new();

    let mut registry = InMemorySystemRegistry::new();
    let mut loader = RustLoader::new();

    for (name, factory) in FACTORIES {
        registry.register(name, factory);
    }

    let agents = stage("agents");
    let mut systems = loader.load_app(&mut registry, &[agents], "agents")?;

    let vocab = Vocab::resolve(graph.inner_mut());
    driver.set_vocab(vocab);

    let spec_a = AgentSpec {
        name: "alice".into(),
        goal: "research topic A".into(),
        max_steps: 8,
        timeout: 16,
        tools: vec!["search".into()],
    };
    let spec_b = AgentSpec {
        name: "bob".into(),
        goal: "summarize topic B".into(),
        max_steps: 8,
        timeout: 16,
        tools: vec!["read".into()],
    };

    bus.push(spawn_event(&vocab, &spec_a));
    bus.push(spawn_event(&vocab, &spec_b));

    for tick in 0..12u64 {
        let bytes = graph.export();
        store.save_tick(tick, &bytes)?;

        for req in driver.last_requests() {
            driver.submit_reply(Reply {
                agent: req.agent,
                verb: "finish".into(),
                tool: None,
                args: None,
                text: Some(format!("done at tick {tick}")),
            });
        }

        let outcome = onyx_core::tick(
            &mut driver,
            graph.inner_mut(),
            &mut systems,
            &mut bus,
            &mut store,
            tick,
        )?;

        println!("tick {tick}: agents={}", graph.agent_count(&vocab));
        if outcome.shutdown {
            break;
        }
    }

    println!("finished");
    Ok(())
}
