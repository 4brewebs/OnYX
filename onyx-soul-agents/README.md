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

`request_snapshot` and `request_restore` are served at the end of the next tick, like any other control event. The graph snapshot does not include in-flight events (values the outbox returns to the bus). The caller keeps those events. After restore, to match an uninterrupted run, push them back with `AgentHost::push`. The recovery tests do that on every cut from tick 0 through 28 of a 30-tick run. Without that re-injection, only a quiet cut (empty outbox) matches, which is what the quiet-cut control and `onyx-example-agents` check.

`request_restore` does not realign the host counter. `ticks` is the `n` of the next `step`. After a restore, call `set_tick` so that `tick` and `DetRng` see the intended `n`. Pair that with `ReplayBridge::starting_at` (or `RecordingBridge::starting_at`) on the same poll offset. Compare `fingerprint()` and contract events, not snapshot bytes.

This is implemented and covered by host and recovery tests, and by `onyx-example-agents`. It is not the complete `TickState` recovery unit proposed in the root [README_es.md](../README_es.md).

## Designed or not yet implemented

The `operation` module is in development. `OperationLog` is the graph-storage layer for an independent concept per external tool call. Its ten control fields (`schema`, `execution`, `agent`, `request`, `tool`, `args`, `route`, `state`, `outcome_kind`, and `outcome_text`) use `<prefix>.operation.<field>` and are reserved against agent extras for that configured prefix. This does not reserve arbitrary names under `operation.*`, fields from other prefixes, or direct writes through `Graph`. The runtime does not create these records yet. Receiver dispatch, recovery after an external effect, and strict TR2 replay are not connected. Passing the existing tests does not establish those guarantees.

Two integration limits remain explicit: a restore from before agent creation into the same graph can recreate the agent with a different `ConceptId`, so the current storage key is not enough to deduplicate that continuation. Also, `is_settled` and `run_until_settled` describe agent completion, not resolution of independent tool operations. These cases must be handled before external dispatch is enabled.

- **TOIMPLEMENT.** TR2 and external tools. A tool name with no `ToolExecutor` is left for another system or a driver to answer (`ev.tool_result` or an observe `Reply`). If nobody answers, the agent times out. TR1 records decider replies. The proposed TR2 design records the complete operation identity and exact tool request, including its receiver route. Replay must reject divergent requests without invoking the receiver. At-least-once delivery and a key stored in the host alone do not prevent duplicate effects: safely repeating a send requires atomic deduplication at the receiver. Reconciliation may establish a known outcome; an unknown outcome without safe repetition must remain uncertain, without automatic retry. None of this is implemented yet or promises exactly-once delivery.
- **TODO** in `RustLoader`: `SystemSpec::persistent` and `StageBlueprint::entities` are ignored. Runtime stage change (`LoadStage`) is proposed design, not implemented.
- TUI and WASM drivers are not part of this crate.

## Usage

The full tour, a `ToolExecutor`, `step_with` with a surface, a snapshot after `say`, a second host, `set_tick`, and `ReplayBridge::starting_at`, lives in the [`onyx-example-agents`](../onyx-example-agents/README.md) crate.

```rust
use onyx_soul_agents::{
    AgentHost, AgentSpec, AgentView, HeadlessDriver, RecordingBridge, Reply,
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
let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
    let has = |kind: &str| req.memory.iter().any(|(k, _)| k == kind);
    if has("observe") {
        Some(Reply::done(req.agent, req.request, "ok"))
    } else if has("tool_error") {
        Some(Reply::fail(req.agent, req.request, "search failed"))
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

`Search` in this snippet is a `ToolExecutor`. `pending_requests` only lists agents in `Thinking`, so the bridge does not ask the decider while the agent is `Acting`. After a `tool_error`, the runtime returns the agent to `Idle` and opens a new request; this snippet answers it with `Reply::fail`. Returning `None` leaves that request unanswered and the agent fails by timeout. The example crate has a complete, runnable success path.
