# onyx-workspace

**Workspace coding tools, EAV side-effect ledger, mode policies, and Git tree checkpoints for harnesses built on `onyx-soul-agents`.**

Provides sandboxed filesystem, Git inspection, AST outline/impact (`onyx-scent`), and shell execution capabilities that plug directly into `onyx-soul-agents` as a `PlatformDriver` bridge, keeping external I/O out of deterministic `OnyxSystem`s and preserving diskless `Transcript` replay.

## Components

- `WorkspacePolicy`, `PermissionRules` & `PermissionDecision`:
  - Canonicalizes `root` on disk and blocks path escapes (`..` or symlinks outside `root`) via `resolve_existing` and `resolve_for_write`.
  - Skips `ignored_dirs` (`.git`, `target`, `node_modules`) and runtime snapshot/transcript artifacts (`snapshots/`, `snaps_*`, `*.tr`, `*.onyxpack`, `tick-*.bin`) by default.
  - Enforces default I/O limits (`max_read_bytes = 64 KiB`, `max_output_bytes = 32 KiB`, `command_timeout = 30s`, configurable via `WorkspacePolicy::with_command_timeout`).
  - Loads declarative `[permissions]` rules (`deny`, `allow`, `ask`) and `[sandbox]` settings (`mode`, `network`, `env_passthrough`, `allow_read_paths`, `max_memory_mb`, `max_pids`, `container_image`) from `~/.onyx/config.toml` and `<workspace>/.onyx/config.toml` (`with_loaded_config` / `with_permissions` / `with_sandbox` / `with_ignore_deny`) with precedence `deny > ask > allow`; global `deny` rules cannot be removed by project config, `WorkspaceMode::Plan` / `Explore` unconditionally block mutating tools, and Control Plane files (`.onyx/config.toml`, `.git/hooks/**`, `.git/config`, `.env*`) are protected read-only (`check_control_plane_write`).
- OS Sandbox & RAII Process Supervision (`sandbox.rs`):
  - `SandboxMode` (`Off`, `WorkspaceWrite` default, `StrictContainer`), `NetworkPolicy` (`Offline` default, `LocalhostOnly`, `Unrestricted`), and `SandboxConfig::elevate_for_auto_approve`.
  - Strict environment sanitization (`apply_sanitized_env`): starts with `env_clear()`, strips all sensitive keys (`*_API_KEY`, `*_TOKEN`, `AWS_*`, etc.), redirects `TMP`/`TEMP`/`TMPDIR` to a session-scoped temp dir, and injects offline tool hints (`CARGO_NET_OFFLINE`, `PIP_NO_INDEX`, `NPM_CONFIG_OFFLINE`).
  - Cross-platform `ProcessJob` (`std::process::Child`, zero `tokio`): Win32 Job Objects (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, memory/PID limits) on Windows and POSIX `PGID` termination on Unix, designed for reuse by both `shell_exec` and `onyx-sprout` (SDD-08).
  - Native OS driver selection (`bwrap`, `landlock`, `sandbox-exec`, `win32-job`, OCI `docker`/`nerdctl`/`podman`) plus `detect_sandbox_capabilities()` for `openonyx doctor`.
