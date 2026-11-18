# open-onyx (`openonyx`)

**CLI and TUI coding agent harness powered by OnYX (`onyx-soul-agents` + `onyx-workspace`).**

`openonyx` connects an external LLM responder (`GeminiResponder` / `AsyncGeminiDriver` via Google AI Studio REST API) and an interactive terminal UI (`TuiDriver` via `ratatui` + `crossterm`) to OnYX's deterministic `AgentHost` runtime, with multi-turn sessions, project instruction loading (`ONYX.md` / `AGENTS.md`), declared logical surfaces, `Plan`/`Build` mode toggling, transactional Git `/undo`, transcript recording (`ONYXTR1` / `ONYXTR2`), per-tick graph snapshots (`--resume`), and Git-reconciled time-travel `rewind`.

## Environment Variables

- `GEMINI_API_KEY` — required for `run`, `chat`, and live `rewind` (not needed for `replay`, `inspect`, `rewind --dry-run`, `rewind --transcript`, or `doctor`).
- `ONYX_GEMINI_MODEL` — optional Gemini model name (defaults to `gemini-4-hydrogen`).

## Project Instructions

On startup (`run`, `chat`, and live `rewind`), `load_project_instructions` loads markdown rules in order from:

1. `~/.onyx/AGENTS.md` and `~/.onyx/ONYX.md`
2. `<workspace>/AGENTS.md` and `<workspace>/ONYX.md`

Loaded files are filtered through `scrub_secrets`, combined with the compact on-demand skills catalog (`discover_skills_summary`, scanning `.onyx/skills/`, `skills/`, `~/.onyx/skills/`, and built-ins `scent`/`rust`), capped at `16 KiB` total, and appended to Gemini's `systemInstruction`.

## Usage

