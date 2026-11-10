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

The full tour — a `ToolExecutor`, `step_with` with a surface, a snapshot after `say`, a second host, `set_tick` and `ReplayBridge::starting_at` — lives in the `onyx-example-agents` crate.

```rust
use onyx_soul_agents::{
    AgentHost, AgentSpec, HeadlessDriver, RecordingBridge, Reply,
    SurfaceBinding, SurfaceId,
};

let mut host = AgentHost::builder(1)
    .tool(Box::new(Search))
    .build()?;
host.spawn(&AgentSpec::new("alice", "research topic A").with_tools(["search"]));

let view = host.attr("view.main");
let surfaces = [SurfaceBinding {
    surface: SurfaceId(1),
    view,
    focused: true,
}];
let headless = HeadlessDriver::new();
let monitor = headless.handle();
let mut driver = RecordingBridge::new(host.vocab(), |req| {
    let has = |kind: &str| req.memory.iter().any(|(k, _)| k == kind);
    if has("observe") {
        Some(Reply::done(req.agent, req.request, "ok"))
    } else if has("say") {
        Some(Reply::call(req.agent, req.request, "search q"))
    } else {
        Some(Reply::say(req.agent, req.request, "thinking"))
    }
})
.wrapping(headless);

host.step_with(&mut driver, &surfaces);
let _seen = monitor.last_surfaces();
```
