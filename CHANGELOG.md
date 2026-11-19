# Changelog

All notable changes to the **OnYX** workspace crates (`onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-scent`, `onyx-workspace`, and `open-onyx`) are documented in this file.

## [0.1.1] - 2026-11-19

Patch release updating **`onyx-workspace` (`0.1.1`)** and **`open-onyx` (`0.1.1`)** with SDD-01 (tiered context, prompt caching, `/compact`, live telemetry) and SDD-02 (`code_search` v2, `glob_files`, declarative `.onyx/config.toml` permissions). Unchanged crates (`onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-scent`) remain at **`0.1.0`** with zero external dependencies preserved in the core/soul layer.

### Added (`open-onyx` `0.1.1` — SDD-01: Tiered Context, Prompt Caching & Live Telemetry)

- **Shared tiered context engine (`open-onyx/src/context.rs`)**: unified `TieredContextConfig` and `ToolOutputTier` (`Tier0Full`, `Tier1Summary`, `Tier2Compacted`) reused by both Gemini and OpenAI backends. Keeps the last 10 tool observations in full (Tier 0), deterministically summarizes older tool outputs preserving coordinates (`fs_read`, `scent_outline`, `scent_impact`, `code_search`, `glob_files`, `shell_exec`, Tier 1) with cache-stable prefixes, and supports automatic Tier 2 compaction (`auto_compact_ratio`, default `80%`).
- **`/compact [focus]` & `cmd:/compact` milestone**: deterministic conversation compaction in `CommandSpawnerSystem` (`SlashCommand::Compact`) with `files_found` and keyword-scored `focus_matches` rescue from earlier turns, automatic `cmd:/compact` milestone tagging in `MilestoneSnapshotSystem` (`snapshots.onyxpack`), and 100% backward compatibility with `ONYXTR2` transcripts and `ReplayBridge`.
- **Real-time token & cost telemetry**: extracts exact provider counters from Gemini (`usageMetadata` REST + SSE) and OpenAI (`stream_options: { include_usage: true }` + `usage` + `prompt_tokens_details.cached_tokens`), displaying `ctx: <1%` / `XX% · in / cache / out / think · ~$cost` on the TUI header border (and `[openOnyx telemetry]` in headless `run`) without polluting the deterministic EAV graph.

### Added (`onyx-workspace` `0.1.1` & `open-onyx` `0.1.1` — SDD-02: Search v2, `glob_files` & Declarative Permissions)

- **`code_search` v2**: adds optional `regex`, `case_insensitive`, `include_glob`, `exclude_glob`, and `context_lines` (`0..=5` grouped hunks with `--` separators and `:` vs `-` markers) while remaining 100% backward compatible and scrubbing secrets.
- **`glob_files` tool**: read-only workspace file discovery with glob patterns (`*`, `**`, `?`, `[...]`, `{a,b}`), deterministic lexicographical ordering (or optional `sort_by_mtime`), ignored-directory filtering, and strict path-traversal rejection.
- **Granular permissions in `.onyx/config.toml`**: `PermissionRules` / `PermissionDecision` loaded from `~/.onyx/config.toml` and `<workspace>/.onyx/config.toml` (`[permissions]` with `deny`, `allow`, `ask`, precedence `deny > ask > allow`). Global `deny` rules block even under `--auto-approve` (unless explicit `--ignore-deny` is passed), and `WorkspaceMode::Plan` / `Explore` unconditionally block mutations regardless of `allow` rules.

### Added (`onyx-workspace` & `open-onyx` — SDD-06: OS Sandboxing, `env_clear`, Control Plane RO & `ProcessJob`)

- **Shared OS sandbox & RAII `ProcessJob` (`onyx-workspace/src/sandbox.rs`)**: `SandboxMode` (`WorkspaceWrite` default, `StrictContainer`, `Off`), `NetworkPolicy` (`Offline` default, `LocalhostOnly`, `Unrestricted`), `[sandbox]` section loader in `.onyx/config.toml`, strict `env_clear()` environment sanitizer stripping `*_API_KEY`/`*_TOKEN`/`AWS_*`, and cross-platform RAII `ProcessJob` (Win32 Job Objects with `KILL_ON_JOB_CLOSE`, memory/PID limits, and POSIX `PGID` cleanup) ready for reuse by `onyx-sprout` (SDD-08).
- **Control Plane protection & OS drivers**: read-only protection of `.onyx/config.toml`, `.git/hooks/**`, `.git/config`, and `.env*` across `fs_write`, `fs_edit`, and `shell_exec`; native wrappers for Linux `bwrap`, macOS `sandbox-exec`, Windows Job Objects, and OCI `docker`/`nerdctl`/`podman`.
- **CLI, TUI & `doctor` integration**: added `--sandbox` and `--sandbox-net` flags, automatic elevation to `workspace-write` + `offline` under `--auto-approve`, `[SANDBOX:workspace|strict|off]` badge in the TUI header, and host sandbox driver reporting in `openonyx doctor`.

