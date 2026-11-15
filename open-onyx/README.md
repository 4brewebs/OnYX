# open-onyx (`openonyx`)

**CLI and TUI coding agent harness powered by OnYX (`onyx-soul-agents` + `onyx-workspace`).**

`openonyx` connects an external LLM responder (`GeminiResponder` via Google AI Studio REST API) and an interactive terminal UI (`TuiDriver` via `ratatui` + `crossterm`) to OnYX's deterministic `AgentHost` runtime, with full transcript recording, per-tick graph snapshots, inspection, and time-travel rewind.

## Environment Variables

- `GEMINI_API_KEY` — required for `run` and `chat` modes.
- `ONYX_GEMINI_MODEL` — optional Gemini model name (defaults to `gemini-4-hydrogen`).

## Usage

```text
openonyx chat [--workspace <dir>] [--auto-approve] [--save-transcript <file>] [--snapshot-dir <dir>]
openonyx run <goal> [--workspace <dir>] [--auto-approve] [--save-transcript <file>] [--snapshot-dir <dir>]
openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>]
openonyx inspect <snapshot_dir> [--tick <n>]
openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--workspace <dir>] [--auto-approve]
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

Rewind to tick `2` from a snapshot directory and resume deterministically from a transcript:

```sh
cargo run -p open-onyx -- rewind snaps 2 --transcript session.tr
```

## TUI Controls (`chat`)

- **Type + `Enter`**: submit a prompt/goal (handled deterministically by `CommandSpawnerSystem` via `IngressEvent::Command`).
- **`y` / `n`**: approve or deny pending sensitive tools (`fs_write`, `fs_edit`, `shell_exec`) when `AgentState::WaitingApproval` is active (unless `--auto-approve` is set).
- **`Esc` or `Ctrl+C`**: exit the TUI and save the transcript if `--save-transcript` was provided.

## Architecture

- [`main.rs`](src/main.rs): CLI parsing, `CommandSpawnerSystem`, `CliApprovalDriver`, `all_live_waiting_approval`, and `run` / `chat` / `replay` / `inspect` / `rewind` execution loops:
  - `chat` only advances `host.step()` when agents are active (`!is_settled()` and not blocked on `WaitingApproval`) or when UI events (`Enter` command, `y`/`n` approval) are queued, avoiding idle/approval tick bloat.
  - `--save-transcript` flushes after each settled turn in `chat` as well as on exit.
  - `replay` stops cleanly when `ReplayBridge` is exhausted and all live agents are settled or in `WaitingApproval`.
- [`gemini.rs`](src/gemini.rs): synchronous `GeminiResponder` implementing `Responder`, including `functionCall` / `functionResponse` formatting and `thoughtSignature` preservation across thinking turns.
- [`tui.rs`](src/tui.rs): `TuiDriver` + `TuiHandle` (`pump_idle`, `has_queued_events`, multiline entry rendering) implementing `PlatformDriver` with `ratatui` and `crossterm`. Kept in `open-onyx` so `onyx-core` and `onyx-soul-agents` remain dependency-free.

## Roadmap / TODO (Not Yet Implemented)

- **Async Gemini worker in TUI**: `GeminiResponder::respond` currently blocks synchronously during `present`; moving HTTP calls to a background worker thread will keep the TUI responsive (and allow `Esc` cancellation) while an agent is `Thinking`.
- **Unified recovery bundle**: bundle graph snapshot, tick `n`, RNG seed, and `Transcript` into a single validated checkpoint artifact for `rewind` and `resume`.
- **Configurable `shell_exec` timeout**: expose `--command-timeout` / per-call timeout overrides (currently fixed at `30s` in `WorkspacePolicy`).
- **Subagent delegation tool**: expose a `spawn_agent` tool in `open-onyx` so the main agent can delegate subtasks to child agents via `onyx-soul-agents`'s `parent` links.
