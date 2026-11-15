# onyx-soul-agents

**Agent harness soul for OnYX.**

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

The application decides stages, systems, tools, and policies.
This soul closes the framework so the app can be written against it.

`AgentDriver` remains for the older request/reply tests. New apps should use `RecordingBridge` / `ReplayBridge` (and `AgentHost` if they do not want to wire `tick` themselves).

## Usage

The full tour — a `ToolExecutor`, `step_with` with a surface, a snapshot after `say`, a second host, `set_tick` and `ReplayBridge::starting_at` — lives in the `onyx-example-agents` crate. For a full coding harness with workspace tools, approvals, TUI, and snapshot rewind, see `onyx-workspace` and `open-onyx`.

```rust
use onyx_soul_agents::{
    AgentHost, AgentSpec, HeadlessDriver, RecordingBridge, Reply,
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