```text
openonyx chat [--workspace <dir>] [--auto-approve] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file>] [--snapshot-dir <dir>] [--resume <dir>]
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

Start an interactive multi-turn TUI chat session:

```sh
cargo run -p open-onyx -- chat --snapshot-dir snaps_chat --save-transcript chat.tr
```

Resume an existing chat session from the latest `tick-*.bin` snapshot (automatically continues writing new snapshots into the same directory unless `--snapshot-dir` overrides it):

```sh
cargo run -p open-onyx -- chat --resume snaps_chat --save-transcript chat_resumed.tr
```

Deterministically replay a saved transcript (no API key and no disk mutations) and write snapshots:

```sh
cargo run -p open-onyx -- replay chat.tr --snapshot-dir snaps_replay
```

Inspect the EAV `AgentGraph` at the latest tick or at a specific tick:

```sh
cargo run -p open-onyx -- inspect snaps_chat
cargo run -p open-onyx -- inspect snaps_chat --tick 2
```

Rewind to tick `2` from a snapshot directory (inspecting orphaned side effects and restoring the workspace Git tree checkpoint unless `--dry-run` is set) and resume deterministically from a transcript:

```sh
cargo run -p open-onyx -- rewind snaps_chat 2 --dry-run
cargo run -p open-onyx -- rewind snaps_chat 2 --transcript chat.tr
```

Audit Git worktree health, stray artifacts, snapshots, transcript headers, and ledger `tree_sha` objects:

```sh
cargo run -p open-onyx -- doctor --snapshot-dir snaps_chat --transcript chat.tr
```

## TUI Controls & Commands (`chat`)

- **Type + `Enter`**: submit an initial goal or follow-up prompt. Follow-up prompts reactivate the same root agent (`continue_agent`) with its full `rel_memory` preserved instead of spawning an amnesic agent, and reset scroll position to auto-follow the bottom.
- **`Up` / `Down` / `PageUp` / `PageDown` / `Home` / `End` / `Ctrl+U` / `Ctrl+D`**: scroll smoothly through the full wrapped chat & tool-call history (`Memory & Trace` pane), both while idle and while an approval prompt is open (`[SCROLLED -N lines | End to follow]`).
- **`Tab`**: toggle between `[BUILD]` and `[PLAN]` modes by queuing `IngressEvent::Command("/mode plan" | "/mode build")`, persisting `ws.mode` in the EAV graph and recording the switch in `ONYXTR2` transcripts.
- **`y` / `n`**: approve or deny pending mutating tools (`fs_write`, `fs_edit`, `shell_exec`) when `AgentState::WaitingApproval` is active. For `fs_write` / multi-hunk `fs_edit`, the approval box expands and renders a colored unified diff preview (`preview_tool_diff`).
- **Live Plan side panel**: when `plan_update` has recorded steps in `ws.plan.text` and terminal width `>= 100` columns, `TuiDriver` splits the main area (`70%` Memory & Trace / `30%` Live Plan) displaying `[x]`, `[>]`, and `[ ]` items live.
- **`/undo`**: marks the latest reversible `fs_write`/`fs_edit` checkpoint (`ws.effect.undone`) inside `CommandSpawnerSystem`; `WorkspaceToolBridge::present` restores the Git tree checkpoint on disk (`restore_git_tree_checkpoint` + `clear_seen_reads()`), while `ReplayBridge` reproduces the graph state without touching disk.
- **`/mode [plan|build|explore]`**: switch workspace mode explicitly via command.
- **`/new` or `/new <goal>`**: instruct the next prompt to spawn a new root agent (or immediately spawn one with `<goal>`).
- **`Esc` or `Ctrl+C`**: save the latest snapshot (when stepped) and transcript, then exit the TUI cleanly.

## Architecture

`open-onyx` keeps external concurrency, HTTP, terminal I/O, and disk mutation strictly inside `PlatformDriver` adapters, while graph state transitions live in deterministic `OnyxSystem`s:

- [`src/main.rs`](src/main.rs): CLI parsing (`CliArgs`, `Mode`, `parse_args_from`), driver stack composition, system registration (`SubagentSystem`, `WorkspaceLedgerSystem`, `CommandSpawnerSystem`), and delegation to `run_session` / `ops`.
- [`src/command.rs`](src/command.rs): pure `CommandSpawnerSystem` (`OnyxSystem`, zero disk/Git I/O) handling `/new`, `/mode`, `/undo` (`mark_latest_effect_undone`), `on_restore` mode sync, and multi-turn `continue_agent`.
- [`src/session.rs`](src/session.rs): declares logical surfaces (`SURFACE_STATUS`, `SURFACE_TRACE`, `SURFACE_APPROVAL`, `SURFACE_PLAN` via `active_surfaces`), provides `SessionDriver`, `CliApprovalDriver`, `build_host` / `build_host_with_checkpoints`, and the single `run_session` tick loop parameterized by `StopPolicy` (`Settled`, `ReplayExhausted`, `Quit`) using `AgentHost::step_with`.
- [`src/ops.rs`](src/ops.rs): offline snapshot & ledger utilities (`run_inspect`, `reconcile_rewind_effects`, `run_doctor`).
- [`src/gemini.rs`](src/gemini.rs): `GeminiResponder` (sync, for `run`/`rewind`), `AsyncGeminiDriver` (non-blocking background HTTP worker + `InFlightGuard`, wrapped by `RecordingBridge` for `chat`), `load_project_instructions` + on-demand skills catalog, *as-of* read `superseded` projection + `ContextBudget` pruning (preserving errors, denials, and `@TOON v1` outputs), sequential multi-`functionCall` queueing, `"user"`/`"system"` turn mapping, and `thoughtSignature` inheritance.
- [`src/tui.rs`](src/tui.rs): `TuiDriver` + `TuiHandle` (`ratatui` + `crossterm`) rendering header + `[PLAN]`/`[BUILD]` badge, wrapped history navigation (`visual_row_count` + `apply_scroll_key`), optional `70%`/`30%` `Live Plan` side panel (`ws.plan.text`), colored trace entries, and unified diff approval panel. Kept in `open-onyx` so `onyx-core` and `onyx-soul-agents` retain 0 external dependencies.

## Roadmap / TODO (Not Yet Implemented)

- **SSE streaming & OpenAI-compatible provider**: token streaming preview in `TuiDriver` and an optional OpenAI-compatible responder/driver.
- **Unified recovery bundle**: bundle graph snapshot, tick `n`, RNG seed, and `Transcript` into a single validated checkpoint artifact for `rewind` and `resume`.