### Added (`onyx-workspace` & `open-onyx` — SDD-08: Local Process & Container Orchestrator `onyx-sprout`)

- **Unified `sprout.toml` & DAG supervisor (`onyx-workspace/src/sprout.rs`)**: pure-`std` `SproutSupervisor` (0 `tokio`, MSRV 1.85) supporting both modern `[runtime]`/`[services.<id>]` (`kind = "process" | "container"`, `up`/`logs`/`down`/`container`) and legacy `[global]`/`[[service]]` (`env_wrapper`, `debug_port`, `dir`, `depends_on`), with Kahn topological DAG validation, real probes (`ready_port`, `ready_log`, `ready_cmd`), 2,000-line UTF-8-lossy log ring-buffers, `env_clear()`, and automatic `ProcessJob` teardown on `Drop`.
- **5 agent tools + deterministic replay (`ONYXTR2`)**: added `service_status`, `service_logs` (read-only), `service_start`, `service_stop`, and `service_exec` (`docker exec` / `nerdctl exec` with `env_clear()`, timeout, `scrub_secrets`, `PermissionRules`, and `Plan`/`Explore` mutation blocking). Recorded sessions replay deterministically via `ReplayBridge` with zero subprocesses spawned.
- **EAV projection, TUI panel, slash commands & `doctor`**: added `ws.services.text` in `WorkspaceLedgerSystem`, colorized `Services (Sprout)` right-hand panel in `TuiDriver`, `/services` and `/service <start|stop|logs>` commands in `CommandSpawnerSystem`, and `sprout.toml` DAG/runtime/occupied-port auditing in `openonyx doctor`.

## [0.1.0] - 2026-11-19

Initial public release of the deterministic OnYX agent microkernel, reference souls, AST distiller, sandboxed workspace tools, and the `openonyx` CLI/TUI harness.

### Crates

- **`onyx-core` (`0.1.0`)**: Zero-dependency deterministic microkernel contracts (`Graph`, `GraphView`, `AttrRegistry`, `EventBus`, `OnyxSystem`, `PlatformDriver`, `SnapshotStore`, `DetRng`, ticks, and conformance suite).
- **`onyx-soul` (`0.1.0`)**: Zero-dependency reference soul implementing the core ports with an in-memory EAV graph, priority bus, supervisor, and snapshot store.
- **`onyx-soul-agents` (`0.1.0`)**: Zero-dependency multi-agent soul with `AgentHost`, `RecordingBridge` / `ReplayBridge` (`ONYXTR2` transcripts), `ToolSystem`, `SubagentSystem` (`spawn_agent`), `continue_agent`, `append_agent_memory`, and `HeadlessDriver`.
- **`onyx-scent` (`0.1.0`)**: Deterministic Tree-sitter AST indexer (`Rust`, `Python`, `Java`), integer-only thermal impact diffusion (`0..=10_000` bp), and `@TOON v1` compact serializer (`scent` binary behind the `cli` feature).
- **`onyx-workspace` (`0.1.0`)**: Deterministic workspace tools and EAV ledger (`fs_read` with deduplication/invalidation, `fs_list`, `fs_write`, multi-hunk atomic `fs_edit` with relative re-indentation, `code_search`, `git_status`, `git_diff`, `load_skill`, `plan_update`, `shell_exec` with credential scrubbing and timeouts, and optional `scent_outline` / `scent_impact`). Includes `[PLAN]`/`[BUILD]` mode enforcement and transactional git tree checkpoints for `/undo` and `rewind`.
- **`open-onyx` (`0.1.0`, bin `openonyx`)**:
  - Interactive `chat` TUI (`ratatui` + `crossterm`) with SSE token streaming (`[stream] ...▍`), live diff approval (`y`/`n`), `Tab` Plan/Build toggle, `/undo`, `/mode`, `/new`, full `Memory & Trace` scroll synchronization, and side `Live Plan` panel (`>= 100` cols).
  - Multi-provider LLM support (`--provider gemini|openai`, `--model`, `--api-base`) for Gemini and OpenAI-compatible / local endpoints (Ollama, vLLM, LM Studio) with context budgeting and `ONYX.md` / `AGENTS.md` / `.onyx/skills/*/SKILL.md` support.
  - Compressed append-only milestone snapshot container (`snapshots.onyxpack`, `ONYXPK1` zlib frames + FNV-1a checksums) with `MilestoneSnapshotSystem`, non-contiguous `inspect` (`--list`, `--json`) and `rewind` (`--dry-run`), pure offline `replay`, and `doctor` diagnostics.
  - Complete `--help` (`-h`, `help`) and `--version` (`-V`) CLI output (`exit code 0`).
