# OnYX

**A Lego of brains, not a closed solution.**

OnYX separates a small, dependency-free core of contracts from the souls that fulfill them.
If your reality is different, write your own soul: the contract is what matters.

**Status: v0, under active design.**
The public contract is not yet closed and will change in breaking ways.
Do not use it where a failure has consequences.

The full project description, including proposed design that is not yet code, is in Spanish in [README_es.md](README_es.md).

```
onyx-core           = contracts + tick + deterministic RNG            · 0 dependencies
onyx-soul           = default implementations of those contracts      · 0 external dependencies
onyx-soul-agents    = deterministic multi-agent harness soul          · 0 external dependencies
onyx-workspace      = sandboxed coding tools + bridge                 · uses onyx-soul-agents
open-onyx           = CLI/TUI coding agent harness (Gemini + ratatui) · uses onyx-soul-agents + onyx-workspace
onyx-example-app    = minimal REPL that exercises onyx-soul           · uses onyx-soul
onyx-example-agents = deterministic multi-agent tour                  · uses onyx-soul-agents
```

## Workspace

The workspace has these five member crates. The minimum Rust version is 1.85, the edition is 2024, and the workspace uses resolver 3.

`onyx-core`, `onyx-soul`, `onyx-soul-agents`, and both example crates declare no external crates. Souls depend on `onyx-core`. Each example depends on its soul, which reexports the core types the app needs, so the app does not declare `onyx-core`. Future souls and apps may add external dependencies. There is no CI workflow in this tree that enforces the rule.

## What exists today

### `onyx-core`

- `Graph`, `GraphView`, and `AttrRegistry` (concepts, attributes, directed relations, deterministic order)
- `OnyxEvent` (`System`, `Ingress`, `Domain`)
- `OnyxSystem` and `Ctx`, with lifecycle hooks (`init`, `on_start`, `on_stop`, `on_restore`)
- Synchronous, ordered `tick` with `DetRng` and `Tick { n }` as a counter, not a wall clock
- Ports: `EventBus`, `SnapshotStore`, `PlatformDriver`, `BlueprintLoader`, `SystemRegistry`
- Capability adapters `ReadOnly` and `AttrOnly` (Rust 1.85 has no trait upcasting)
- Graph conformance cases behind the `conformance` feature, off by default

`tick` does not use `BlueprintLoader` or `SystemRegistry`. Those ports exist so a soul or an app can load stages. There is no `Supervisor` type in core. System states live in a slice the caller passes into `tick`.

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
- `WorkspacePolicy` (canonicalized root sandbox, symlink-safe traversal, ignored dirs, I/O and process-tree timeout limits)
- `WorkspaceTools` (`fs_read`, `fs_list`, `fs_write`, `fs_edit` with line-trimmed fallback, `code_search`, `shell_exec`)
- `WorkspaceMode` (`Plan`, `Build`, `Explore`), `WorkspaceLedgerSystem` (EAV `File` & `ExternalEffect` ledger, `effects_after_tick`), lightweight Git tree checkpoints (`capture_git_tree_checkpoint` / `restore_git_tree_checkpoint`), and `WorkspaceToolBridge`

### `open-onyx` (`openonyx`)
- CLI & TUI coding agent harness (`run`, `chat`, `replay`, `inspect`, `rewind [--dry-run]`)
- `GeminiResponder` (sync, for `run`/`rewind`) and `AsyncGeminiDriver` (non-blocking background HTTP worker + `has_ready` / `InFlightGuard` RAII + sequential multi-`functionCall` batch queueing with `thoughtSignature` inheritance, for `chat`)
- `TuiDriver` (`ratatui` + `crossterm`), per-tick graph snapshots (`--snapshot-dir`), transcript saving, and Git-reconciled time-travel rewind

### `onyx-example-app` & `onyx-example-agents`
- `onyx-example-app`: minimal REPL that loads systems/stages, restores/saves snapshots, and processes `agree`, `status`, `exit`
- `onyx-example-agents`: end-to-end tour of `AgentHost`, `RecordingBridge`, `AgentSnapshotStore`, and `ReplayBridge::starting_at`

## What does NOT exist yet (Roadmap / TODO)

- WASM drivers
- Stage change at runtime
- TOML / config loaders
- Unified recovery bundle (snapshot + tick `n` + RNG seed + `Transcript` in one validated artifact)
- Configurable `shell_exec` timeout and subagent delegation tool in `open-onyx`
- Multi-soul interoperability tests

## Quick start

```bash
cargo run -p onyx-example-app
cargo run -p onyx-example-agents
cargo run -p open-onyx -- --help
```

```text
[CORE] Arranque limpio.
========================================
  OnYX Engine REPL (v0.1.0)
  Type 'agree', 'status', or 'exit'
========================================
onyx> agree
[BOOT] System online. Awaiting 'agree'...
[BOOT] NDA accepted. Clearance = 1
onyx> status
[BOOT] ConceptId(1), Clearance: Some(Int(1))
onyx> exit
Apagado seguro...
[CORE] Guardado en ./snapshots/repl_savegame.snap
Engine stopped after 2 ticks.
```

The first tick both creates the player and handles `agree`, so those two `[BOOT]` lines appear together. The demo reads stdin in `main` and injects commands through `HeadlessDriver`. It does not go through a TUI driver.

```bash
cargo run -p onyx-example-agents
```

The run ends with `fingerprints_match=true` and `ok` when restore, `set_tick`, and replay match the uninterrupted recording.

## Design principles

- The brain only speaks `OnyxEvent`.
- The facade only projects.
- Execution is synchronous, ordered, and deterministic.
- Application state that must survive a snapshot lives in the graph.
- Concurrency lives in the adapters.

## License

MIT or Apache-2.0, at your choice. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
