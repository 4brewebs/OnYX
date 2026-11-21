# open-onyx (`openonyx`)

**CLI and TUI coding agent harness powered by OnYX (`onyx-soul-agents` + `onyx-workspace`).**

`openonyx` connects an external LLM responder (`GeminiResponder` / `AsyncGeminiDriver` via Google AI Studio SSE/REST API, or `OpenAiResponder` / `AsyncOpenAiDriver` via any OpenAI-compatible `/v1/chat/completions` endpoint such as OpenAI, Ollama, vLLM, or OpenRouter with SSE EOF tolerance and transient retry backoff) and an interactive terminal UI (`TuiDriver` via `ratatui` + `crossterm` with live SSE token streaming `[stream] ...▍`, Braille status emblem, and `F2` multi-agent selector) to OnYX's deterministic `AgentHost` runtime, with multi-turn sessions, configurable reasoning `--effort`, project instruction loading (`ONYX.md` / `AGENTS.md`), declared logical surfaces, `Plan`/`Build` mode toggling, OS sandboxing & Sprout local service orchestration, transactional Git `/undo`, transcript recording (`ONYXTR1` / `ONYXTR2`), milestone graph snapshots stored in compressed append-only `snapshots.onyxpack`, and non-destructive time-travel/forking (`/rewind`, `/fork`, `chat --resume --tick`, `rewind --interactive`).

> **Complete User Manual:** see [`docs/manual/index.md`](../docs/manual/index.md) (quickstart in English & Spanish, daily TUI workflow, permissions & OS sandbox, time-travel & forking, 19-tool reference, Sprout services, and troubleshooting).

## Environment Variables & Multi-Provider Selection

- `ONYX_PROVIDER` — optional default provider (`gemini` or `openai`; defaults to `gemini`, overridden by `--provider`).
- `GEMINI_API_KEY` (or `ONYX_API_KEY`) — required for `gemini` provider in `run`, `chat`, and live `rewind` (not needed for `replay`, `inspect`, `rewind --dry-run`, `rewind --transcript`, or `doctor`).
- `ONYX_MODEL` / `ONYX_GEMINI_MODEL` — optional Gemini model name (defaults to `gemini-4-hydrogen`, overridden by `--model`).
- `GEMINI_API_BASE` — optional custom Gemini API base URL (overridden by `--api-base`).
- `OPENAI_API_KEY` (or `ONYX_API_KEY`) — API key for `--provider openai` (optional when using a custom local `--api-base` / `OPENAI_API_BASE` / `OPENAI_BASE_URL` such as Ollama or vLLM).
- `ONYX_MODEL` / `OPENAI_MODEL` — optional OpenAI-compatible model name (defaults to `gpt-4o`, overridden by `--model`).
- `OPENAI_API_BASE` / `OPENAI_BASE_URL` — optional base URL for `--provider openai` (defaults to `https://api.openai.com/v1`, overridden by `--api-base`).
- `ONYX_EFFORT` — optional reasoning/thinking effort (`low`, `medium`, or `high`; overridden by `--effort`, with provider-specific fallbacks `GEMINI_THINKING_LEVEL` for Gemini and `OPENAI_REASONING_EFFORT` for OpenAI).

## Project Instructions

On startup (`run`, `chat`, and live `rewind`), `load_project_instructions` loads markdown rules in order from:

1. `~/.onyx/AGENTS.md` and `~/.onyx/ONYX.md`
2. `<workspace>/AGENTS.md` and `<workspace>/ONYX.md`

Loaded files are filtered through `scrub_secrets`, combined with the host OS shell hint (Windows PowerShell vs Unix/POSIX) and the compact on-demand skills catalog (`discover_skills_summary`, scanning `.onyx/skills/`, `skills/`, `~/.onyx/skills/`, and built-ins `scent`/`rust`), capped at `16 KiB` total, and appended to the system instruction.

## Usage

```text
openonyx chat [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>] [--resume <dir> [--tick <n>] [--fork-dir <dir>] [--truncate-future]]
openonyx run <goal> [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]
openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]
openonyx inspect <snapshot_dir> [--tick <n>] [--list] [--json]
openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--interactive | --tui] [--fork-dir <dir>] [--truncate-future] [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--dry-run]
openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]
openonyx [--help | -h | --version | -V]
```

### Examples

Run a single goal and save both the transcript (`--save-transcript` or `--record`) and milestone graph snapshots (`snapshots.onyxpack`):

```sh
cargo run -p open-onyx -- run "summarize the workspace crates" --record session.tr --snapshot-dir snaps
```