- `WorkspaceTools` & `ToolSchema`:
  - `fs_read` — read UTF-8 files (optional 1-based `start_line` / `end_line` range), with `recall_seen` session deduplication (`@TOON v1` `unchanged_since_tick:<N>`) invalidated on `fs_write`, `fs_edit`, `shell_exec`, or `/undo` (`clear_seen_reads`).
  - `fs_list` — list directory entries up to `max_depth` (default `3`), ignoring symlinks and ignored directories.
  - `fs_write` — create or overwrite a UTF-8 file *(requires approval by default)*.
  - `fs_edit` — unique exact replacement (`old_text`/`new_text`) or atomic sequential multi-hunk `edits: [{ old_text, new_text }]` applied in memory before writing, with line-trimmed fallback and automatic relative indentation adjustment *(requires approval by default)*.
  - `code_search` — literal or `regex` search across workspace files with optional `case_insensitive`, `include_glob`, `exclude_glob`, `context_lines` (`0..=5` grouped hunks), secret scrubbing, and symlink/ignored-dir skipping *(read-only, no approval required)*.
  - `glob_files` — read-only workspace file discovery matching glob patterns (`*`, `**`, `?`, `[...]`, `{a,b}`) with deterministic lexicographical order (or optional `sort_by_mtime`), path-traversal rejection, and ignored-directory filtering *(no approval required)*.
  - `git_status` — concise `git status --short --branch` inspection *(read-only, no approval required)*.
  - `git_diff` — unified `git diff --no-color` inspection with optional relative `path` (passed strictly after `--`), `staged`, and `context_lines` *(read-only, no approval required)*.
  - `load_skill` — on-demand protocol/guide loader (`SKILL.md` / `*.md` from `<workspace>/.onyx/skills/`, `<workspace>/skills/`, `~/.onyx/skills/`, plus built-in `scent` and `rust` guides) paired with `discover_skills_summary` *(read-only, no approval required)*.
  - `plan_update` — structured progress checklist (`[x]`, `[>]`, `[ ]` + optional `explanation`) persisted to the EAV graph via `ws.plan.text` (`set_workspace_plan`, `graph_workspace_plan`, `format_plan_steps`) *(no approval required)*.
  - `scent_outline` / `scent_impact` *(optional `feature = "scent"`)* — read-only AST outline and semantic call-impact tools backed by `onyx-scent` (`blake3` incremental cache + `@TOON v1` paginated output with `public_only`, `exclude_tests`, and `max_rows` clamped `1..=500`), validated through `WorkspacePolicy` *(no approval required)*.
  - `shell_exec` — execute a shell command inside `root` with timeout, full process-tree cleanup, concurrent stdout/stderr draining, credential scrubbing (`scrub_secrets`), output limits, and automatic `clear_seen_reads()` *(requires approval by default)*.
  - `service_status`, `service_start`, `service_stop`, `service_logs`, `service_exec` (`sprout.rs`, SDD-08) — local process & OCI container (`docker`/`nerdctl`/`podman`) DAG orchestrator backed by `sprout.toml` (`[runtime]`/`[services.<id>]` and legacy `[global]`/`[[service]]`), real readiness probes (`ready_port`, `ready_log`, `ready_cmd`), bounded UTF-8-lossy log ring-buffers, `env_clear()`, Win32/POSIX `ProcessJob` teardown on `Drop`, and `ws.services.text` EAV graph projection (`service_status`/`service_logs` read-only; `service_start`/`service_stop`/`service_exec` require approval and are blocked in `Plan`/`Explore`).
  - `preview_tool_diff(root, tool, args_json, max_lines)` — pure unified diff preview generator for single- and multi-hunk `fs_edit` / `fs_write` reusing line-trimmed matching and relative re-indentation.
  - `WorkspaceTools::schemas()` / `schemas_with_subagents(true)` — export JSON parameter schemas for LLM function calling.
- `WorkspaceMode`, `WorkspaceLedgerSystem` & Transactional Undo (`state.rs`):
  - `WorkspaceMode` (`Plan`, `Build`, `Explore`), thread-safe `WorkspaceModeHandle`, and graph-backed `ws.mode` (`set_workspace_mode`, `graph_workspace_mode`).
  - Tracks `File` nodes (`ws.file.path`, `ws.file.read_tick`, `ws.file.dirty`) and `ExternalEffect` nodes (`ws.effect.*`, `ws.effect.undone`, `effects_after_tick`, `latest_undoable_effect`, `mark_latest_effect_undone`, `undone_effects`).
  - Lightweight Git checkpoints: `capture_git_tree_checkpoint` writes a temporary `GIT_INDEX_FILE` (`git add -A` + `git write-tree`, supporting repositories both with commits and with an unborn `HEAD`) without mutating `HEAD` or the user's staging area; `restore_git_tree_checkpoint` runs `git read-tree --reset -u <tree_sha>` and resets `.git/index` back to `HEAD` when `HEAD` exists so untracked files are restored cleanly without remaining staged.
- `WorkspaceToolBridge` (`bridge.rs`):
  - Implements `PlatformDriver` wrapping an inner driver (`HeadlessDriver`, `CliApprovalDriver`, or `TuiDriver`).
  - Reads mode from graph (`ws.mode`, falling back to `WorkspaceModeHandle`), enforces `require_read_before_edit`, and evaluates `PermissionRules` (`evaluate_permission_in_mode`) both on `WaitingApproval` (auto-approving `Allow`, auto-denying `Deny` or read-only mode, signaling wakeups via `with_ready_flag`) and on `Acting` (enforcing `deny` even when `--auto-approve` is active).
  - Captures pre-mutation Git checkpoints for `fs_write`/`fs_edit`, records them in `PendingCheckpoints`, and embeds `[checkpoint:<head>:<tree>]` in `Reply::observe` so `WorkspaceLedgerSystem` reconstructs `ws.effect.tree_sha` deterministically even during diskless `ReplayBridge` playback.
  - Materializes `undone_effects` in `present()` (avoiding double-execution on resumed graphs via `sync_existing_undos`), keeping `OnyxSystem`s free of disk/Git I/O.

## Roadmap / TODO (Not Yet Implemented)

- **Streaming / cancellation for `shell_exec`**: allow incremental output capture and cancellation hooks from external adapters without blocking the brain.
