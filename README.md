# OnYX

**A Lego of brains, not a closed solution.**

OnYX separates a small, dependency-free core of contracts from the souls that fulfill them.
If your reality is different, write your own soul: the contract is what matters.

**Status: v0, under active design.**
The public contract is not yet closed and will change in breaking ways.
Do not use it where a failure has consequences.

The full project description, including proposed design that is not yet code, is in Spanish in [README_es.md](README_es.md). Contributor and agent rules are in [CONTRIBUTING.md](CONTRIBUTING.md).

```
onyx-core           = contracts + tick + deterministic RNG            · 0 dependencies
onyx-soul           = default implementations of those contracts      · 0 external dependencies
onyx-soul-agents    = deterministic multi-agent harness soul          · 0 external dependencies
onyx-scent          = AST distiller + blake3 cache + @TOON v1         · optional cli feature
onyx-workspace      = sandboxed coding tools + bridge                 · uses onyx-soul-agents (+ optional onyx-scent)
open-onyx           = CLI/TUI coding agent harness (Gemini + ratatui) · uses onyx-soul-agents + onyx-workspace
onyx-example-app    = minimal REPL that exercises onyx-soul           · uses onyx-soul
onyx-example-agents = deterministic multi-agent tour                  · uses onyx-soul-agents
```

## Workspace

The workspace has eight member crates (`onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-scent`, `onyx-workspace`, `open-onyx`, `onyx-example-app`, and `onyx-example-agents`). The minimum Rust version is `1.85`, the edition is `2024`, and the workspace uses resolver `3`.

