# open-onyx (`openonyx`)

**CLI, TUI, and embedded Web PWA coding agent harness powered by OnYX (`onyx-soul-agents` + `onyx-workspace`).**

`openonyx` connects an external LLM responder (`GeminiResponder` / `AsyncGeminiDriver` via Google AI Studio SSE/REST API, or `OpenAiResponder` / `AsyncOpenAiDriver` via any OpenAI-compatible `/v1/chat/completions` endpoint such as OpenAI, Ollama, vLLM, or OpenRouter with SSE EOF tolerance and transient retry backoff) and either an interactive terminal UI (`TuiDriver` via `ratatui` + `crossterm` with live SSE token streaming `[stream] ...▍`, Braille status emblem, and `F2` multi-agent selector) or an embedded mobile-first Web UI (`WebDriver` via `openonyx serve`, pure `std::net` HTTP/1.1 + SSE + Obsidian/Champagne-Gold PWA with zero `tokio`) to OnYX's deterministic `AgentHost` runtime, with multi-turn sessions, configurable reasoning `--effort`, `blake3` Workspace Trust (`openonyx trust` / `/trust`), project instruction loading (`ONYX.md` / `AGENTS.md`), declared logical surfaces, `Plan`/`Build`/`Explore` mode toggling, OS sandboxing, async subagents in isolated Git worktrees with Scent-Heated 3-way merge approval (`spawn_subagent`, `/subagents`, `/merge`, `/discard`) and external CLI harness drivers (`/spawn`, `[[external_drivers]]`), Sprout local service orchestration, transactional Git `/undo`, transcript recording (`ONYXTR1` / `ONYXTR2`), milestone graph snapshots stored in compressed append-only `snapshots.onyxpack`, and non-destructive time-travel/forking (`/rewind`, `/fork`, `chat --resume --tick`, `rewind --interactive`).

> **Complete User Manual:** see [`docs/manual/index.md`](../docs/manual/index.md) (quickstart in English & Spanish, daily TUI workflow, permissions & OS sandbox, Workspace Trust, `openonyx serve` Web UI, time-travel & forking, 19-tool reference, Sprout services, and troubleshooting).

## Environment Variables & Multi-Provider Selection

- `ONYX_PROVIDER` — optional default provider (`gemini` or `openai`; defaults to `gemini`, overridden by `--provider`).
- `GEMINI_API_KEY` (or `ONYX_API_KEY`) — required for `gemini` provider in `run`, `chat`, `serve`, and live `rewind` (not needed for `replay`, `inspect`, `rewind --dry-run`, `rewind --transcript`, `trust`, or `doctor`).
- `ONYX_MODEL` / `ONYX_GEMINI_MODEL` — optional Gemini model name (defaults to `gemini-4-hydrogen`, overridden by `--model`).
- `GEMINI_API_BASE` — optional custom Gemini API base URL (overridden by `--api-base`; remote hosts require `https://` and `--allow-custom-auth-host` when sending API keys to non-official hosts).
- `OPENAI_API_KEY` (or `ONYX_API_KEY`) — API key for `--provider openai` (optional when using a custom local `--api-base` / `OPENAI_API_BASE` / `OPENAI_BASE_URL` such as Ollama or vLLM).
- `ONYX_MODEL` / `OPENAI_MODEL` — optional OpenAI-compatible model name (defaults to `gpt-4o`, overridden by `--model`).
- `OPENAI_API_BASE` / `OPENAI_BASE_URL` — optional base URL for `--provider openai` (defaults to `https://api.openai.com/v1`, overridden by `--api-base`).
- `ONYX_ALLOW_CUSTOM_AUTH_HOST` — set to `1`/`true` (or pass `--allow-custom-auth-host`) to explicitly authorize sending LLM API keys to a non-official HTTPS `--api-base` host.
- `ONYX_EFFORT` — optional reasoning/thinking effort (`low`, `medium`, or `high`; overridden by `--effort`, with provider-specific fallbacks `GEMINI_THINKING_LEVEL` for Gemini and `OPENAI_REASONING_EFFORT` for OpenAI).
- `ONYX_AUTO_SAVE` — enable `--auto-save` by default (`1`, `true`, `yes`, or `on`), automatically storing snapshots and transcripts together under `<workspace>/.onyx/vault/<session>/`.
- `ONYX_TRUST_FILE` — optional override path for the `blake3` Workspace Trust store (defaults to `~/.onyx/trusted_workspaces.toml`).
- `ONYX_WEB_BIND` — default bind address for `openonyx serve` (defaults to `127.0.0.1:8787`, overridden by `--bind`).
- `ONYX_WEB_API_KEY` — explicit API key for `openonyx serve` (required with `>= 16` characters whenever `--bind` is non-loopback; ephemeral 256-bit OS-CSPRNG key generated automatically on loopback).
- `ONYX_WEB_CORS_ORIGIN` — optional single allowlisted origin (`http://localhost:5173` or `https://...`, never `*`) for an external frontend (e.g. a Svelte SPA) talking to `openonyx serve`.

