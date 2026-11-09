# onyx-soul-agents

**Agent harness soul for OnYX.**

Implements the core ports for deterministic multi-agent applications:

- `AgentGraph` — in-memory EAV graph with agent helpers
- `AgentSnapshotStore` — tick-indexed snapshots (rewind to any tick)
- `AgentBus` — in-memory bus
- `RecordingBridge` / `ReplayBridge` — external decider with record/replay, `wrapping` and `starting_at`
- `HeadlessDriver` / `HeadlessHandle` — inspect surfaces and counts, including after `wrapping`
- `AgentHost` — mounts graph, bus, store, systems and the tick loop
- `AgentSpawner` + `AgentRuntime` — systems that only speak `OnyxEvent`
- `ToolSystem` / `ToolExecutor` — deterministic in-brain tools

The application decides stages, systems, tools, and policies.
This soul closes the framework so the app can be written against it.

`AgentDriver` remains for the older request/reply tests. New apps should use the bridge (and `AgentHost` if they do not want to wire `tick` themselves).

## Usage

```rust
use onyx_soul_agents::{
    AgentHost, AgentSpec, HeadlessDriver, RecordingBridge, Reply,
};

let mut host = AgentHost::builder(1).build()?;
host.spawn(&AgentSpec::new("alice", "research topic A"));

let headless = HeadlessDriver::new();
let monitor = headless.handle();
let mut driver = RecordingBridge::new(host.vocab(), |req| {
    Some(Reply::done(req.agent, req.request, "done"))
})
.wrapping(headless);

host.run_until_settled(&mut driver, 16);
let _seen = monitor.last_surfaces();
```
