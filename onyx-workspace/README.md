# onyx-workspace

**Workspace coding tools and path sandbox for harnesses built on `onyx-soul-agents`.**

Provides sandboxed filesystem, search, and shell capabilities that plug directly into `onyx-soul-agents` as a `PlatformDriver` bridge, preserving deterministic `Transcript` recording and diskless replay.

## Components

- `WorkspacePolicy`:
  - Canonicalizes `root` on disk and blocks path escapes (`..` or symlinks outside `root`) via `resolve_existing` and `resolve_for_write`.
  - Skips `ignored_dirs` by default (`.git`, `target`, `node_modules`).
  - Enforces default I/O limits (`max_read_bytes = 64 KiB`, `max_output_bytes = 32 KiB`, `command_timeout = 30s`, configurable via `WorkspacePolicy::with_command_timeout`).
- `WorkspaceTools` & `ToolSchema`:
  - `fs_read` — read UTF-8 files (optional 1-based `start_line` / `end_line` range), with `recall_seen` session deduplication (`@TOON v1` `unchanged_since_tick:<N>`) invalidated automatically on `fs_write`/`fs_edit`.
  - `fs_list` — list directory entries up to `max_depth` (default `3`), ignoring symlinks and ignored directories.
  - `fs_write` — create or overwrite a UTF-8 file *(requires approval by default)*.
  - `fs_edit` — unique exact replacement (with line-trimmed fallback for whitespace/indentation) in a UTF-8 file *(requires approval by default)*.
  - `code_search` — search for a pattern across workspace files (ignoring symlinks and ignored directories).
  - `scent_outline` / `scent_impact` *(optional `feature = "scent"`)* — read-only AST outline and semantic call-impact tools backed by `onyx-scent` (`blake3` incremental cache + `@TOON v1` paginated output), validated through `WorkspacePolicy` without requiring human approval or creating mutating `SideEffectLedger` entries.
  - `shell_exec` — execute a shell command inside `root` with timeout, full process-tree cleanup, concurrent stdout/stderr draining, credential scrubbing (`scrub_secrets`), and output limits *(requires approval by default; `WorkspacePolicy` restricts filesystem tool paths, whereas `shell_exec` runs host OS processes under explicit approval)*.
  - `WorkspaceTools::schemas()` / `schemas_with_subagents(true)` export JSON parameter schemas suitable for LLM function calling (including optional `spawn_agent` delegation schema).
- `WorkspaceMode` & `WorkspaceLedgerSystem` (`state.rs`):
  - Enforces workspace modes (`Plan`, `Build`, `Explore`) and optional `require_read_before_edit` policy.
  - Tracks `File` nodes (`ws.file.path`, `ws.file.read_tick`, `ws.file.dirty`, `ws.file.write_count`) and the `SideEffectLedger` (`ExternalEffect` nodes with `ws.effect.*` reversibility and `effects_after_tick` query for two-phase `/rewind`).
  - Captures lightweight Git working-tree checkpoints (`capture_git_tree_checkpoint` via temporary `GIT_INDEX_FILE` + `git add -A` + `git write-tree`, without touching `HEAD` or the user's staging area) and restores them via `restore_git_tree_checkpoint` (`git read-tree --reset -u <tree_sha>`).
- `WorkspaceToolBridge`:
  - Implements `PlatformDriver` and wraps an inner driver (e.g., `HeadlessDriver`, `CliApprovalDriver`, or `TuiDriver`).
  - Enforces `WorkspaceMode`, `require_read_before_edit`, and pre-mutation `PendingCheckpoints` capture before executing `WorkspaceTools` actions and pushing `Reply::observe` or `Reply::tool_error` back into the `EventBus`.
  - Designed to sit inside `RecordingBridge::wrapping(...)` so tool observations are recorded into the `Transcript` and replayed deterministically without touching disk.

## Roadmap / TODO (Not Yet Implemented)

- **Streaming / cancellation for `shell_exec`**: allow incremental output capture and cancellation hooks from external adapters without blocking the brain.
