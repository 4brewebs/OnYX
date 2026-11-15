# OnYX

**A Lego of brains, not a closed solution.**

OnYX separates a small, dependency-free core of contracts from the souls that fulfill them.  
If your reality is different, write your own soul: the contract is what matters.

```
onyx-core           = contracts + tick + deterministic RNG            · 0 dependencies
onyx-soul           = default implementations of those contracts      · 0 external dependencies
onyx-soul-agents    = deterministic multi-agent harness soul          · 0 external dependencies
onyx-workspace      = sandboxed coding tools + bridge                 · uses onyx-soul-agents
open-onyx           = CLI/TUI coding agent harness (Gemini + ratatui) · uses onyx-soul-agents + onyx-workspace
onyx-example-app    = minimal REPL that exercises onyx-soul           · uses onyx-soul
onyx-example-agents = deterministic multi-agent tour                  · uses onyx-soul-agents
```

**Status: v0, under active design.**  
The public contract is not yet closed and will change in breaking ways.  
Do not use it where a failure has consequences.

## What exists today

### `onyx-core`
- `Graph` + `AttrRegistry` (concepts, attributes, directed relations, deterministic order)
- `OnyxEvent` (System / Ingress / Domain)
- `OnyxSystem` + `Ctx` + lifecycle hooks (`init`, `on_start`, `on_stop`, `on_restore`)
- Synchronous deterministic `tick` with `DetRng`
- Ports: `SnapshotStore`, `PlatformDriver`, `BlueprintLoader`, `SystemRegistry`
- `Supervisor` for system lifecycle

### `onyx-soul`
- `WorldGraph` (in-memory EAV)
- `FileSnapshotStore` (private binary format, 0 deps)
- `HeadlessDriver`
- `InMemoryBus`
- `InMemorySystemRegistry`
- `RustLoader` with `load_app` helper
- `Supervisor`

### `onyx-soul-agents`
- `AgentGraph`, `AgentSnapshotStore` (tick-indexed snapshots & rewind), `AgentBus`, `AgentHost`
- `AgentSpawner`, `AgentRuntime`, `ToolSystem` / `ToolExecutor`
- `RecordingBridge` / `ReplayBridge` (`ONYXTR1` / `ONYXTR2` transcripts), `HeadlessDriver`
- `AgentState::WaitingApproval`, `ApprovalPolicy`, structured tool calls (`Reply::call_structured`)

### `onyx-workspace`
- `WorkspacePolicy` (canonicalized root sandbox, ignored dirs, I/O and timeout limits)
- `WorkspaceTools` (`fs_read`, `fs_list`, `fs_write`, `fs_edit`, `code_search`, `shell_exec`)
- `WorkspaceToolBridge` (`PlatformDriver` adapter for `RecordingBridge` / `ReplayBridge`)

### `open-onyx` (`openonyx`)
- CLI & TUI coding agent harness (`run`, `chat`, `replay`, `inspect`, `rewind`)
- `GeminiResponder` (Google AI Studio REST API) & `TuiDriver` (`ratatui` + `crossterm`)
- Per-tick graph snapshots (`--snapshot-dir`), transcript saving, and time-travel rewind

### `onyx-example-app` & `onyx-example-agents`
- `onyx-example-app`: minimal REPL that loads systems/stages, restores/saves snapshots, and processes `agree`, `status`, `exit`
- `onyx-example-agents`: end-to-end tour of `AgentHost`, `RecordingBridge`, `AgentSnapshotStore`, and `ReplayBridge::starting_at`

## What does NOT exist yet

- WASM drivers
- Stage change at runtime
- TOML / config loaders
- Unified recovery bundle (snapshot + RNG seed + transcript in one artifact)
- Multi-soul interoperability tests

## Quick start

```bash
cargo run -p onyx-example-app
cargo run -p onyx-example-agents
cargo run -p open-onyx -- --help
```

```text
onyx> agree
[BOOT] NDA accepted. Clearance = 1
onyx> status
[BOOT] ConceptId(1), Clearance: Some(Int(1))
onyx> exit
```

## Design principles

- The brain only speaks `OnyxEvent`.
- The facade only projects.
- Execution is synchronous, ordered, and deterministic.
- Application state that must survive a snapshot lives in the graph.
- Concurrency lives in the adapters.

## License

MIT or Apache-2.0, at your choice.