## Project Instructions

On startup (`run`, `chat`, `serve`, and live `rewind`), `load_project_instructions` loads markdown rules in order from:

1. `~/.onyx/AGENTS.md` and `~/.onyx/ONYX.md`
2. `<workspace>/AGENTS.md` and `<workspace>/ONYX.md`

Loaded files are filtered through `scrub_secrets`, combined with the host OS shell hint (Windows PowerShell vs Unix/POSIX) and the compact on-demand skills catalog (`discover_skills_summary`, scanning `.onyx/skills/`, `skills/`, `~/.onyx/skills/`, and built-ins `scent`/`rust`), capped at `16 KiB` total, and appended to the system instruction.

## Usage

```text
openonyx chat [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--allow-custom-auth-host] [--trust-workspace] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--max-steps <n>] [--max-subagents <n>] [--shell-timeout <secs>] [--auto-save] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>] [--resume <dir> [--tick <n>] [--fork-dir <dir>] [--truncate-future]]
openonyx serve [--bind <host:port>] [--api-key <key>] [--cors-origin <origin>] [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--allow-custom-auth-host] [--trust-workspace] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--max-steps <n>] [--max-subagents <n>] [--shell-timeout <secs>] [--auto-save] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>] [--resume <dir> [--tick <n>] [--fork-dir <dir>] [--truncate-future]]
openonyx run <goal> [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--allow-custom-auth-host] [--trust-workspace] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--max-steps <n>] [--max-subagents <n>] [--shell-timeout <secs>] [--auto-save] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]
openonyx trust [add|list|check|revoke] [--workspace <dir>]
openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]
openonyx inspect <snapshot_dir> [--tick <n>] [--list] [--json]
openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--interactive | --tui] [--fork-dir <dir>] [--truncate-future] [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--allow-custom-auth-host] [--trust-workspace] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--dry-run]
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

Audit Git worktree health, Workspace Trust state, stray artifacts, `snapshots.onyxpack` integrity (including FNV-1a checksums, `ONYXAG` payloads, and crash-truncated tail detection), legacy `tick-*.bin` snapshots, transcript headers, ledger `tree_sha` objects, OS sandbox capabilities, `sprout.toml`, and external harness drivers (`[[external_drivers]]` binary resolution, isolated `--version` probe, and network-policy severity):

```sh
cargo run -p open-onyx -- doctor --snapshot-dir snaps_chat --transcript chat.tr
```

Manage `blake3` Workspace Trust for repositories with `.onyx/config.toml` or `sprout.toml`:

```sh
cargo run -p open-onyx -- trust check --workspace .
cargo run -p open-onyx -- trust add --workspace .
cargo run -p open-onyx -- trust list
```

Serve the embedded mobile-first Web UI / PWA over HTTP/1.1 + SSE (loopback generates an ephemeral 256-bit API key; non-loopback requires an explicit `--api-key` `>= 16` chars):

```sh
# Local browser preview (prints URL + ephemeral API key)
cargo run -p open-onyx -- serve --workspace . --auto-save

