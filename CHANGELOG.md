# Changelog

All notable changes to the **OnYX** workspace crates (`onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-scent`, `onyx-workspace`, and `open-onyx`) are documented in this file.

## [Unreleased]

### Added (`open-onyx` — `openonyx serve`: Embedded Mobile-First PWA & Deterministic HTTP/1.1 + SSE API)

- **Zero-`tokio` Web UI & API server (`open-onyx/src/web.rs`, `open-onyx/web/`)**: added `openonyx serve [--bind <ip:port>] [--api-key <key>] [--cors-origin <origin>]` reusing the deterministic `run_session` loop via `UiIdlePump` (0 tick leaks, 0 new external crates, full Vault/Scent/Sprout/Workspace Trust parity).
- **Embedded Obsidian/Light mobile-first PWA (`index.html`, `styles.css`, `app.js`, `manifest.webmanifest`, `icon.svg`)**: installable on Android Chrome and desktop, built with native `<dialog>` modals, `color-scheme` + `light-dark()` with fallbacks, `100dvh` + `env(safe-area-inset-*)`, `@container` split panes, 44px touch targets, colorized unified diffs + `# impact:` banners, `/` and `@file` autocomplete, and 100% DOM-safe rendering (zero `innerHTML`, strict CSP with `require-trusted-types-for 'script'`).
- **Secure API-key authentication & Svelte-ready API (`/api/v1/...`)**: binds `127.0.0.1:8787` by default (generating an ephemeral 256-bit OS-CSPRNG key), refuses non-loopback binds (`0.0.0.0`) without an explicit `--api-key` / `ONYX_WEB_API_KEY` (`>= 16` chars), supports `HttpOnly` session cookies (`__Host-` + `Secure` behind HTTPS proxies) and `Authorization: Bearer <key>`, enforces `Sec-Fetch-Site` CSRF checks, `Clear-Site-Data` on logout, and explicit `--cors-origin` allowlisting for external SPAs (e.g. Svelte).

### Security (`onyx-workspace`, `open-onyx` & CI — Workspace Trust, Sprout/OCI Injection Hardening, Honest Windows Sandbox Badge & `--api-base` Protection)

- **Workspace Trust with `blake3` pinning (`onyx-workspace/src/trust.rs`, `policy.rs`, `sandbox.rs`, `impact.rs`, `sprout.rs`, `open-onyx/src/{main.rs, command.rs, tui.rs, ops.rs}`)**: tracks approved workspaces and `blake3` hashes of `.onyx/config.toml` and `sprout.toml` in `~/.onyx/trusted_workspaces.toml` (`ONYX_TRUST_FILE` override for tests/CI). Untrusted or modified repositories can only tighten policy (`deny`/`ask`, stricter sandbox/impact); project `allow` rules, `env_passthrough`, `allow_read_paths`, `container_image` overrides, and `sprout.toml` execution are blocked until explicitly approved via `openonyx trust`, `--trust-workspace`, or `/trust` in the TUI (showing `[UNTRUSTED]` in the header and auditing status in `openonyx doctor`).
- **Strict `sprout.toml` & OCI validation (`onyx-workspace/src/{sandbox.rs, sprout.rs}`)**: locked `[runtime].container_cli` to closed enum `ContainerCli` (`docker` | `nerdctl` | `podman`), added strict `validate_oci_image_ref`, `validate_container_name`, and opt-in `exec_user` (`validate_exec_user`, passing `--user` only when configured so dev workflows are preserved), rejecting flag and binary injection.
- **Honest Windows sandbox badge & process-tree verification (`onyx-workspace/src/sandbox.rs`, `open-onyx/src/{tui.rs, ops.rs}`)**: added `SandboxMode::effective_badge(driver)` displaying `[SANDBOX:env+job]` on Windows (`Win32Job`) and `[SANDBOX:env+pgid]` when no kernel FS/net isolation driver is active, added explicit Windows `doctor` guidance recommending `--sandbox strict-container` for untrusted workloads, and added parent+child `ProcessJob` teardown tests.
- **`--api-base` transport & API-key anti-exfiltration + CI supply-chain checks (`open-onyx/src/main.rs`, `deny.toml`, `.github/workflows/ci.yml`)**: allows `http://` only on loopback (`localhost`, `127.0.0.1`, `[::1]`), requires `https://` for remote hosts, requires explicit `--allow-custom-auth-host` (`ONYX_ALLOW_CUSTOM_AUTH_HOST=1`) before sending API keys to non-official remote HTTPS endpoints, and adds `cargo audit` + `cargo deny` CI security job.