Start an interactive multi-turn TUI chat session (streaming SSE token deltas live with Gemini or any OpenAI-compatible local/cloud model):

```sh
cargo run -p open-onyx -- chat --effort high --snapshot-dir snaps_chat --record chat.tr
cargo run -p open-onyx -- chat --provider openai --model gpt-4o --effort medium --snapshot-dir snaps_openai
cargo run -p open-onyx -- chat --provider openai --api-base http://localhost:11434/v1 --model qwen2.5-coder:14b --effort low
```

Resume an existing chat session from the latest milestone snapshot in `snapshots.onyxpack` (with automatic fallback to legacy `tick-*.bin` files), or time-travel/fork directly from a historical tick:

```sh
# Continue from latest milestone
cargo run -p open-onyx -- chat --resume snaps_chat --record chat_resumed.tr

# Safe non-destructive fork from tick 12 into a sibling directory (snaps_chat.fork-t12)
cargo run -p open-onyx -- chat --resume snaps_chat --tick 12

# Same via rewind --interactive with an explicit fork directory
cargo run -p open-onyx -- rewind snaps_chat 12 --interactive --fork-dir snaps_alt
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

Preview or run a headless rewind to tick `4` (automatically resolving the nearest prior milestone tick `<= N`), auditing orphaned side effects and restoring the workspace Git tree checkpoint unless `--dry-run` is set:

```sh
cargo run -p open-onyx -- rewind snaps_chat 4 --dry-run
cargo run -p open-onyx -- rewind snaps_chat 4 --transcript chat.tr
```

Audit Git worktree health, stray artifacts, `snapshots.onyxpack` integrity (including FNV-1a checksums, `ONYXAG` payloads, and crash-truncated tail detection), legacy `tick-*.bin` snapshots, transcript headers, ledger `tree_sha` objects, OS sandbox capabilities, and `sprout.toml`:

```sh
cargo run -p open-onyx -- doctor --snapshot-dir snaps_chat --transcript chat.tr
```

## Milestone Snapshots, Forking & `snapshots.onyxpack`

When `--snapshot-dir <dir>` is enabled, `MilestoneSnapshotSystem` records snapshots only at meaningful graph transitions (tagged as `init`, `spawn`, `approval:<tool>`, `tool:<tool>`, `tool_error:<tool>`, `cmd:/agent`, `cmd:/undo`, `cmd:/rewind`, `cmd:/fork`, `cmd:/compact`, `cmd:/mode ...`, `cmd:/new`, `settled`, `failed`, `exit`, plus optional `--snapshot-every <n>` `periodic` frames) instead of dumping full uncompressed `tick-*.bin` files on idle or internal micro-ticks.

`PackSnapshotStore` writes canonical `ONYXAG\x01\0` graph payloads into an append-only compressed container `<dir>/snapshots.onyxpack` (`ONYXPK1` frames with zlib compression, raw/stored lengths, and 64-bit FNV-1a checksums). Interrupted writes that leave a truncated tail frame are ignored safely on read and healed automatically on the next append, legacy `tick-*.bin` directories remain readable via transparent fallback, and `--max-snapshots <n>` is available as an opt-in retention bound (disabled by default so full session history stays rewindable). `fork_pack_until_tick` and `SharedPackStore` allow non-destructive branching into sibling directories both from the CLI (`--resume --tick`, `--fork-dir`) and live inside the TUI (`/fork`, `/rewind`), leaving the original pack untouched unless `--truncate-future` is explicitly requested.

## TUI Controls & Commands (`chat`)

- **Type + `Enter`**: submit an initial goal or follow-up prompt. Follow-up prompts reactivate the same root agent (`continue_agent`) with its full `rel_memory` preserved instead of spawning an amnesic agent, and reset scroll position to auto-follow the bottom. Duplicate root prompts are guarded while the root agent is actively `thinking`, `acting`, `waiting_children`, or `waiting_approval`.
- **`Up` / `Down` / `PageUp` / `PageDown` / `Home` / `End` / `Ctrl+U` / `Ctrl+D`**: scroll smoothly through the full wrapped chat & tool-call history (`Memory & Trace` pane), both while idle and while an approval prompt is open (`[SCROLLED -N lines | End to follow]`).
- **`Tab`**: toggle between `[BUILD]` and `[PLAN]` modes by queuing `IngressEvent::Command("/mode plan" | "/mode build")`, persisting `ws.mode` in the EAV graph and recording the switch in `ONYXTR2` transcripts.
- **`F2` / `Ctrl+Tab` / `/agent [id|name]`**: open the interactive **Agent Selector** popup (or jump directly by ID/name) to inspect and focus the root agent or any delegated `spawn_agent` child (subagents default to a `32`-step cap unless overridden).
- **`y` / `n`**: approve or deny pending mutating tools (`fs_write`, `fs_edit`, `shell_exec`, `service_*`) when `AgentState::WaitingApproval` is active. The banner identifies `[agent #id name]`, and for `fs_write` / multi-hunk `fs_edit` expands to render a colored unified diff preview (`preview_tool_diff`).
- **Compact 3-row header, Braille emblem & side panels**: shows an animated Braille spinner (`⠋..⠏`, 90 ms throttle), `[PLAN]`/`[BUILD]`, `[SANDBOX:workspace|strict|off]`, active model + `--effort`, and live telemetry (`ctx: <1%`/`XX% · in / cache / out / think · ~$cost` or `local`) without mutating deterministic ticks. When `plan_update` (`ws.plan.text`) and/or `sprout.toml` (`ws.services.text`) are present and terminal width `>= 100` columns, `TuiDriver` splits the main area (`70%` Memory & Trace / `30%` right pane, vertically split `50%`/`50%` when both are active).
- **`/services` and `/service <start|stop|logs> <id|all> [tail]`**: inspect and control local processes & Docker/Rancher containers declared in `sprout.toml` directly from the TUI (validated in `openonyx doctor`).
- **`/undo`**: marks the latest reversible `fs_write`/`fs_edit` checkpoint (`ws.effect.undone`) inside `CommandSpawnerSystem`; `WorkspaceToolBridge::present` restores the Git tree checkpoint on disk (`restore_git_tree_checkpoint_for_targets` + `clear_seen_reads()`), also cleaning up newly created untracked `fs_write` files without deleting unrelated user files, while `ReplayBridge` reproduces the graph state without touching disk.
- **`/rewind [tick]`**: lists available milestones when called without arguments, or rewinds live in-session to `<tick>` (or nearest prior milestone `<= tick`) when idle, forking snapshots into a sibling directory, reconciling orphaned effects/Git tree on disk, and resuming seamlessly in the TUI.
- **`/fork [name]`**: forks snapshots up to the current tick into a sibling directory (`<dir>.fork-<name>` or `<dir>.fork-t<tick>`) and switches the live `SharedPackStore` to the new branch without mutating the source pack.
- **`/compact [focus]`**: deterministically compacts prior turns into a verified `[session_summary]` boundary (`cmd:/compact` milestone in `snapshots.onyxpack`), rescuing `files_found` and keyword-matched `focus_matches` from earlier turns while dropping active prompt fill immediately and keeping pre-compact ticks recoverable via `rewind`.
- **`/mode [plan|build|explore]`**: switch workspace mode explicitly via command.
- **`/new` or `/new <goal>`**: instruct the next prompt to spawn a new root agent (or immediately spawn one with `<goal>`, auto-focusing the new root in the TUI).
- **Declarative permissions & OS Sandbox (`.onyx/config.toml`)**: `~/.onyx/config.toml` and `<workspace>/.onyx/config.toml` support `[permissions]` (`deny`, `allow`, `ask`, precedence `deny > ask > allow`) and `[sandbox]` (`mode = "workspace-write" | "strict-container" | "off"`, `network = "offline" | "localhost" | "unrestricted"`, `env_passthrough`, `max_memory_mb`, `max_pids`). `shell_exec` runs with `env_clear()`, Control Plane RO protection (`.onyx/config.toml`, `.git/hooks/**`, `.git/config`, `.env*`), and RAII `ProcessJob` process-tree cleanup; `--sandbox` / `--sandbox-net` override config, `--auto-approve` elevates `off` to `workspace-write` + `offline`, and `openonyx doctor` reports the detected host driver. Pass `--ignore-deny` only when you explicitly want to bypass `deny` rules.
- **Search v2 & `glob_files`**: `code_search` supports `regex`, `case_insensitive`, `include_glob`, `exclude_glob`, and `context_lines` (`0..=5`); `glob_files` locates workspace files deterministically without requiring approval.
- **`Esc` or `Ctrl+C`**: close an open popup or save the latest snapshot (`exit`) and transcript, then exit the TUI cleanly.

## Architecture

`open-onyx` keeps external concurrency, HTTP, terminal I/O, and disk mutation strictly inside `PlatformDriver` adapters, while graph state transitions live in deterministic `OnyxSystem`s:

- [`src/main.rs`](src/main.rs): CLI parsing (`CliArgs`, `Mode`, `parse_args_from`), `--effort` / resume-fork resolution (`prepare_chat_resume`, `launch_interactive_chat`), driver stack composition, system registration (`SubagentSystem`, `WorkspaceLedgerSystem`, `CommandSpawnerSystem`, `MilestoneSnapshotSystem`), and delegation to `run_session` / `ops`.
- [`src/context.rs`](src/context.rs): shared `TieredContextConfig` / `ToolOutputTier` projection (Tier 0 full recent outputs, deterministic coordinate-preserving Tier 1 summaries for older outputs, Tier 2 `/compact` and auto-compaction boundaries, prompt-cache-stable prefixes), `ReasoningEffort`, and `TokenUsage` / `SessionTelemetryHandle` extraction for Gemini and OpenAI.
- [`src/command.rs`](src/command.rs): pure `CommandSpawnerSystem` (`OnyxSystem`, zero disk/Git I/O) handling `SlashCommand` (`/new`, `/mode`, `/agent`, `/undo`, `/rewind`, `/fork`, `/compact`, `/services`, `/service`), `PendingSessionAction` / `PendingSystemNotes`, `on_restore` mode sync, and multi-turn `continue_agent` with active-root protection.
- [`src/pack.rs`](src/pack.rs): `PackSnapshotStore`, `SharedPackStore` (live branch switching), `fork_pack_until_tick`, `truncate_pack_until_tick`, `default_fork_dir`, `verify_pack_integrity` (zlib-compressed `snapshots.onyxpack` with FNV-1a verification, truncated-tail crash tolerance, opt-in `--max-snapshots` compaction, and legacy `tick-*.bin` fallback), and `MilestoneSnapshotSystem`.
- [`src/session.rs`](src/session.rs): declares logical surfaces (`SURFACE_STATUS`, `SURFACE_TRACE`, `SURFACE_APPROVAL`, `SURFACE_PLAN`, `SURFACE_SERVICES` via `active_surfaces`), provides `SessionDriver`, `CliApprovalDriver`, `apply_session_action` (live `/rewind` and `/fork` orchestration), `build_host` / `build_host_with_shared_store`, and the single `run_session` tick loop parameterized by `StopPolicy` (`Settled`, `ReplayExhausted`, `Quit`) using `AgentHost::step_with`.
- [`src/ops.rs`](src/ops.rs): offline snapshot & ledger utilities (`render_inspect` / `run_inspect` with `--list` and `--json`, `reconcile_rewind_effects` & quiet `reconcile_rewind_effects_report` for live TUI rewind, and `run_doctor`).
- [`src/gemini.rs`](src/gemini.rs): `GeminiResponder` (sync), `AsyncGeminiDriver` (non-blocking `:streamGenerateContent?alt=sse` HTTP worker + `StreamPreviewHandle` + `InFlightGuard`), `load_project_instructions` + OS shell hint + on-demand skills catalog, `thinkingLevel` (`--effort`), *as-of* read `superseded` projection + `ContextBudget` pruning, sequential multi-`functionCall` queueing, `"user"`/`"system"` turn mapping, and `thoughtSignature` inheritance.
- [`src/openai.rs`](src/openai.rs): OpenAI-compatible `/v1/chat/completions` adapter (`OpenAiResponder` sync and `AsyncOpenAiDriver` SSE streaming with `StreamPreviewHandle`, `reasoning_effort`, completed-SSE EOF tolerance, and transient HTTP/stream retry backoff), supporting OpenAI, OpenRouter, Ollama, vLLM, and LM Studio while reusing `ContextBudget`, `superseded` pruning, and tool call queuing.
- [`src/tui.rs`](src/tui.rs): `TuiDriver` + `TuiHandle` (`ratatui` + `crossterm`) rendering the compact 3-row header (Braille emblem, `[PLAN]`/`[BUILD]`, `[SANDBOX:...]`, model/`--effort`, live telemetry), live SSE token preview (`[stream] ...▍` in `pump_idle`), wrapped history navigation (`visual_row_count` + `apply_scroll_key`), `F2` interactive Agent Selector popup (`render_agent_picker`), optional `70%`/`30%` `Live Plan` + `Services (Sprout)` side panels, colored trace entries, and `[agent #id]` unified diff approval panel. Kept in `open-onyx` so `onyx-core` and `onyx-soul-agents` retain 0 external dependencies.

## Roadmap / TODO (Not Yet Implemented)

- **Input ergonomics & palette (SDD-03 Block 2/3)**: interactive `/` command & `@file` autocomplete popup, command history, queued follow-up prompts, and `F1..F5` / `Ctrl+K` modular TUI views.
- **Unified recovery bundle**: bundle graph snapshot, tick `n`, RNG seed, and `Transcript` into a single validated checkpoint artifact for `rewind` and `resume`.
