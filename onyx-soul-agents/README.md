# onyx-soul-agents

Agent-oriented soul for OnYX. It implements the core ports without depending on `onyx-soul` and declares zero external crate dependencies (`onyx-core` only).

## Components

- `AgentGraph`: in-memory EAV graph with agent helpers, a private incoming-edge index, and surgical `destroy`. Its snapshot format is private to this soul, with no checksum and no migrations. The index is not serialized; import rebuilds it after remapping attributes.
- **Multi-turn lifecycle (`graph.rs`)**:
  - `append_agent_memory` — appends memory entries maintaining monotonic `mem_seq` / `mem_count` / `mem_tick`; if an agent is `AgentState::Done` with a non-empty `vocab.result`, automatically flushes `result` into `"say"` first so post-turn `"system"` notes remain ordered chronologically.
  - `continue_agent` — reactivates a terminal (`Done` / `Failed`) agent for a follow-up user turn preserving `rel_memory` and keeping `vocab.steps` strictly monotonic (raising `max_steps = current_steps + extra_steps`) so `request = steps + 1` never collides with prior replies.
- `AgentSnapshotStore`: stores graph bytes under a tick key (`tick-{n}.bin`) or a free key. Keys that parse as `u64` map to `tick-{n}.bin`. The store does not save `n`, the seed, the bus, or system states.
- `AgentBus`: in-memory FIFO event bus.
- `RecordingBridge` and `ReplayBridge`: external decider & driver wrapper (`wrapping`, `starting_at`) producing deterministic `Transcript`s (`ONYXTR1` / `ONYXTR2`, capturing both `Reply` events with optional `call_id` and `IngressEvent::Command` inputs).
- `HeadlessDriver` and `HeadlessHandle`: inspect surfaces and counts, including after `wrapping`.
- `AgentHost`: mounts graph, bus, store, systems, and the `tick` loop (`step` passes `&[]`; `step_with` forwards declared `SurfaceBinding`s).
- `AgentSpawner`, `AgentRuntime`, and `SubagentSystem`: systems that only speak `OnyxEvent` (`SubagentSystem` handles `spawn_agent`, `AgentState::WaitingChildren`, child completion/failure propagation, and parent resumption).
- `ToolSystem` and `ToolExecutor`: deterministic in-brain tools (`ToolCall`, `ToolCtx`) touching only the graph and `DetRng`.
- **External tool calls & human-in-the-loop approvals**:
  - `AgentState::WaitingApproval` and `ApprovalPolicy` (`AgentSpec::with_approval_tools`, `PendingApproval`, `pending_approvals`)
  - Structured calls via `Reply::call_structured` (`call_id`) and `pending_tool_calls`
  - Approval verbs `Verb::Approve` / `Verb::Deny` (`Reply::approve`, `Reply::deny`)

`AgentDriver` remains for older request/reply tests. New apps should use `RecordingBridge` / `ReplayBridge` (and `AgentHost` if they do not want to wire `tick` themselves).

## Usage

The full tour — a `ToolExecutor`, `step_with` with a surface, a snapshot after `say`, a second host, `set_tick` and `ReplayBridge::starting_at` — lives in the `onyx-example-agents` crate. For a complete coding harness with workspace tools, approvals, multi-turn chat, `/undo`, TUI, and snapshot rewind, see `onyx-workspace` and `open-onyx`.

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

`Search` in this snippet is a `ToolExecutor`. `pending_requests` only lists agents in `Thinking`, so the bridge does not ask the decider while the agent is `Acting`. After a `tool_error`, the runtime returns the agent to `Idle` and opens a new request; this snippet answers it with `Reply::fail`. Returning `None` leaves that request unanswered and the agent fails by timeout.

## Roadmap / TODO (Not Yet Implemented)

- **Unified recovery unit**: bundle `AgentGraph` snapshot, tick `n`, RNG seed, execution root, and `Transcript` with strict divergence validation so restore + replay cannot drift silently.