### Added (`onyx-scent`, `onyx-workspace` & `open-onyx` — `SDD-mix-1` Wave A: Scent-Heated Approval, `signature_hash` & Symbol Provenance)

- **On-demand range impact & signature hashing (`onyx-scent`)**: added deterministic line-invariant `Entity.signature_hash` (`compute_signature_hash`), immutable `WorkspaceIndex::impact_for_line_ranges` (`RangeImpactSummary`, L3 derived-not-stored), and `signature_snapshot` / `diff_changed_signatures` / `stale_callers_for_changed_signatures`.
- **Monotonic Scent-Heated Approval (`onyx-workspace/src/impact.rs`, `tools.rs`, `bridge.rs`, `policy.rs`)**: parses `[impact]` (`enabled`, `max_hops`, `ask_heat_bp`, `hard_ask_bp`) from `.onyx/config.toml`; monotonically escalates `Allow → Ask` when `fs_edit`/`fs_write` touches high-heat public `scope:src` symbols or exceeds `hard_ask_bp` (never weakening `Deny` or `Ask`, degrading cleanly when unindexed or disabled).
- **Symbol provenance & enriched approval banners (`onyx-workspace/src/state.rs`, `open-onyx/src/{tui.rs, session.rs, ops.rs}`)**: persists touched epicenters as `ws.effect.symbols` (`RecordedEffect.symbols`, tagged inline for deterministic `ONYXTR2` replay and shown in `inspect`/`rewind`) and displays `# impact: N callers, H hops, scope:src(pub) [heat Xbp]` right above the unified diff in `TuiDriver` and `CliApprovalDriver`.

### Added (`open-onyx` — SDD-03 Block 2: TUI Input Ergonomics, `/` & `@` Autocomplete & Follow-Up Queue)

- **UTF-8 cursor editing, prompt history & floating autocomplete (`open-onyx/src/tui_input.rs`)**: added `InputBuffer` (`Left`/`Right`/`Home`/`End`/`Delete` with visible reversed cursor), `InputHistory` (`Alt+Up`/`Alt+Down` and `Ctrl+P`/`Ctrl+N` preserving unsent drafts), and floating `PopupState` triggered by `/` (slash commands with descriptions) and `@` (cached workspace file paths respecting `WorkspacePolicy`, ignoring emails like `user@domain`).
- **Queued follow-up prompts & subagent badge (`open-onyx/src/tui.rs`)**: typing a follow-up while the root agent is busy queues it cleanly (`queued:N`, previewed in the input bar, clearable via `Ctrl+X`) and dispatches it automatically upon settle without spawning duplicate roots; displays live `subagents:N` and `queued:N` badges in the header.

### Added (`open-onyx` & `onyx-workspace` — Session Vault `.onyx/vault/` & `--auto-save`)

- **Centralized session vault (`open-onyx/src/vault.rs`)**: short session names for `--snapshot-dir <name>` resolve under `<workspace>/.onyx/vault/<name>/`, co-locating short `--record <file>` transcripts inside the same directory (or under `.onyx/vault/unassigned/<file>` when no snapshot dir is set) while preserving explicit paths verbatim.
- **`--auto-save` & `ONYX_AUTO_SAVE`**: automatically creates descriptive timestamped session folders (`<YYYYMMDD-HHMMSS>_<model>[_<branch|goal>]`) containing both `snapshots.onyxpack` and `<session>.tr`, generates `.onyx/vault/.gitignore` (`*`) automatically so Git status and `/undo` checkpoints never index vault files, displays live `vault:<label>` in the TUI header (updating across `/fork` and `/rewind`), prints the resume command on exit, and resolves short names transparently in `--resume`, `inspect`, `rewind`, `replay`, and `doctor`.
- **Policy & doctor hardening (`onyx-workspace/src/policy.rs`, `open-onyx/src/ops.rs`)**: `.onyx/vault/**` is ignored during workspace scans (`fs_list`, `glob_files`, `code_search`), protected as read-only Control Plane against agent writes, and audited by `openonyx doctor`.

### Added (`open-onyx` — SDD-03 Block 1: Non-Destructive Time-Travel & Forking)

