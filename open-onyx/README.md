# open-onyx (`openonyx`)

**CLI and TUI coding agent harness powered by OnYX (`onyx-soul-agents` + `onyx-workspace`).**

`openonyx` connects an external LLM responder (`GeminiResponder` / `AsyncGeminiDriver` via Google AI Studio SSE/REST API, or `OpenAiResponder` / `AsyncOpenAiDriver` via any OpenAI-compatible `/v1/chat/completions` endpoint such as OpenAI, Ollama, vLLM, or OpenRouter) and an interactive terminal UI (`TuiDriver` via `ratatui` + `crossterm` with live SSE token streaming `[stream] ...▍`) to OnYX's deterministic `AgentHost` runtime, with multi-turn sessions, project instruction loading (`ONYX.md` / `AGENTS.md`), declared logical surfaces, `Plan`/`Build` mode toggling, transactional Git `/undo`, transcript recording (`ONYXTR1` / `ONYXTR2`), milestone graph snapshots stored in compressed append-only `snapshots.onyxpack` (`--resume`), and Git-reconciled time-travel `rewind`.

## Environment Variables & Multi-Provider Selection

- `ONYX_PROVIDER` — optional default provider (`gemini` or `openai`; defaults to `gemini`, overridden by `--provider`).
- `GEMINI_API_KEY` (or `ONYX_API_KEY`) — required for `gemini` provider in `run`, `chat`, and live `rewind` (not needed for `replay`, `inspect`, `rewind --dry-run`, `rewind --transcript`, or `doctor`).
- `ONYX_MODEL` / `ONYX_GEMINI_MODEL` — optional Gemini model name (defaults to `gemini-4-hydrogen`, overridden by `--model`).
- `GEMINI_API_BASE` — optional custom Gemini API base URL (overridden by `--api-base`).
- `OPENAI_API_KEY` (or `ONYX_API_KEY`) — API key for `--provider openai` (optional when using a custom local `--api-base` / `OPENAI_API_BASE` / `OPENAI_BASE_URL` such as Ollama or vLLM).
- `ONYX_MODEL` / `OPENAI_MODEL` — optional OpenAI-compatible model name (defaults to `gpt-4o`, overridden by `--model`).
- `OPENAI_API_BASE` / `OPENAI_BASE_URL` — optional base URL for `--provider openai` (defaults to `https://api.openai.com/v1`, overridden by `--api-base`).

## Project Instructions

On startup (`run`, `chat`, and live `rewind`), `load_project_instructions` loads markdown rules in order from:

1. `~/.onyx/AGENTS.md` and `~/.onyx/ONYX.md`
2. `<workspace>/AGENTS.md` and `<workspace>/ONYX.md`

Loaded files are filtered through `scrub_secrets`, combined with the compact on-demand skills catalog (`discover_skills_summary`, scanning `.onyx/skills/`, `skills/`, `~/.onyx/skills/`, and built-ins `scent`/`rust`), capped at `16 KiB` total, and appended to Gemini's `systemInstruction`.

## Usage

```text
openonyx chat [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--api-base <url>] [--auto-approve] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>] [--resume <dir>]
openonyx run <goal> [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--api-base <url>] [--auto-approve] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]
openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]
openonyx inspect <snapshot_dir> [--tick <n>] [--list] [--json]
openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--api-base <url>] [--auto-approve] [--dry-run]
openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]
```

### Examples

Run a single goal and save both the transcript (`--save-transcript` or `--record`) and milestone graph snapshots (`snapshots.onyxpack`):

```sh
cargo run -p open-onyx -- run "summarize the workspace crates" --record session.tr --snapshot-dir snaps
```

Start an interactive multi-turn TUI chat session (streaming SSE token deltas live with Gemini or any OpenAI-compatible local/cloud model):

```sh
cargo run -p open-onyx -- chat --snapshot-dir snaps_chat --record chat.tr
cargo run -p open-onyx -- chat --provider openai --model gpt-4o --snapshot-dir snaps_openai
cargo run -p open-onyx -- chat --provider openai --api-base http://localhost:11434/v1 --model qwen2.5-coder:14b
```

Resume an existing chat session from the latest milestone snapshot in `snapshots.onyxpack` (with automatic fallback to legacy `tick-*.bin` files; continues writing new milestones into the same directory unless `--snapshot-dir` overrides it):

```sh
cargo run -p open-onyx -- chat --resume snaps_chat --record chat_resumed.tr
```

Deterministically replay a saved transcript (no API key and no disk mutations) and write milestone snapshots:

```sh
cargo run -p open-onyx -- replay chat.tr --snapshot-dir snaps_replay
```

List recorded milestones (tick, milestone tag, compressed/raw size, FNV-1a checksum, Git tree checkpoint) or inspect any milestone graph as human-readable text or JSON:

```sh
cargo run -p open-onyx -- inspect snaps_chat --list
cargo run -p open-onyx -- inspect snaps_chat --list --json
cargo run -p open-onyx -- inspect snaps_chat
cargo run -p open-onyx -- inspect snaps_chat --tick 4 --json
```

Rewind to tick `4` (or the nearest prior milestone tick `<= N` automatically) from a snapshot directory, inspecting orphaned side effects and restoring the workspace Git tree checkpoint unless `--dry-run` is set, and resume deterministically from a transcript:

```sh
cargo run -p open-onyx -- rewind snaps_chat 4 --dry-run
cargo run -p open-onyx -- rewind snaps_chat 4 --transcript chat.tr
```

Audit Git worktree health, stray artifacts, `snapshots.onyxpack` integrity (including FNV-1a checksums, `ONYXAG` payloads, and crash-truncated tail detection), legacy `tick-*.bin` snapshots, transcript headers, and ledger `tree_sha` objects:

```sh
cargo run -p open-onyx -- doctor --snapshot-dir snaps_chat --transcript chat.tr
```

## Milestone Snapshots & `snapshots.onyxpack`

When `--snapshot-dir <dir>` is enabled, `MilestoneSnapshotSystem` records snapshots only at meaningful graph transitions (tagged as `init`, `spawn`, `approval:<tool>`, `tool:<tool>`, `tool_error:<tool>`, `cmd:/undo`, `cmd:/mode ...`, `cmd:/new`, `settled`, `failed`, `exit`, plus optional `--snapshot-every <n>` `periodic` frames) instead of dumping full uncompressed `tick-*.bin` files on idle or internal micro-ticks.

`PackSnapshotStore` writes canonical `ONYXAG\x01\0` graph payloads into an append-only compressed container `<dir>/snapshots.onyxpack` (`ONYXPK1` frames with zlib compression, raw/stored lengths, and 64-bit FNV-1a checksums). Interrupted writes that leave a truncated tail frame are ignored safely on read and healed automatically on the next append, legacy `tick-*.bin` directories remain readable via transparent fallback, and `--max-snapshots <n>` is available as an opt-in retention bound (disabled by default so full session history stays rewindable).

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

- [`src/main.rs`](src/main.rs): CLI parsing (`CliArgs`, `Mode`, `parse_args_from`), driver stack composition, system registration (`SubagentSystem`, `WorkspaceLedgerSystem`, `CommandSpawnerSystem`, `MilestoneSnapshotSystem`), and delegation to `run_session` / `ops`.
- [`src/command.rs`](src/command.rs): pure `CommandSpawnerSystem` (`OnyxSystem`, zero disk/Git I/O) handling `/new`, `/mode`, `/undo` (`mark_latest_effect_undone`), `on_restore` mode sync, and multi-turn `continue_agent`.
- [`src/pack.rs`](src/pack.rs): `PackSnapshotStore` (`SnapshotStore` implementation over append-only zlib-compressed `snapshots.onyxpack` with FNV-1a verification, truncated-tail crash tolerance, opt-in `--max-snapshots` compaction, and legacy `tick-*.bin` fallback) and `MilestoneSnapshotSystem` (deterministic `OnyxSystem` requesting snapshots on meaningful transitions and `--snapshot-every`).
- [`src/session.rs`](src/session.rs): declares logical surfaces (`SURFACE_STATUS`, `SURFACE_TRACE`, `SURFACE_APPROVAL`, `SURFACE_PLAN` via `active_surfaces`), provides `SessionDriver`, `CliApprovalDriver`, `build_host` / `build_host_with_options`, and the single `run_session` tick loop parameterized by `StopPolicy` (`Settled`, `ReplayExhausted`, `Quit`) using `AgentHost::step_with`.
- [`src/ops.rs`](src/ops.rs): offline snapshot & ledger utilities (`render_inspect` / `run_inspect` with `--list` and `--json`, `reconcile_rewind_effects`, `run_doctor`).
- [`src/gemini.rs`](src/gemini.rs): `GeminiResponder` (sync), `AsyncGeminiDriver` (non-blocking `:streamGenerateContent?alt=sse` HTTP worker + `StreamPreviewHandle` + `InFlightGuard`), `load_project_instructions` + on-demand skills catalog, *as-of* read `superseded` projection + `ContextBudget` pruning (preserving errors, denials, and `@TOON v1` outputs), sequential multi-`functionCall` queueing, `"user"`/`"system"` turn mapping, and `thoughtSignature` inheritance.
- [`src/openai.rs`](src/openai.rs): OpenAI-compatible `/v1/chat/completions` adapter (`OpenAiResponder` sync and `AsyncOpenAiDriver` SSE streaming with `StreamPreviewHandle`), supporting OpenAI, OpenRouter, Ollama, vLLM, and LM Studio while reusing `ContextBudget`, `superseded` pruning, and tool call queuing.
- [`src/tui.rs`](src/tui.rs): `TuiDriver` + `TuiHandle` (`ratatui` + `crossterm`) rendering header + `[PLAN]`/`[BUILD]` badge, live SSE token preview (`[stream] ...▍` in `pump_idle` without advancing ticks), wrapped history navigation (`visual_row_count` + `apply_scroll_key`), optional `70%`/`30%` `Live Plan` side panel (`ws.plan.text`), colored trace entries, and unified diff approval panel. Kept in `open-onyx` so `onyx-core` and `onyx-soul-agents` retain 0 external dependencies.

## Roadmap / TODO (Not Yet Implemented)

- **Unified recovery bundle**: bundle graph snapshot, tick `n`, RNG seed, and `Transcript` into a single validated checkpoint artifact for `rewind` and `resume`.