`onyx-core`, `onyx-soul`, `onyx-soul-agents`, and both example crates declare zero external crates. Souls depend on `onyx-core`. Each example depends on its soul, which reexports the core types the app needs, so the app does not declare `onyx-core`. Adapter crates (`onyx-scent`, `onyx-workspace`, `open-onyx`) add external dependencies while preserving pure deterministic core/soul semantics. [`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs `fmt`, `check`, `clippy`, `test`, `test --release`, and `doc` checks; run `cargo tree -p onyx-core -p onyx-soul -p onyx-soul-agents` to verify the zero-dependency rule.

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

**`onyx-core` audit (18 November 2026).** `tick` and `DetRng` match the written contract. This pass does not change traits or phase order. The crate suite is 31 tick tests, 6 `DetRng` tests, 7 graph-port tests, 3 doctests, and 8 `conformance` probes. The new edge cases live in [`onyx-core/tests/tick_contract.rs`](onyx-core/tests/tick_contract.rs) and [`onyx-core/tests/detrng_contract.rs`](onyx-core/tests/detrng_contract.rs). The Spanish contract note is in [README_es.md](README_es.md).

Four behaviors are protocol hazards, not implementation bugs: a same-tick outbox is not quarantined across `RestoreRequested` (domain events keep pre-import `ConceptId`s and payloads, with `SnapshotRestored` between them); the outbox is a shared `Vec`, so a later system can drop an earlier `SnapshotRequested`; ingress is discarded when every system is `Stopped`, and writing `SystemState::Active` by hand skips `on_start`; a panic inside `run` keeps earlier graph writes, drops the already-drained inbox and the not-yet-queued outbox, and skips control and `present`. Unknown `Start`/`Stop` names emit no `Fault`. `Shutdown` still has no runtime policy. Duplicate system names all receive `Start`. `poll` gets the full `EventBus`, and `run` / `on_start` / `on_stop` get the full `Graph`. A failed re-init after a successful import still emits `SnapshotRestored`. `DetRng` matches its fixed vectors; 4096 consecutive ticks for seeds 0, 1, and 91 do not repeat the first output. Full 64-bit collision freedom is not promised.

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
- Multi-turn memory lifecycle: `append_agent_memory` (flushing post-`Done` `result` into `"say"` chronologically) and `continue_agent` (reactivating terminal agents with monotonic `steps`)
- `AgentSpawner`, `AgentRuntime`, `SubagentSystem` (`spawn_agent` / `AgentState::WaitingChildren`), `ToolSystem` / `ToolExecutor`
- `RecordingBridge` / `ReplayBridge` (`ONYXTR1` / `ONYXTR2` transcripts), `HeadlessDriver`
- `AgentState::WaitingApproval`, `ApprovalPolicy`, structured tool calls (`Reply::call_structured`)

### `onyx-scent`
- Deterministic multi-language (`Rust`, `Python`, `Java`) `tree-sitter` AST indexer with `blake3` incremental cache
- Integer-only (`0..=10_000` bp) Max-Heat impact diffusion (`distill_heat`), `scope:src|test` & `vis:pub|priv` tagging, `exclude_tests` filtering, trait method visibility inheritance, struct field summaries, Java annotations, intra-file call resolution, and `@TOON v1` paginated encoder/decoder (`outline_toon`, `impact_toon`)
- Pure library by default (`clap` gated behind `feature = "cli"`)

### `onyx-workspace`
- `WorkspacePolicy` (canonicalized root sandbox, symlink-safe traversal, ignored dirs, I/O and process-tree timeout limits)
- `WorkspaceTools` (`fs_read` with `recall_seen`, `fs_list`, `fs_write`, multi-hunk atomic `fs_edit` with line-trimmed fallback & relative re-indentation, `code_search`, approval-free `git_status` & `git_diff`, `load_skill` for on-demand `SKILL.md` / built-in guides, `plan_update`, `shell_exec` with secret scrubbing and cache invalidation, plus read-only `scent_outline` & `scent_impact` under `feature = "scent"` with `max_rows` clamped `1..=500` and `public_only`)
- `preview_tool_diff` (pure single/multi-hunk unified diff preview for `fs_write`/`fs_edit`)
- `WorkspaceMode` (`Plan`, `Build`, `Explore`), `WorkspaceModeHandle`, graph-backed `ws.mode` & `ws.plan.text`, `WorkspaceLedgerSystem` (EAV `File` & `ExternalEffect` ledger, `effects_after_tick`, `latest_undoable_effect`, `mark_latest_effect_undone`, `undone_effects`), Git tree checkpoints (`capture_git_tree_checkpoint` supporting unborn `HEAD`, `restore_git_tree_checkpoint` keeping `.git/index` clean), and `WorkspaceToolBridge` (inline `[checkpoint:<head>:<tree>]` tagging for deterministic `ReplayBridge` playback and live `/undo` materialization)

### `open-onyx` (`openonyx`)
- Modular CLI & TUI coding agent harness:
  - [`main.rs`](open-onyx/src/main.rs) (`run`, `chat [--resume <dir>]`, `replay`, `inspect [--tick <n>] [--list] [--json]`, `rewind [--dry-run]`, `doctor`, `--max-steps <n>`, `--shell-timeout <secs>`, `--record` / `--save-transcript`, `--snapshot-every <n>`, `--max-snapshots <n>`)
  - [`command.rs`](open-onyx/src/command.rs) (pure deterministic `CommandSpawnerSystem` handling `/new`, `/mode`, `/undo`, `on_restore` mode sync, and multi-turn `continue_agent`)
  - [`pack.rs`](open-onyx/src/pack.rs) (`PackSnapshotStore` append-only zlib-compressed `snapshots.onyxpack` with 64-bit FNV-1a checksums, truncated-tail recovery, opt-in `--max-snapshots`, and legacy `tick-*.bin` fallback, plus deterministic `MilestoneSnapshotSystem` recording only meaningful state transitions)
  - [`session.rs`](open-onyx/src/session.rs) (declared logical surfaces `SURFACE_STATUS`/`SURFACE_TRACE`/`SURFACE_APPROVAL`/`SURFACE_PLAN` passed via `AgentHost::step_with`, `SessionDriver`, unified `run_session` + `StopPolicy`)
  - [`ops.rs`](open-onyx/src/ops.rs) (`inspect [--list] [--json]`, non-contiguous `rewind` floor reconciliation, and `doctor` validation of `snapshots.onyxpack` & legacy `.bin` snapshots)
- [`gemini.rs`](open-onyx/src/gemini.rs): `GeminiResponder` (sync), `AsyncGeminiDriver` (non-blocking background HTTP worker + `InFlightGuard`), `~/.onyx/{AGENTS,ONYX}.md` & `<workspace>/{AGENTS,ONYX}.md` instruction loading + compact skill catalog (`discover_skills_summary`), *as-of* `supersede` & `ContextBudget`, sequential multi-`functionCall` queueing with `thoughtSignature` propagation, `"user"`/`"system"` turn mapping
- [`tui.rs`](open-onyx/src/tui.rs): `TuiDriver` (`ratatui` + `crossterm`), full wrapped chat history scrolling (`Up`/`Down`/`PageUp`/`PageDown`/`Home`/`End`/`Ctrl+U`/`Ctrl+D`), optional `70%`/`30%` `Live Plan` split panel (`ws.plan.text` when width `>= 100`), `Tab` mode toggle (`[PLAN]`/`[BUILD]` emitted as recorded commands), and colored unified diff preview box on sensitive tool approvals (`y`/`n`)

### `onyx-example-app` & `onyx-example-agents`
- `onyx-example-app`: minimal REPL that loads systems/stages, restores/saves snapshots, and processes `agree`, `status`, `exit`
- `onyx-example-agents`: end-to-end tour of `AgentHost`, `RecordingBridge`, `AgentSnapshotStore`, and `ReplayBridge::starting_at`

## What does NOT exist yet (Roadmap / TODO)

- WASM drivers
- Stage change at runtime
- TOML / config loaders
- Unified recovery bundle (snapshot + tick `n` + RNG seed + `Transcript` in one validated artifact)
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