- **CLI & live TUI time-travel / forking**:
  - `openonyx chat --resume <dir> --tick <n> [--fork-dir <dir>] [--truncate-future]` and `openonyx rewind <dir> <tick> --interactive|--tui [--fork-dir <dir>] [--truncate-future]` restore a historical milestone into a safe sibling fork pack by default (leaving the original `snapshots.onyxpack` untouched unless `--truncate-future` is passed) and launch the interactive TUI.
  - Live `/rewind [tick]` (lists milestones without arguments; rewinds in-place when idle with active-agent / in-flight guards and quiet Git reconciliation) and `/fork [name]` (branches `snapshots.onyxpack` up to the current tick into a sibling directory via `SharedPackStore`, emitting `cmd:/rewind` and `cmd:/fork` milestone tags).
  - `PackSnapshotStore::verify_pack_integrity`, `fork_pack_until_tick`, `truncate_pack_until_tick`, and `default_fork_dir` in `open-onyx/src/pack.rs`.

### Added (`open-onyx`, `onyx-workspace` & `onyx-soul-agents` — Multi-Agent TUI, `--effort`, Anti-Loop Hardening & User Manual)

- **Reasoning effort control**: added `--effort <low|medium|high>` and `ONYX_EFFORT` (with provider fallbacks `GEMINI_THINKING_LEVEL` and `OPENAI_REASONING_EFFORT`), wired into Gemini `thinkingLevel` and OpenAI `reasoning_effort` and surfaced in the compact 3-row TUI header.
- **Interactive multi-agent TUI ergonomics**: added `F2` Agent Selector popup (`render_agent_picker`), `/agent [id|name]` (`cmd:/agent`), `Ctrl+Tab` quick-cycle, `[agent #id name]` origin badges in approval banners, animated Braille status emblem (`⠋..⠏`, 90 ms throttle), auto-focus on `/new`, and active-root protection preventing duplicate root agent spawns while `thinking`/`acting`/`waiting_children`/`waiting_approval`.
- **Local LLM & tool anti-loop hardening**:
  - `onyx-soul-agents`: capped inherited `spawn_agent` child step budgets at `DEFAULT_SUBAGENT_MAX_STEPS = 32` (with optional explicit `max_steps` override in JSON args, preserving 0 external dependencies).
  - `onyx-workspace`: added session `seen_outlines` cache for `scent_outline` (invalidated automatically on file edits, `shell_exec`, and `/undo`), overlapping/subset `fs_read` range deduplication (`covered_by:<start>-<end>`), and actionable error hints when reading binary OnYX artifacts (`ONYXTR1`, `ONYXTR2`, `ONYXPK1`, `ONYXAG`).
  - `open-onyx`: injected host OS shell hints (Windows PowerShell vs Unix/POSIX) and anti-loop rules into `BASE_INSTRUCTION`.
- **Comprehensive User Manual (`docs/manual/`)**: added a complete, cross-linked 8-chapter User Manual for `open-onyx` covering ES/EN quickstart, daily TUI & multi-agent workflow, modes/permissions/OS sandbox, time-travel/snapshots/forking, all 19 tools + `onyx-scent` + Sprout + custom skills, and full CLI/troubleshooting reference, and synchronized `--help`, TUI hints, `SKILL.md`, and crate READMEs.

### Fixed

- **OpenAI/vLLM SSE resilience (`open-onyx/src/openai.rs`)**: tolerate chunked-stream EOF errors after a completed response (`finish_reason` / `[DONE]`) and retry up to 2 transient transport/408/429/5xx/stream errors with exponential backoff (`400ms`, `1200ms`).
- **Untracked file cleanup on `/undo` & `/rewind` (`onyx-workspace/src/state.rs`, `bridge.rs`, `open-onyx/src/ops.rs`)**: `restore_git_tree_checkpoint_for_targets` now deletes newly created untracked `fs_write` files that did not exist in the pre-mutation `tree_sha`, while preserving unrelated user untracked files.
- **Linux Ubuntu 24.04 AppArmor `bwrap` & CI fixes (`onyx-workspace/src/sandbox.rs`, `.github/workflows/ci.yml`)**: added live `bwrap_is_usable()` probe with safe fallback when AppArmor restricts unprivileged user namespaces, fixed `/tmp` bind order and pre-sandbox offline network checks, fixed Sprout log readiness test ordering, and aligned CI toolchain to `1.85.1`.

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
