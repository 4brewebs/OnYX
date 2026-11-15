# onyx-soul-agents

Agent-oriented soul for OnYX. It implements the core ports and does not depend on `onyx-soul`.

Implements the core ports for deterministic multi-agent applications and coding harnesses without any external dependencies (`onyx-core` only):

- `AgentGraph` — in-memory EAV graph with agent helpers (`spawn`, tools, memory, approvals)
- `AgentSnapshotStore` — tick-indexed snapshots (inspect and rewind to any tick)
- `AgentBus` — in-memory FIFO event bus
- `RecordingBridge` / `ReplayBridge` — external decider & driver wrapper (`wrapping`, `starting_at`) producing deterministic `Transcript`s (`ONYXTR1` / `ONYXTR2`, capturing both `Reply` events and `IngressEvent::Command`)
- `HeadlessDriver` / `HeadlessHandle` — inspect surfaces and counts, including after `wrapping`
- `AgentHost` / `HostBuilder` — mounts graph, bus, store, systems, and the `tick` loop
- `AgentSpawner` + `AgentRuntime` — systems that only speak `OnyxEvent`
- `ToolSystem` / `ToolExecutor` — deterministic in-brain tools (`ToolCall`, `ToolCtx`)
- **External tool calls & human-in-the-loop approvals**:
  - `AgentState::WaitingApproval` and `ApprovalPolicy` (`AgentSpec::with_approval_tools`, `PendingApproval`, `pending_approvals`)
  - Structured calls via `Reply::call_structured` (`call_id`) and `pending_tool_calls`
  - Approval verbs `Verb::Approve` / `Verb::Deny` (`Reply::approve`, `Reply::deny`)

- `AgentGraph`: in-memory EAV graph with agent helpers, a private incoming-edge index, and surgical `destroy`. Its snapshot format is private to this soul, with no checksum and no migrations. The index is not serialized; import rebuilds it after remapping attributes.
- `AgentSnapshotStore`: stores graph bytes under a tick key (`tick-{n}.bin`) or a free key, so an app can reload a previous graph. A key that parses as `u64` is a tick (`3`, `003`, and `+3` all mean `tick-3.bin`). A free key must be a single relative file name inside the store directory and must not be a canonical tick file name such as `tick-3.bin`; other keys are rejected with `OnyxError::Format`. The store does not save `n`, the seed, the bus, or system states.
- `AgentBus`: in-memory bus.
- `RecordingBridge` and `ReplayBridge`: external decider with record and replay. The transcript format is TR1 (`ONYXTR1`), with no checksum. Both expose `wrapping` (chain another `PlatformDriver`) and `starting_at` (align the poll cursor after a restore, without rewriting the transcript).
- `HeadlessDriver` and `HeadlessHandle`: inspect surfaces and counts, including after `wrapping`.
- `AgentHost`: mounts graph, bus, store, systems, and the tick loop. `step` declares no surfaces. `step_with` forwards the slice the app provides. `run` and `run_until_settled` call `step`, so they pass an empty surface list.
- `AgentSpawner` and `AgentRuntime`: systems that only speak `OnyxEvent`.
- `ToolSystem` and `ToolExecutor`: deterministic in-brain tools. An executor may touch only the graph and `DetRng`.

`AgentDriver` remains for the older request/reply tests. New apps should use `RecordingBridge` / `ReplayBridge` (and `AgentHost` if they do not want to wire `tick` themselves).

## Usage

The full tour — a `ToolExecutor`, `step_with` with a surface, a snapshot after `say`, a second host, `set_tick` and `ReplayBridge::starting_at` — lives in the `onyx-example-agents` crate. For a full coding harness with workspace tools, approvals, TUI, and snapshot rewind, see `onyx-workspace` and `open-onyx`.

```rust
use onyx_soul_agents::{
    AgentHost, AgentSpec, AgentView, HeadlessDriver, RecordingBridge, Reply,
    SurfaceBinding, SurfaceId,
};

let mut host = AgentHost::builder(1)
    .tool(Box::new(Search))
    .build()?;
host.spawn(
    &AgentSpec::new("alice", "research topic A")
        .with_tools(["search"])
        .with_approval_tools(["search"]),
);

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

## Roadmap / TODO (Not Yet Implemented)

- **Unified recovery unit**: bundle `AgentGraph` snapshot, tick `n`, RNG seed, execution root, and `Transcript` with divergence validation so restore + replay cannot drift silently.
- **Parent/child orchestration helpers**: `WaitingChildren` / join-style coordination on top of `rel_parent` for structured multi-agent delegation.
