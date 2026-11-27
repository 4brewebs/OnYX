[![CI](https://github.com/4brewebs/OnYX/actions/workflows/ci.yml/badge.svg)](https://github.com/4brewebs/OnYX/actions/workflows/ci.yml)                             [![crates.io](https://img.shields.io/crates/v/open-onyx.svg)](https://crates.io/crates/open-onyx)                                                                   [![downloads](https://img.shields.io/crates/d/open-onyx.svg)](https://crates.io/crates/open-onyx)                                                                   [![docs.rs](https://docs.rs/onyx-core/badge.svg)](https://docs.rs/onyx-core)                  [![MSRV](https://img.shields.io/badge/MSRV-1.85-blue.svg)](https://www.rust-lang.org)         [![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE-MIT) 
# OnYX

[![User Manual](https://img.shields.io/badge/docs-User%20Manual-1f6feb?style=for-the-badge)](https://4brewebs.github.io/OnYX/index-en.html)
[![User Manual](https://img.shields.io/badge/docs-User%20Manual-1f6feb?style=for-the-badge)](https://4brewebs.github.io/OnYX/)
[![Quickstart EN](https://img.shields.io/badge/quickstart-English-2da44e?style=for-the-badge)](docs/manual/01-first-steps.md)
[![Primeros Pasos ES](https://img.shields.io/badge/gu%C3%ADa-Espa%C3%B1ol-e36209?style=for-the-badge)](docs/manual/01-primeros-pasos.md)


| Crate | crates.io | docs.rs | Descripción |
| --- | :---: | :---: | --- |
| [`open-onyx`](open-onyx) |[![crates.io](https://img.shields.io/crates/v/open-onyx.svg)](https://crates.io/crates/open-onyx) | [![manual](https://img.shields.io/badge/docs-User%20Manual-1f6feb.svg)](https://4brewebs.github.io/OnYX/) | Harness CLI/TUI/Web PWA (`cargo install open-onyx`) |             
| [`onyx-workspace`](onyx-workspace) |[![crates.io](https://img.shields.io/crates/v/onyx-workspace.svg)](https://crates.io/crates/onyx-workspace) | [![docs.rs](https://docs.rs/onyx-workspace/badge.svg)](https://docs.rs/onyx-workspace) |Herramientas de código, sandbox OS, Sprout y checkpoints Git |
| [`onyx-scent`](onyx-scent) |[![crates.io](https://img.shields.io/crates/v/onyx-scent.svg)](https://crates.io/crates/onyx-scent) | [![docs.rs](https://docs.rs/onyx-scent/badge.svg)](https://docs.rs/onyx-scent) | Destilador AST (`tree-sitter`) + caché `blake3` + `@TOON v1` |
| [`onyx-soul-agents`](onyx-soul-agents) | [![crates.io](https://img.shields.io/crates/v/onyx-soul-agents.svg)](https://crates.io/crates/onyx-soul-agents) |[![docs.rs](https://docs.rs/onyx-soul-agents/badge.svg)](https://docs.rs/onyx-soul-agents) | Soul multi-agente determinista · **0 deps externas** |
| [`onyx-soul`](onyx-soul) | [![crates.io](https://img.shields.io/crates/v/onyx-soul.svg)](https://crates.io/crates/onyx-soul) | [![docs.rs](https://docs.rs/onyx-soul/badge.svg)](https://docs.rs/onyx-soul) | Soul de referencia · **0 deps externas** |
| [`onyx-core`](onyx-core) | [![crates.io](https://img.shields.io/crates/v/onyx-core.svg)](https://crates.io/crates/onyx-core) | [![docs.rs](https://docs.rs/onyx-core/badge.svg)](https://docs.rs/onyx-core) | Contratos del microkernel + `tick` + `DetRng` · **0 deps** |

**A Lego of brains, not a closed solution.**

OnYX separates a small, dependency-free core of contracts from the souls that fulfill them.
If your reality is different, write your own soul: the contract is what matters.

**Status: v0, under active design.**
The public contract is not yet closed and will change in breaking ways.
Do not use it where a failure has consequences.

The full project description, including proposed design that is not yet code, is in Spanish in [README_es.md](README_es.md). For the full **`openonyx`** CLI/TUI guide (quickstart in English & Spanish, TUI workflow, sandbox & permissions, time-travel/forking, 23-tool catalog, and troubleshooting), see the **[openOnyx User Manual](docs/manual/index.md)**. Contributor and agent rules are in [CONTRIBUTING.md](CONTRIBUTING.md).

```
onyx-core           = contracts + tick + deterministic RNG            · 0 dependencies
onyx-soul           = default implementations of those contracts      · 0 external dependencies
onyx-soul-agents    = deterministic multi-agent harness soul          · 0 external dependencies
onyx-scent          = AST distiller + blake3 cache + @TOON v1         · optional cli feature
onyx-workspace      = sandboxed coding tools + bridge                 · uses onyx-soul-agents (+ optional onyx-scent)
open-onyx           = CLI/TUI/Web PWA coding agent harness (Gemini/OpenAI/Anthropic + ratatui + SSE) · uses onyx-soul-agents + onyx-workspace
onyx-example-app    = minimal REPL that exercises onyx-soul           · uses onyx-soul
onyx-example-agents = deterministic multi-agent tour                  · uses onyx-soul-agents
```

## Workspace

The workspace has eight member crates (`onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-scent`, `onyx-workspace`, `open-onyx`, `onyx-example-app`, and `onyx-example-agents`). The minimum Rust version is `1.85`, the edition is `2024`, and the workspace uses resolver `3`.

`onyx-core`, `onyx-soul`, `onyx-soul-agents`, and both example crates declare zero external crates. Souls depend on `onyx-core`. Each example depends on its soul, which reexports the core types the app needs, so the app does not declare `onyx-core`. Adapter crates (`onyx-scent`, `onyx-workspace`, `open-onyx`) add external dependencies while preserving pure deterministic core/soul semantics. [`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs `fmt`, `check` (including `onyx-workspace --no-default-features`), `clippy`, `test`, `test --release`, `doc`, and a `security` job (`cargo audit` + `cargo deny` via [`deny.toml`](deny.toml)); run `cargo tree -p onyx-core -p onyx-soul -p onyx-soul-agents` to verify the zero-dependency rule.

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
- `AgentSpawner`, `AgentRuntime`, `SubagentSystem` (`spawn_agent` with default `32`-step child cap & optional override / `AgentState::WaitingChildren`), `ToolSystem` / `ToolExecutor`
- `RecordingBridge` / `ReplayBridge` (`ONYXTR1` / `ONYXTR2` transcripts), `HeadlessDriver`
- `AgentState::WaitingApproval`, `ApprovalPolicy`, structured tool calls (`Reply::call_structured`)

### `onyx-scent`
- Deterministic multi-language **Scent 2.0** `tree-sitter` AST indexer — **7 languages**: `Rust`, `Python`, `Java`, `TypeScript` (`.ts/.tsx/.mts/.cts`), `JavaScript` (`.js/.jsx/.mjs/.cjs`), `Go`, and `C/C++` (`.c/.h/.cc/.cpp/.cxx/.hh/.hpp`) — with `blake3` incremental cache and a persistent binary cache **`.onyx/scent.idx`** (`ONYXSCI1`: magic + version + sorted entries + final `blake3` checksum, atomic temp-file+rename writes, silent rebuild on any anomaly, hygiene `.onyx/.gitignore`)
- Integer-only (`0..=10_000` bp) Max-Heat impact diffusion (`distill_heat`), `scope:src|test` & `vis:pub|priv` tagging, `exclude_tests` filtering, `EntityKind::{TypeAlias, Component}` (JS/TS type aliases and PascalCase JSX components, both callable), trait method visibility inheritance, struct field summaries, Java annotations, intra-file call resolution, and `@TOON v1` paginated encoder/decoder (`outline_toon`, `impact_toon`, `references_toon`)
- Pure library by default (`clap` gated behind `feature = "cli"`), with the `scent` binary exposing `outline`, `impact`, and `references` subcommands

### `onyx-workspace` (`0.1.1`)
- `WorkspacePolicy`, `PermissionRules` & `PermissionDecision` (canonicalized root sandbox, symlink-safe traversal, ignored dirs & `.onyx/vault/` + runtime artifacts, Control Plane RO protection for `.onyx/config.toml`/`.onyx/vault/**`/`.git/hooks/**`/`.git/config`/`.env*`, I/O and timeout limits, plus declarative `[permissions]`, `[sandbox]`, and `[impact]` *Scent-Heated Approvals* loaded from `~/.onyx/config.toml` and `<workspace>/.onyx/config.toml` with precedence `deny > ask > allow` and unconditional read-only blocking in `Plan` / `Explore`)
- Workspace Trust (`trust.rs`): `blake3` trust store (`~/.onyx/trusted_workspaces.toml`, `ONYX_TRUST_FILE`) gating project-level `.onyx/config.toml` relaxations and `sprout.toml` mutations until trusted
- OS Sandbox & Process Supervision (`sandbox.rs`): `SandboxMode` (`Off`, `WorkspaceWrite`, `StrictContainer`) with honest `effective_badge` (`env+job` on Windows), `NetworkPolicy` (`Offline`, `LocalhostOnly`, `Unrestricted`), strict OCI validation (`ContainerCli` + `validate_oci_image_ref`), `apply_sanitized_env` (`env_clear()` + safe allowlist stripping `*_API_KEY`/`*_TOKEN`/`AWS_*`), cross-platform RAII `ProcessJob` (Win32 Job Objects `KILL_ON_JOB_CLOSE` + memory/PID limits, POSIX `PGID`), `bwrap` (with live AppArmor userns fallback) / `sandbox-exec` / OCI wrappers, and `detect_sandbox_capabilities`
- Local Service & Container Orchestrator (`sprout.rs`, SDD-08): pure-`std` `SproutSupervisor` (0 `tokio`), unified `sprout.toml` (`[runtime]`/`[services.<id>]` + legacy `[global]`/`[[service]]`), opt-in `exec_user` + Workspace Trust gating, DAG validation, `process` & `container` (`docker`/`nerdctl`/`podman`) services, real probes (`ready_port`/`ready_log`/`ready_cmd`), `service_status`/`service_start`/`service_stop`/`service_logs`/`service_exec`, `ws.services.text` EAV projection, and `audit_sprout_workspace` for `doctor`
- MCP Client (`mcp.rs`, SDD-04): 0-deps parser for `[mcp.servers.<name>]` (`transport = "stdio"` (`command`/`args`/`env`) or `"http"` (`url`/`headers`), `timeout_secs` default 20, `enabled`), global < workspace *replace-by-name* merge with Workspace Trust gate, JSON-RPC 2.0 over stdio (dedicated reader thread, per-call `recv_timeout`, 4 MiB line cap, `ProcessJob` teardown) and Streamable HTTP (`Mcp-Session-Id`), dynamic `mcp__<server>__<tool>` tools with `requires_approval = true`, `truncate_utf8` + `scrub_secrets` on results/errors, per-server failure isolation for `doctor`, and `extract_mcp_tools_from_transcript` for 0-I/O replay
- Verify-on-Edit (`verify.rs`, SDD-05): 0-deps parser for `[verify]` (`on_edit` opt-in, `max_secs` default 20 clamped `1..=120`) and `[[verify.rules]]` (`glob`, `command`, optional `max_secs`) from `~/.onyx/config.toml` / `<workspace>/.onyx/config.toml` with Workspace Trust gate and global < workspace (trusted) replace-by-rule-set merge; after each successful `fs_edit`/`fs_write`, matching rules run through the `shell_exec` sandbox pipeline (with `[permissions].deny` pre-check, `ProcessJob`, per-rule timeout, `scrub_secrets`, 4 KiB diagnostic cap) and inject `[verify: PASS|FAIL|BLOCKED]` (+ `impacted_tests: [...]` from Scent `scope:test` files within ≤ 2 hops) into the tool observation
- Isolated subagent worktrees (`bridge.rs`, SDD-05 REQ-05.5): `spawn_agent(isolated: true)` children run in lazy `onyx/sub/agent-<parent>-<child>-<seq>` Git worktrees (`WorktreeManager` + `WorkspacePolicy::for_worktree` + `NativeOffline`, `commit-tree` creation without moving root `HEAD`/index), unconditionally finalized on terminal/vanished children with ephemeral-commit capture + `git merge-tree` 3-way reconciliation (`IsolatedWorktreeResult`: `Pending`/`Applied`/`Discarded`/`Transferred`), clean 3-ways applied sequentially to root, conflicts transferred to `SubagentManager` (`subagent_status`/`subagent_merge`/`subagent_discard`), and worktree/branch cleanup on apply/discard, `Drop`, and `doctor` pruning
- `WorkspaceTools` (`fs_read` with `recall_seen`, overlapping range dedup `covered_by`, and actionable binary OnYX artifact errors; `fs_list`; `fs_write`; multi-hunk atomic `fs_edit` with line-trimmed fallback & relative re-indentation; `code_search` v2 with `regex`/`case_insensitive`/`include_glob`/`exclude_glob`/`context_lines`; read-only `glob_files`; approval-free `git_status` & `git_diff`; `load_skill` for on-demand `SKILL.md` / built-in guides; `plan_update`; `shell_exec` with secret scrubbing and cache invalidation; plus read-only `scent_outline` with session `seen_outlines` cache & `scent_impact` and `scent_references` (exact definition + direct cross-references: callers, callees, implementors, type users) under `feature = "scent"`, `max_rows` clamped `1..=500`, `public_only`)
- `preview_tool_diff` (pure single/multi-hunk unified diff preview for `fs_write`/`fs_edit`)
- `WorkspaceMode` (`Plan`, `Build`, `Explore`), `WorkspaceModeHandle`, graph-backed `ws.mode` & `ws.plan.text`, `WorkspaceLedgerSystem` (EAV `File` & `ExternalEffect` ledger, `effects_after_tick`, `latest_undoable_effect`, `mark_latest_effect_undone`, `undone_effects`), Git tree checkpoints (`capture_git_tree_checkpoint` supporting unborn `HEAD`, `restore_git_tree_checkpoint_for_targets` also removing newly created untracked `fs_write` files while keeping `.git/index` and unrelated user files clean), and `WorkspaceToolBridge` (granular policy auto-approval/auto-denial, inline `[checkpoint:<head>:<tree>]` tagging for deterministic `ReplayBridge` playback, and live `/undo` materialization)

### `open-onyx` (`openonyx`, `0.1.1`)
- Modular CLI, TUI & embedded Web PWA coding agent harness ([User Manual](docs/manual/index.md)):
  - [`main.rs`](open-onyx/src/main.rs) (`run`, `chat [--resume <dir> [--tick <n>] [--fork-dir <dir>] [--truncate-future]]`, `serve [--bind] [--api-key] [--cors-origin]`, `trust [add|list|check|revoke]`, `replay`, `inspect [--tick <n>] [--list] [--json]`, `rewind [--dry-run] [--interactive|--tui] [--fork-dir <dir>] [--truncate-future]`, `bench --snapshot-dir <dir> --tick <n> --models <list> [--goal] [--verify <cmd>] [--out <file.json>] [--max-steps <n>]`, `doctor`, `--provider <gemini|openai|anthropic>`, `--model <name>`, `--effort <low|medium|high>`, `--thinking-budget <n>`, `--api-base <url>`, `--allow-custom-auth-host`, `--trust-workspace`, `--sandbox`, `--sandbox-net`, `--auto-approve`, `--ignore-deny`, `--max-steps <n>`, `--shell-timeout <secs>`, `--auto-save` / `ONYX_AUTO_SAVE`, `--record` / `--save-transcript`, `--snapshot-every <n>`, `--max-snapshots <n>`)
  - [`vault.rs`](open-onyx/src/vault.rs) (centralized `.onyx/vault/` session vault with automatic `.gitignore`, `<session>/` co-locating `snapshots.onyxpack` + `.tr`, `unassigned/` for standalone transcripts, `--auto-save` readable IDs, and transparent short-name lookup)
  - [`context.rs`](open-onyx/src/context.rs) (shared `TieredContextConfig` / `ToolOutputTier` Tier 0/1/2 projection, `ReasoningEffort`, cache-stable prefixes, focus-directed deterministic `/compact` summary with `focus_matches` & `files_found`, and normalized `TokenUsage` / `SessionTelemetryHandle` for Gemini and OpenAI)
  - [`command.rs`](open-onyx/src/command.rs) (pure deterministic `CommandSpawnerSystem` handling `/new`, `/mode`, `/agent`, `/undo`, `/rewind [tick]`, `/fork [name]`, `/compact [focus]`, `/services`, `/service`, the SDD-07 sealed TimeFreeze primitives (`!cmd`, `/read`, `/inject`, `/step [n]`, `/step-llm`, `/go` `/unfreeze`, `/time`, `/graph inspect|drop-mem|edit-mem|set-goal`), the shared `is_immediate_session_command` so TUI and Web never queue the freeze escape routes while the root is busy, `on_restore` mode sync, and multi-turn `continue_agent` with active-root guard)
  - [`pack.rs`](open-onyx/src/pack.rs) (`PackSnapshotStore` append-only zlib-compressed `snapshots.onyxpack` with 64-bit FNV-1a checksums, truncated-tail recovery, `SharedPackStore` live branching, `fork_pack_until_tick`, `truncate_pack_until_tick`, opt-in `--max-snapshots`, and legacy `tick-*.bin` fallback, plus deterministic `MilestoneSnapshotSystem`)
  - [`session.rs`](open-onyx/src/session.rs) (declared logical surfaces `SURFACE_STATUS`/`SURFACE_TRACE`/`SURFACE_APPROVAL`/`SURFACE_PLAN`/`SURFACE_SERVICES` passed via `AgentHost::step_with`, `SessionDriver`, policy-aware `CliApprovalDriver`, live `/rewind` & `/fork` orchestration via `apply_session_action`, unified `run_session` + `StopPolicy`)
  - [`ops.rs`](open-onyx/src/ops.rs) (`inspect [--list] [--json]`, non-contiguous `rewind` floor reconciliation for CLI & TUI, and `doctor` validation of `snapshots.onyxpack`, legacy `.bin` snapshots, sandbox capabilities, `sprout.toml`, and orphaned-worktree pruning)
  - [`bench.rs`](open-onyx/src/bench.rs) (SDD-05 REQ-05.6 `openonyx bench`: multi-model benchmark from an identical snapshot — required `--snapshot-dir`/`--tick`/`--models`, live `provider:model` candidates via `resolve_llm_config`/`build_sync_responder` with real telemetry or offline `replay:<label>=<file.tr>`/`replay:<file.tr>` `ONYXTR2` transcripts via a deterministic `BenchTranscriptDriver` (`:` split before `=`, Windows drive-letter paths intact), per-candidate throwaway `bench-<pid>-<idx>-<label>` worktree with RAII guard + `reconcile_rewind_effects_report` rewind + fresh restored host, optional `--goal` root agent when the tick has no active agent, sandboxed `--verify` in the worktree (same `shell_exec` pipeline, honors `[permissions].deny`), without `--verify` `passed_verify` = settled `Done`, 9-column Markdown table on stdout + pretty JSON report via `--out`, root workspace and `HEAD` never mutated)
  - [`time_debug.rs`](open-onyx/src/time_debug.rs) (SDD-07 `TimeFreeze` interactive time debugger: `TimeDebugState` — frozen/cursor/present over a RAM tick-boundary ring + on-disk milestone references, `preview_graph(cursor)` projection cache, 0-token primitives sealed into `ONYXTR2` as `/__onyx_det_*` (sandboxed `!cmd`, `/read`, `/inject`, `/step [n]`, `/step-llm`) for bit-for-bit offline replay, safe frozen-graph surgery `/graph inspect|drop-mem|edit-mem|set-goal` via `SealedGraphOp`, and `confirm_branch_from_cursor` — git tree checkpoint restore from `tree_sha` + pack fork + ring truncation — 100 % ephemeral until the branch is confirmed; non-goal: Sprout/MCP effects are not rewound)
- [`gemini.rs`](open-onyx/src/gemini.rs): `GeminiResponder` (sync), `AsyncGeminiDriver` (non-blocking `:streamGenerateContent?alt=sse` HTTP worker + `StreamPreviewHandle` + `thinkingLevel` (`--effort`) + `usageMetadata` extraction + `InFlightGuard`), `~/.onyx/{AGENTS,ONYX}.md` & `<workspace>/{AGENTS,ONYX}.md` instruction loading + OS shell hint + compact skill catalog (`discover_skills_summary`), tiered context projection, sequential multi-`functionCall` queueing with `thoughtSignature` propagation, `"user"`/`"system"` turn mapping
- [`openai.rs`](open-onyx/src/openai.rs): OpenAI-compatible `/v1/chat/completions` adapter (`OpenAiResponder` + streaming SSE `AsyncOpenAiDriver` with `reasoning_effort`, `stream_options.include_usage`, `StreamPreviewHandle`, completed-SSE EOF tolerance & transient retry backoff, shared tiered context projection, and sequential tool-call queueing for OpenAI, Ollama, vLLM, and OpenRouter)
- [`anthropic.rs`](open-onyx/src/anthropic.rs): native Anthropic Messages API adapter (`POST /v1/messages`, `x-api-key` + `anthropic-version: 2023-06-01`, SSE `message_start`/`thinking_delta`/`text_delta`/`input_json_delta`/`message_delta`, ephemeral prompt caching `cache_control` on system + last tool definition, extended thinking `thinking.budget_tokens` via `--thinking-budget`/`ANTHROPIC_THINKING_BUDGET` with `--effort` mapping 2048/8192/16384, and transient retry backoff on `408`/`429`/`5xx`/`529` honoring `retry-after`)
- [`tui.rs`](open-onyx/src/tui.rs): `TuiDriver` (`ratatui` + `crossterm`), 5-row header (tab strip `F1 Chat` · `F2 Agents` · `F3 Sprout` · `F4 PlanDiff` · `F5 Timeline` with per-view `!APPROVAL` badges, live title ` openOnyx [MODE] [SANDBOX:workspace|env+job|strict|off] [UNTRUSTED] · provider/model (effort:...) ` with animated Braille emblem, body with `goal:`/`phase:`/agent tags, quick hints, and live telemetry border bar `ctx:`/`in/cache/out/think`/`~$cost`), live SSE token preview (`[stream] ...▍` in `pump_idle`), `F1..F5` view switching (`/view`) + `Ctrl+K` command palette (View/LLM/Agent groups; any printable key filters, `Up`/`Down`/`Ctrl+P`/`Ctrl+N` navigate, `Enter` runs, `Esc` closes first), LLM hot-swap (`/profile <id>`, `/provider <p>`, `/model <m>`, `/effort <e>` from `[[llm_profiles]]`), async subagent worktree jobs listed in the `F2 Agents` view (`[bg]` status + `→ <driver>` badge; `/spawn [--driver <id>]`, `/subagents`, `/merge <job_id>`, `/discard <job_id>`), agent focus (`Ctrl+Tab` next agent, `/agent [id|name]` with interactive Agent Selector popup when several agents are alive), `y`/`n` approvals only in `Chat` (`Esc` quits there, goes back in `F2..F5`), full wrapped chat history scrolling (`Up`/`Down`/`PageUp`/`PageDown`/`Home`/`End`/`Ctrl+U`/`Ctrl+D`), optional `70%`/`30%` `Live Plan` + `Services (Sprout)` split panels (`>= 100` cols), `Tab` mode toggle, `[agent #id]` colored unified diff preview box (with `# impact:` Scent heat banner) on sensitive tool approvals (`y`/`n`), and the SDD-07 TimeFreeze time debugger (amber `⏸ TIME FROZEN · tick X / present Y` banner with the files that would be reverted on branch, `Alt+←`/`Alt+→` scrub over available ticks, `Ctrl+T` freeze toggle, safe `Esc` returning to present without quitting, `F5 Timeline` scrubbing with `[pack:*]`/`[ram]`/`(present)` labels and `▶ cursor`, plus the `/time` `/go` `/step [n]` `/step-llm` `!cmd` `/read` `/inject` `/graph …` escape routes that run immediately even while the root agent is busy in `Thinking`)
- [`web.rs`](open-onyx/src/web.rs) & [`open-onyx/web/`](open-onyx/web/): `WebDriver` HTTP/1.1 + SSE server (`std::net::TcpListener`, 0 `tokio`) powering `openonyx serve` with OS CSPRNG + `blake3` auth (`HttpOnly`/`__Host-` cookies + `Bearer`), `Sec-Fetch-Site` CSRF guard, strict CSP, explicit `--cors-origin` for external frontends, an embedded mobile-first Obsidian/Champagne-Gold PWA with real rotating `openOnyx` logo (`logo.jpg`), native `<dialog>` modals, and zero `innerHTML`, plus SDD-07 TimeFreeze Web parity (`#tick-pill` button opening the amber `#time-debug-bar` with a snap-to-available-tick slider, `Step`/`Step 1 LLM`/`Resume` routed through the sealed command tunnel, the ephemeral `POST /api/v1/time/scrub` endpoint — `freeze`/`unfreeze`/`return_to_present`/`scrub_to`/`scrub_delta` — inheriting Bearer auth + the `Sec-Fetch-Site` CSRF guard, `409 Conflict` with an actionable message for plain prompts under freeze, `✎`/`×` memory surgery buttons on root memories (`/graph edit-mem` / `/graph drop-mem`), and the copy-paste `openonyx bench` `bench_hint` synergy)

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
