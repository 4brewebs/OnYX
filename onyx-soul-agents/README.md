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

When vocabularies share a graph, their attribute names must not overlap. Different prefixes alone do not ensure this: `agent` and `agent.operation` share `agent.operation.state`. Prefix validation does not detect such overlaps. `AgentConfig::attr` also resolves reserved names. It does not isolate app attributes from `Vocab` or the ten `OperationLog` fields under that prefix. Spawn extras skip the agent fields, the tool and memory fields (`tool.name`, `mem.*`), the operation fields of the same prefix, and the eight fixed root and identity fields.

`AgentDriver` remains for the older request and reply tests. New apps should use the bridge, and `AgentHost` if they do not want to wire `tick` themselves.

## Recovery today

`request_snapshot` and `request_restore` are served at the end of the next tick, like any other control event. The graph snapshot does not include in-flight events (values the outbox returns to the bus). The caller keeps those events. After restore, to match an uninterrupted run, push them back with `AgentHost::push`. The recovery tests do that on every cut from tick 0 through 28 of a 30-tick run. Without that re-injection, only a quiet cut (empty outbox) matches, which is what the quiet-cut control and `onyx-example-agents` check.

`request_restore` does not realign the host counter. `ticks` is the `n` of the next `step`. After a restore, call `set_tick` so that `tick` and `DetRng` see the intended `n`. Pair that with `ReplayBridge::starting_at` (or `RecordingBridge::starting_at`) on the same poll offset. Compare `fingerprint()` and contract events, not snapshot bytes.

This is implemented and covered by host and recovery tests, and by `onyx-example-agents`. It is not the complete `TickState` recovery unit proposed in the root [README_es.md](../README_es.md).

## Designed or not yet implemented

The `operation` module is in development. `OperationLog` is the graph-storage layer for an independent concept per external tool call. Its key is `(execution, birth, request)`, with the tool channel implicit in the type; it never uses the agent's `ConceptId`. Its ten control fields (`schema`, `execution`, `birth`, `request`, `tool`, `args`, `route`, `state`, `outcome_kind`, and `outcome_text`) use `<prefix>.operation.<field>` and are reserved against agent extras for that configured prefix. This does not reserve arbitrary names under `operation.*`, fields from other prefixes, or direct writes through `Graph`. The runtime does not create these records yet. Receiver dispatch, recovery after an external effect, and strict TR2 replay are not connected. Passing the existing tests does not establish those guarantees.

The `root` module adds an explicit execution root and logical agent births. With `AgentConfig::execution` (an app-supplied `ExecutionId` plus a compatibility identifier), a fresh run starts with `begin_event` / `AgentHost::begin_execution` on an empty graph, before the first recoverable snapshot. The root lives under fixed names (`onyx.agents.root.*`), independent of `vocab_prefix`, and stores the execution, the exact prefix and configuration, the compatibility identifier, and `next_birth` (canonical `u64` text). Each valid spawn validates the root and reserves the next birth in the same `run` that creates the agent; rejected spawns do not consume births, `destroy` does not return them, and the counter is rejected when exhausted instead of wrapping. Agents store `onyx.agents.agent.execution` and `onyx.agents.agent.birth` as text; operations keep execution and birth as values. Restoring a snapshot from before an agent's creation into the same graph and repeating the spawn history gives the agent the same birth, even if its `ConceptId` differs. Resuming never creates a root: `execution_root` / `AgentHost::execution_root` reject a missing (even on an empty graph) or duplicate root, a different execution, prefix, configuration, or compatibility identifier, agents without a birth, records of another execution, duplicate births, and births not below `next_birth`. A spawner without execution configuration keeps the legacy behavior on graphs without a root, and rejects spawns on a graph that has one. The soul cannot inspect policies, memory, or tools: the compatibility identifier is the app's declaration for that semantics.

This does not make rewind safe. Birth and request counters rewind with the snapshot, and an equal key or descriptor does not prove the same intent; a snapshot taken between two operations of the same agent rewinds `request` without recreating the agent. Proving the receiver's frontier (complete inventory at rest, a single dispatcher, exact atomic deduplication before every send) and verified TR2 replay are not implemented. Until they are, a continuation whose external history is ahead of the snapshot or unknown must be rejected before dispatch, on a new or the same host. Also, `is_settled` and `run_until_settled` describe agent completion, not resolution of independent tool operations.

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
