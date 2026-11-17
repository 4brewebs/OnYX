# open-onyx (`openonyx`)

**CLI and TUI coding agent harness powered by OnYX (`onyx-soul-agents` + `onyx-workspace`).**

`openonyx` connects an external LLM responder (`GeminiResponder` / `AsyncGeminiDriver` via Google AI Studio REST API) and an interactive terminal UI (`TuiDriver` via `ratatui` + `crossterm`) to OnYX's deterministic `AgentHost` runtime, with full transcript recording, per-tick graph snapshots, inspection, and Git-reconciled time-travel rewind.

## Environment Variables

- `GEMINI_API_KEY` — required for `run` and `chat` modes.
- `ONYX_GEMINI_MODEL` — optional Gemini model name (defaults to `gemini-4-hydrogen`).

## Usage

```text
openonyx chat [--workspace <dir>] [--auto-approve] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file>] [--snapshot-dir <dir>]
openonyx run <goal> [--workspace <dir>] [--auto-approve] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file>] [--snapshot-dir <dir>]
openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>]
openonyx inspect <snapshot_dir> [--tick <n>]
openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--workspace <dir>] [--auto-approve] [--dry-run]
openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]
```

### Examples

Run a single goal and save both the transcript and per-tick graph snapshots:

```sh
cargo run -p open-onyx -- run "summarize the workspace crates" --save-transcript session.tr --snapshot-dir snaps
```

Start the interactive TUI chat:

```sh
cargo run -p open-onyx -- chat --save-transcript chat.tr --snapshot-dir snaps_chat
```

Deterministically replay a saved transcript (no API key required) and write snapshots:

```sh
cargo run -p open-onyx -- replay session.tr --snapshot-dir snaps
```

Inspect the EAV `AgentGraph` at the latest tick or at a specific tick:

```sh
cargo run -p open-onyx -- inspect snaps
cargo run -p open-onyx -- inspect snaps --tick 2
```

Rewind to tick `2` from a snapshot directory (inspecting orphaned side effects and restoring the workspace Git tree checkpoint unless `--dry-run` is set) and resume deterministically from a transcript:

```sh
cargo run -p open-onyx -- rewind snaps 2 --dry-run
cargo run -p open-onyx -- rewind snaps 2 --transcript session.tr
```

## TUI Controls (`chat`)

- **Type + `Enter`**: submit a prompt/goal (handled deterministically by `CommandSpawnerSystem` via `IngressEvent::Command`).
- **`y` / `n`**: approve or deny pending sensitive tools (`fs_write`, `fs_edit`, `shell_exec`) when `AgentState::WaitingApproval` is active (unless `--auto-approve` is set).
- **`Esc` or `Ctrl+C`**: exit the TUI and save the transcript if `--save-transcript` was provided.

## Architecture

- [`main.rs`](src/main.rs): CLI parsing, `CommandSpawnerSystem`, `CliApprovalDriver`, `all_live_agents_blocked` / `all_live_waiting_approval`, `SubagentSystem` (`WaitingChildren` + `spawn_agent`), `WorkspaceLedgerSystem` + `PendingCheckpoints` wiring, and `run` / `chat` / `replay` / `inspect` / `rewind` / `doctor` execution loops:
  - `chat` only advances `host.step()` when at least one live agent can advance (`!is_settled()` and `!all_live_agents_blocked(...)`), when a background `AsyncGeminiDriver` reply is ready (`has_ready`), or when UI events (`Enter` command, `y`/`n` approval) are queued, avoiding idle/approval/LLM-wait tick bloat while keeping the TUI responsive.
  - `--save-transcript` flushes after each settled turn in `chat` as well as on exit.
  - `replay` stops cleanly when `ReplayBridge` is exhausted and all live agents are settled or in `WaitingApproval`.
  - `rewind` queries `effects_after_tick` from the latest snapshot, restores the working tree via `restore_git_tree_checkpoint` to the first pre-effect `tree_sha` (or prints the reconciliation plan with `--dry-run`), and resumes from tick `N`.
  - `doctor` audits Git worktree health, stray `.tr` / `snaps*` artifacts, `ONYXAG` snapshots, `ONYXTR1`/`ONYXTR2` transcripts, and `SideEffectLedger` `tree_sha` objects in Git.
- [`gemini.rs`](src/gemini.rs): `GeminiResponder` (sync `Responder`, for `run`/`rewind`) and `AsyncGeminiDriver` (non-blocking `PlatformDriver` background worker + `has_ready` + `InFlightGuard` RAII counter, wrapped by `RecordingBridge` for `chat`), passing the API key via `x-goog-api-key` header, projecting *as-of* `superseded` file reads and enforcing `ContextBudget` (prioritizing errors, denials, and `@TOON v1` outputs), queueing simultaneous multi-`functionCall` turns sequentially per tick (clearing queued calls automatically on `tool_error`/`denied` so Gemini can replan), and propagating `thoughtSignature` across parallel call batches and thinking turns.
- [`tui.rs`](src/tui.rs): `TuiDriver` + `TuiHandle` (`pump_idle`, `has_queued_events`, multiline entry rendering) implementing `PlatformDriver` with `ratatui` and `crossterm`. Kept in `open-onyx` so `onyx-core` and `onyx-soul-agents` remain dependency-free.

## Roadmap / TODO (Not Yet Implemented)

- **Unified recovery bundle**: bundle graph snapshot, tick `n`, RNG seed, and `Transcript` into a single validated checkpoint artifact for `rewind` and `resume`.