# Remote/proxy bind or external Svelte SPA via explicit CORS origin
cargo run -p open-onyx -- serve --bind 0.0.0.0:8787 --api-key "your-long-secret-key" --cors-origin http://localhost:5173 --auto-save
```

## Session Vault (`.onyx/vault/`) & `--auto-save`

To keep workspace roots clean, `openonyx` centralizes session data inside `<workspace>/.onyx/vault/` (automatically creating `.onyx/vault/.gitignore` with `*` so Git status and `/undo` checkpoints never index vault files, while `onyx-workspace` ignores `.onyx/vault/` in searches and protects it as read-only Control Plane):

- **`--auto-save`** (or `ONYX_AUTO_SAVE=1`): automatically creates `<workspace>/.onyx/vault/<YYYYMMDD-HHMMSS>_<model>[_<branch|goal>]/` storing both `snapshots.onyxpack` and `<session>.tr` together, displays `vault:<session>` in the TUI header, and prints the exact `--resume` command on exit.
- **Short names vs explicit paths**: passing a short name such as `--snapshot-dir feat-auth --record chat.tr` stores both files under `.onyx/vault/feat-auth/`; passing `--record solo.tr` without `--snapshot-dir` stores it under `.onyx/vault/unassigned/solo.tr`. Explicit paths containing `/` or `\` (or absolute paths) are preserved verbatim.
- **Transparent vault lookup**: `--resume`, `inspect`, `rewind`, `replay`, and `doctor` check literal paths first and fall back to `.onyx/vault/` automatically for short names.

## Milestone Snapshots, Forking & `snapshots.onyxpack`

When `--snapshot-dir <dir>` is enabled, `MilestoneSnapshotSystem` records snapshots only at meaningful graph transitions (tagged as `init`, `spawn`, `approval:<tool>`, `tool:<tool>`, `tool_error:<tool>`, `cmd:/agent`, `cmd:/undo`, `cmd:/rewind`, `cmd:/fork`, `cmd:/compact`, `cmd:/mode ...`, `cmd:/new`, `settled`, `failed`, `exit`, plus optional `--snapshot-every <n>` `periodic` frames) instead of dumping full uncompressed `tick-*.bin` files on idle or internal micro-ticks.

`PackSnapshotStore` writes canonical `ONYXAG\x01\0` graph payloads into an append-only compressed container `<dir>/snapshots.onyxpack` (`ONYXPK1` frames with zlib compression, raw/stored lengths, and 64-bit FNV-1a checksums). Interrupted writes that leave a truncated tail frame are ignored safely on read and healed automatically on the next append, legacy `tick-*.bin` directories remain readable via transparent fallback, and `--max-snapshots <n>` is available as an opt-in retention bound (disabled by default so full session history stays rewindable). `fork_pack_until_tick` and `SharedPackStore` allow non-destructive branching into sibling directories both from the CLI (`--resume --tick`, `--fork-dir`) and live inside the TUI (`/fork`, `/rewind`), leaving the original pack untouched unless `--truncate-future` is explicitly requested.

## TUI Controls & Commands (`chat`)

- **Type + `Enter`**: submit an initial goal or follow-up prompt. Follow-up prompts reactivate the same root agent (`continue_agent`) with its full `rel_memory` preserved, and reset scroll position to auto-follow the bottom. Prompts submitted while the root agent is busy are automatically queued (`queued:N`, clearable with `Ctrl+X`) and dispatched sequentially once settled.
- **Cursor editing, history & `/` / `@` autocomplete**: edit mid-line with `Left`/`Right`/`Home`/`End`/`Delete`, browse previous prompts with `Alt+Up`/`Alt+Down` (`Ctrl+P`/`Ctrl+N`, preserving unsent drafts), and type `/` (slash commands) or `@` (cached workspace files) to open the floating autocomplete popup (`Up`/`Down`, `Tab`/`Enter` to accept, `Esc` to dismiss).
- **`Up` / `Down` / `PageUp` / `PageDown` / `Home` / `End` / `Ctrl+U` / `Ctrl+D`**: scroll smoothly through the full wrapped chat & tool-call history (`Memory & Trace` pane), both while idle and while an approval prompt is open (`[SCROLLED -N lines | End to follow]`).
- **`Tab`**: toggle between `[BUILD]` and `[PLAN]` modes by queuing `IngressEvent::Command("/mode plan" | "/mode build")`, persisting `ws.mode` in the EAV graph and recording the switch in `ONYXTR2` transcripts.
- **`F2` / `Ctrl+Tab` / `/agent [id|name]`**: open the interactive **Agent Selector** popup (or jump directly by ID/name) to inspect and focus the root agent or any delegated `spawn_agent` child (subagents default to a `32`-step cap unless overridden).
- **`y` / `n`**: approve or deny pending mutating tools (`fs_write`, `fs_edit`, `shell_exec`, `service_*`) when `AgentState::WaitingApproval` is active. The banner identifies `[agent #id name]`, and for `fs_write` / multi-hunk `fs_edit` expands to render a colored unified diff preview (`preview_tool_diff`).
- **Compact 3-row header, Braille emblem & side panels**: shows an animated Braille spinner (`⠋..⠏`, 90 ms throttle), `[PLAN]`/`[BUILD]`, `[SANDBOX:workspace|env+job|strict|off]`, `[UNTRUSTED]` when project configs are not yet trusted, active model + `--effort`, and live telemetry (`ctx: <1%`/`XX% · in / cache / out / think · ~$cost` or `local`) without mutating deterministic ticks. When `plan_update` (`ws.plan.text`) and/or `sprout.toml` (`ws.services.text`) are present and terminal width `>= 100` columns, `TuiDriver` splits the main area (`70%` Memory & Trace / `30%` right pane, vertically split `50%`/`50%` when both are active).
- **`/trust [status|revoke]`**: inspect, approve, or revoke `blake3` Workspace Trust for `<workspace>/.onyx/config.toml` and `sprout.toml` live inside the session (hot-unblocking Sprout).
- **`/services` and `/service <start|stop|logs> <id|all> [tail]`**: inspect and control local processes & Docker/Rancher containers declared in `sprout.toml` directly from the TUI (validated in `openonyx doctor`).
- **`/undo`**: marks the latest reversible `fs_write`/`fs_edit` checkpoint (`ws.effect.undone`) inside `CommandSpawnerSystem`; `WorkspaceToolBridge::present` restores the Git tree checkpoint on disk (`restore_git_tree_checkpoint_for_targets` + `clear_seen_reads()`), also cleaning up newly created untracked `fs_write` files without deleting unrelated user files, while `ReplayBridge` reproduces the graph state without touching disk.
- **`/rewind [tick]`**: lists available milestones when called without arguments, or rewinds live in-session to `<tick>` (or nearest prior milestone `<= tick`) when idle, forking snapshots into a sibling directory, reconciling orphaned effects/Git tree on disk, and resuming seamlessly in the TUI.
- **`/fork [name]`**: forks snapshots up to the current tick into a sibling directory (`<dir>.fork-<name>` or `<dir>.fork-t<tick>`) and switches the live `SharedPackStore` to the new branch without mutating the source pack.
- **`/compact [focus]`**: deterministically compacts prior turns into a verified `[session_summary]` boundary (`cmd:/compact` milestone in `snapshots.onyxpack`), rescuing `files_found` and keyword-matched `focus_matches` from earlier turns while dropping active prompt fill immediately and keeping pre-compact ticks recoverable via `rewind`.
- **`/mode [plan|build|explore]`**: switch workspace mode explicitly via command.
- **`/new` or `/new <goal>`**: instruct the next prompt to spawn a new root agent (or immediately spawn one with `<goal>`, auto-focusing the new root in the TUI).
- **Async subagents in isolated Git worktrees (`spawn_subagent`)**: the model can delegate work to a background subagent that runs in its own worktree (checkpoint-based, `HEAD` and the staging index untouched); track jobs live with **`/subagents`** (cards show state, branch, and a `→ <driver>` badge when the backend is not `native`), approve the single 3-way merge with **`/merge <job_id>`** (`preview_three_way_merge` + `# impact:` Scent heat banner, TOCTOU-protected tickets, Control Plane `Deny` / governance `Ask`), or roll back with **`/discard <job_id>`**. Concurrent jobs are capped by `--max-subagents <n>` / `ONYX_MAX_SUBAGENTS` (default `3`), and the sealed `SubagentProposal` makes `replay` reproduce every run with 0 subprocesses.
- **`/spawn [--driver <id>] <task>`**: dispatch a background worktree job directly from the TUI **without spending a root turn** (works even before any root agent exists); case-insensitive command and `--driver` flag, tolerant of multiple spaces. Omit the driver (or use `native`) for the in-process engine, or pick a `[[external_drivers]]` CLI harness (e.g. `opencode`) which runs inside the same worktree with a sanitized environment, prompt over `stdin`, `ProcessJob` process-tree cleanup, and disk-as-truth reconciliation (`capture_subagent_commit` + `changed_files_between`).
- **External harness drivers (`.onyx/config.toml` `[[external_drivers]]`)**: declare additional CLI harnesses (`id`, `bin`, `args`, `prompt_transport = "stdin" | "arg" | "temp_file"`, `prompt_flag`, `timeout_secs`, `allowed_env`, `inherit_home`, `network_allowlist`); the built-in `opencode` preset is overridable by `id` (merge precedence `preset < global < workspace`). Workspace-local drivers only apply in **trusted** workspaces, `root` needs explicit approval (or an `allow spawn_subagent(<driver>)` rule) to delegate to a non-native driver, and the Web composer exposes a driver selector (also trust-gated) with the send button switching to `Spawn`. `openonyx doctor` audits every driver with origin-based severity: `[WARN]` missing built-in preset binary, `[ERR]` missing declared binary, `[CHECK]` responsive isolated `--version` probe (3 s deadline, sanitized offline environment), `[WARN]` advisory vs `[CHECK]`-enforced network allowlist under `strict-container`, and `[WARN]` for `prompt_transport = "arg"` on Windows.
- **Declarative permissions & OS Sandbox (`.onyx/config.toml`)**: `~/.onyx/config.toml` and `<workspace>/.onyx/config.toml` support `[permissions]` (`deny`, `allow`, `ask`, precedence `deny > ask > allow`) and `[sandbox]` (`mode = "workspace-write" | "strict-container" | "off"`, `network = "offline" | "localhost" | "unrestricted"`, `env_passthrough`, `max_memory_mb`, `max_pids`). `shell_exec` runs with `env_clear()`, Control Plane RO protection (`.onyx/config.toml`, `.git/hooks/**`, `.git/config`, `.env*`), and RAII `ProcessJob` process-tree cleanup; `--sandbox` / `--sandbox-net` override config, `--auto-approve` elevates `off` to `workspace-write` + `offline`, and `openonyx doctor` reports the detected host driver. Pass `--ignore-deny` only when you explicitly want to bypass `deny` rules.
- **Search v2 & `glob_files`**: `code_search` supports `regex`, `case_insensitive`, `include_glob`, `exclude_glob`, and `context_lines` (`0..=5`); `glob_files` locates workspace files deterministically without requiring approval.
- **`Esc` or `Ctrl+C`**: close an open popup or save the latest snapshot (`exit`) and transcript, then exit the TUI cleanly.

## Architecture

`open-onyx` keeps external concurrency, HTTP, terminal I/O, and disk mutation strictly inside `PlatformDriver` adapters, while graph state transitions live in deterministic `OnyxSystem`s:

- [`src/main.rs`](src/main.rs): CLI parsing (`CliArgs`, `Mode`, `parse_args_from`), `--effort` / resume-fork resolution (`prepare_chat_resume`, `launch_interactive_chat`), driver stack composition, system registration (`SubagentSystem`, `WorkspaceLedgerSystem`, `CommandSpawnerSystem`, `MilestoneSnapshotSystem`), and delegation to `run_session` / `ops`.
- [`src/context.rs`](src/context.rs): shared `TieredContextConfig` / `ToolOutputTier` projection (Tier 0 full recent outputs, deterministic coordinate-preserving Tier 1 summaries for older outputs, Tier 2 `/compact` and auto-compaction boundaries, prompt-cache-stable prefixes), `ReasoningEffort`, and `TokenUsage` / `SessionTelemetryHandle` extraction for Gemini and OpenAI.
- [`src/command.rs`](src/command.rs): pure `CommandSpawnerSystem` (`OnyxSystem`, zero disk/Git I/O) handling `SlashCommand` (`/new`, `/mode`, `/agent`, `/trust`, `/undo`, `/rewind`, `/fork`, `/compact`, `/services`, `/service`, `/subagents`, `/spawn`, `/merge`, `/discard`), `PendingSessionAction` / `PendingSystemNotes`, `on_restore` mode sync, and multi-turn `continue_agent` with active-root protection.
- [`src/pack.rs`](src/pack.rs): `PackSnapshotStore`, `SharedPackStore` (live branch switching), `fork_pack_until_tick`, `truncate_pack_until_tick`, `default_fork_dir`, `verify_pack_integrity` (zlib-compressed `snapshots.onyxpack` with FNV-1a verification, truncated-tail crash tolerance, opt-in `--max-snapshots` compaction, and legacy `tick-*.bin` fallback), and `MilestoneSnapshotSystem`.
- [`src/vault.rs`](src/vault.rs): centralized `.onyx/vault/` session storage resolver (`resolve_write_storage`, `resolve_read_snapshot_dir`, `resolve_read_transcript`, `generate_auto_session_id`, `ensure_vault_gitignore`, `audit_vault`) backing `--auto-save` / `ONYX_AUTO_SAVE`; also hosts the sealed external-driver raw logs (`.onyx/vault/subagent-logs/<job_id>.log`).
- [`src/subagent.rs`](src/subagent.rs): `SubagentManager` driver for async background subagents in isolated Git worktrees (shared concurrency cap `DEFAULT_MAX_CONCURRENT_SUBAGENTS = 3`, deterministic tick-boundary barrier, `ProcessJob` RAII), `WorkerBackend::{Native, External}` branching, sealed `SubagentProposal` → `ONYXTR2` (`SUBAGENT_PROPOSAL_CMD_PREFIX`, `SubagentProposalWire` backward-compatible `driver` field), 3-way merge preview (`preview_three_way_merge` + `effective_merge_decision` governance escalation), and the `spawn_subagent` / `subagent_status` / `subagent_merge` / `subagent_discard` tool handlers.
- [`src/external_driver.rs`](src/external_driver.rs): `ExternalHarnessDriver` trait + runner for `[[external_drivers]]` CLI harnesses (PATH/`PATHEXT` binary resolution via `clean_process_path`, safe `stdin` / `temp_file` prompt transport with `Arg` rejected for `.cmd`/`.bat`, two bounded concurrent pipe-reader threads, `apply_sanitized_env` without `NODE_OPTIONS`, `configure_process_isolation` + `ProcessJob` + `kill_process_tree` tree teardown).
- [`src/session.rs`](src/session.rs): declares logical surfaces (`SURFACE_STATUS`, `SURFACE_TRACE`, `SURFACE_APPROVAL`, `SURFACE_PLAN`, `SURFACE_SERVICES` via `active_surfaces`), provides `SessionDriver`, `CliApprovalDriver`, `apply_session_action` (live `/rewind` and `/fork` orchestration), `build_host` / `build_host_with_shared_store`, and the single `run_session` tick loop parameterized by `StopPolicy` (`Settled`, `ReplayExhausted`, `Quit`) using `AgentHost::step_with`.
- [`src/ops.rs`](src/ops.rs): offline snapshot & ledger utilities (`render_inspect` / `run_inspect` with `--list` and `--json`, `reconcile_rewind_effects` & quiet `reconcile_rewind_effects_report` for live TUI rewind, and `run_doctor` — including the `[[external_drivers]]` audit with origin-based severity and the isolated `--version` probe).
- [`src/gemini.rs`](src/gemini.rs): `GeminiResponder` (sync), `AsyncGeminiDriver` (non-blocking `:streamGenerateContent?alt=sse` HTTP worker + `StreamPreviewHandle` + `InFlightGuard`), `load_project_instructions` + OS shell hint + on-demand skills catalog, `thinkingLevel` (`--effort`), *as-of* read `superseded` projection + `ContextBudget` pruning, sequential multi-`functionCall` queueing, `"user"`/`"system"` turn mapping, and `thoughtSignature` inheritance.
- [`src/openai.rs`](src/openai.rs): OpenAI-compatible `/v1/chat/completions` adapter (`OpenAiResponder` sync and `AsyncOpenAiDriver` SSE streaming with `StreamPreviewHandle`, `reasoning_effort`, completed-SSE EOF tolerance, and transient HTTP/stream retry backoff), supporting OpenAI, OpenRouter, Ollama, vLLM, and LM Studio while reusing `ContextBudget`, `superseded` pruning, and tool call queuing.
- [`src/tui.rs`](src/tui.rs): `TuiDriver` + `TuiHandle` (`ratatui` + `crossterm`) rendering the compact 3-row header (Braille emblem, `[PLAN]`/`[BUILD]`, `[SANDBOX:workspace|env+job|strict|off]`, `[UNTRUSTED]`, model/`--effort`, live telemetry), live SSE token preview (`[stream] ...▍` in `pump_idle`), wrapped history navigation (`visual_row_count` + `apply_scroll_key`), `F2` interactive Agent Selector popup (`render_agent_picker`), optional `70%`/`30%` `Live Plan` + `Services (Sprout)` side panels, colored trace entries, and `[agent #id]` unified diff approval panel (with `# impact:` Scent heat banner). Kept in `open-onyx` so `onyx-core` and `onyx-soul-agents` retain 0 external dependencies.
- [`src/web.rs`](src/web.rs) & [`web/`](web/): `WebDriver` + `WebHandle` implementing `UiIdlePump` over pure `std::net::TcpListener` + `std::thread` + `Condvar` + `blake3` (0 `tokio`), serving `/api/v1/{health,auth/login,auth/logout,state,events,command,approval,focus,queue/clear,files}` with OS CSPRNG sessions, `HttpOnly`/`__Host-` cookies + `Bearer` auth, `Sec-Fetch-Site` CSRF guard, strict CSP (`require-trusted-types-for 'script'`), explicit `--cors-origin`, and the embedded Obsidian/Champagne-Gold PWA (`index.html`, `styles.css`, `app.js`, `logo.jpg`, `icon.svg`, `manifest.webmanifest`) with zero `innerHTML`, plus the composer driver selector for `[[external_drivers]]` (trust-gated, `Send`/`Spawn` button state).

## Roadmap / TODO (Not Yet Implemented)

- **Modular TUI views (`F1..F5` / `Ctrl+K`)**: dedicated multi-panel command palette and filterable trace views.
- **Unified recovery bundle**: bundle graph snapshot, tick `n`, RNG seed, and `Transcript` into a single validated checkpoint artifact for `rewind` and `resume`.
