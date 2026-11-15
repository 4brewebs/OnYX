# onyx-workspace

**Workspace coding tools and path sandbox for harnesses built on `onyx-soul-agents`.**

Provides sandboxed filesystem, search, and shell capabilities that plug directly into `onyx-soul-agents` as a `PlatformDriver` bridge, preserving deterministic `Transcript` recording and diskless replay.

## Components

- `WorkspacePolicy`:
  - Canonicalizes `root` on disk and blocks path escapes (`..` or symlinks outside `root`) via `resolve_existing` and `resolve_for_write`.
  - Skips `ignored_dirs` by default (`.git`, `target`, `node_modules`).
  - Enforces default I/O limits (`max_read_bytes = 64 KiB`, `max_output_bytes = 32 KiB`, `command_timeout = 30s`).
- `WorkspaceTools` & `ToolSchema`:
  - `fs_read` — read UTF-8 files (optional 1-based `start_line` / `end_line` range).
  - `fs_list` — list directory entries up to `max_depth` (default `3`), skipping ignored directories.
  - `fs_write` — create or overwrite a UTF-8 file *(requires approval by default)*.
  - `fs_edit` — exact string replacement in a UTF-8 file *(requires approval by default)*.
  - `code_search` — search for a pattern across workspace files.
  - `shell_exec` — execute a shell command inside `root` with timeout and output limits *(requires approval by default)*.
  - `WorkspaceTools::schemas()` exports JSON parameter schemas suitable for LLM function calling.
- `WorkspaceToolBridge`:
  - Implements `PlatformDriver` and wraps an inner driver (e.g., `HeadlessDriver`, `CliApprovalDriver`, or `TuiDriver`).
  - Watches `pending_tool_calls` for `AgentState::Acting` agents, executes the matching `WorkspaceTools` action, and pushes `Reply::observe` or `Reply::tool_error` back into the `EventBus`.
  - Designed to sit inside `RecordingBridge::wrapping(...)` so tool observations are recorded into the `Transcript` and replayed deterministically without touching disk.
