# onyx-soul-agents

Agent-oriented soul for OnYX. It implements the core ports and does not depend on `onyx-soul`.

The application decides stages, systems, tools, and policies. This soul closes the framework so the app can be written against it and depend only on this crate. Core contracts the app needs are reexported at the crate root.

## What exists today

- `AgentGraph`: in-memory EAV graph with agent helpers, a private incoming-edge index, and surgical `destroy`. Its snapshot format is private to this soul, with no checksum and no migrations. The index is not serialized; import rebuilds it after remapping attributes.
- `AgentSnapshotStore`: stores graph bytes under a tick key (`tick-{n}.bin`) or a free-form key, so an app can reload a previous graph. The store does not save `n`, the seed, the bus, or system states.
- `AgentBus`: in-memory bus.
- `RecordingBridge` and `ReplayBridge`: external decider with record and replay. The transcript format is TR1 (`ONYXTR1`), with no checksum. Both expose `wrapping` (chain another `PlatformDriver`) and `starting_at` (align the poll cursor after a restore, without rewriting the transcript).
- `HeadlessDriver` and `HeadlessHandle`: inspect surfaces and counts, including after `wrapping`.
- `AgentHost`: mounts graph, bus, store, systems, and the tick loop. `step` declares no surfaces. `step_with` forwards the slice the app provides. `run` and `run_until_settled` call `step`, so they pass an empty surface list.
- `AgentSpawner` and `AgentRuntime`: systems that only speak `OnyxEvent`.
- `ToolSystem` and `ToolExecutor`: deterministic in-brain tools. An executor may touch only the graph and `DetRng`.

`AgentDriver` remains for the older request and reply tests. New apps should use the bridge, and `AgentHost` if they do not want to wire `tick` themselves.

## Recovery today

`request_snapshot` and `request_restore` are served at the end of the next tick, like any other control event. The graph snapshot does not include in-flight events (values the outbox returns to the bus). Take a snapshot at a quiet frontier, or the continued run will not match an uninterrupted one unless those events are supplied again.

`request_restore` does not realign the host counter. `ticks` is the `n` of the next `step`. After a restore, call `set_tick` so that `tick` and `DetRng` see the intended `n`. Pair that with `ReplayBridge::starting_at` (or `RecordingBridge::starting_at`) on the same poll offset. Compare `fingerprint()` and contract events, not snapshot bytes.

This is implemented and covered by host and recovery tests, and by `onyx-example-agents`. It is not the complete `TickState` recovery unit proposed in the root [README_es.md](../README_es.md).

## Designed or not yet implemented

- **TOIMPLEMENT.** TR2 and external tools. A tool name with no `ToolExecutor` is left for another system or a driver to answer (`ev.tool_result` or an observe `Reply`). If nobody answers, the agent times out. TR1 records decider replies. The proposed TR2 design records the full tool request (tool, arguments, and id). Replay fails clearly if that record diverges. After a restore, delivery is at least once. The call id is persisted in the graph as an idempotency key, and the receiver or the host uses that key to avoid duplicating the effect. This is not a promise of exactly-once delivery.
- **TODO** in `RustLoader`: `SystemSpec::persistent` and `StageBlueprint::entities` are ignored. Runtime stage change (`LoadStage`) is proposed design, not implemented.
- TUI and WASM drivers are not part of this crate.

## Usage

The full tour, a `ToolExecutor`, `step_with` with a surface, a snapshot after `say`, a second host, `set_tick`, and `ReplayBridge::starting_at`, lives in the [`onyx-example-agents`](../onyx-example-agents/README.md) crate.

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
    } else if has("call") {
        // The tool result arrives later as `observe`; do not invent another reply
        // while that event is in flight.
        None
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

`Search` in this snippet is a `ToolExecutor`. The example crate has a complete, runnable version.
