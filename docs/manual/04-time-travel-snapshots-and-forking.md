# 04. Time-Travel, Snapshots & Forking

> 🇪🇸 **[Ver este capítulo en Español](04-time-travel-snapshots-y-forking.md)**

A defining capability of `openonyx` is that the agent's entire state lives in a deterministic EAV graph that can be snapshotted, inspected, replayed offline, rolled back on disk, and **forked without mutating the original timeline**.

---

## 1. Session Vault (`.onyx/vault/`) & `--auto-save`

To keep your repository root free of stray `.snaps_*` folders, fork siblings, and `.tr` files, `openonyx` centralizes session storage inside **`<workspace>/.onyx/vault/`**:

```text
<workspace>/.onyx/vault/
├── .gitignore                                  # created automatically with `*`
├── 20261121-103500_qwen3-coder_main/           # created by --auto-save
│   ├── snapshots.onyxpack
│   └── 20261121-103500_qwen3-coder_main.tr
├── 20261121-103500_qwen3-coder_main.fork-t12/  # branch created by /fork or /rewind
├── feat-auth/                                  # --snapshot-dir feat-auth --record chat.tr
│   ├── snapshots.onyxpack
│   └── chat.tr
└── unassigned/                                 # standalone --record solo.tr (no snapshot-dir)
    └── solo.tr
```

### Routing Rules
- **`--auto-save`** *(or `ONYX_AUTO_SAVE=1`)*: automatically creates `<workspace>/.onyx/vault/<YYYYMMDD-HHMMSS>_<model>[_<branch|goal>]/` storing both `snapshots.onyxpack` and `<id>.tr` together, shows `vault:<id>` in the TUI header, and prints the exact `openonyx chat --resume <id>` command on exit.
- **Short names (no `/` or `\` separators)**:
  - `--snapshot-dir feat-auth` → `.onyx/vault/feat-auth/`
  - `--snapshot-dir feat-auth --record chat.tr` → places `chat.tr` **inside** `.onyx/vault/feat-auth/chat.tr`.
  - `--record solo.tr` (without `--snapshot-dir`) → places it in `.onyx/vault/unassigned/solo.tr`.
- **Explicit paths preserved verbatim**: any absolute path or path containing `/` or `\` (`./my-dir`, `C:\tmp\snaps`, `../other`) is used as-is.
- **Transparent lookup on read**: `--resume`, `inspect`, `rewind`, `replay`, and `doctor` check literal paths first and automatically resolve short names inside `.onyx/vault/` (and `.onyx/vault/unassigned/` for `.tr` files).
- **Automatic Git & tool isolation**: `openonyx` writes `.onyx/vault/.gitignore` (`*`) automatically so `git status` and `/undo` Git checkpoints never index vault files, while `WorkspacePolicy` hides `.onyx/vault/` from `fs_list`/`glob_files`/`code_search` and protects it as read-only Control Plane.

---

## 2. How `snapshots.onyxpack` & Milestones Work

Whenever snapshot saving is enabled (`--auto-save` or `--snapshot-dir <dir>`), `MilestoneSnapshotSystem` captures snapshots only at **meaningful transitions**:

- `init` — initial session state.
- `spawn` — root agent or subagent creation.
- `approval:<tool>` — pending mutating tool approval.
- `tool:<tool>` / `tool_error:<tool>` — tool observation or error.
- `cmd:/agent`, `cmd:/undo`, `cmd:/rewind`, `cmd:/fork`, `cmd:/compact`, `cmd:/mode ...`, `cmd:/new` — slash commands.
- `settled`, `failed`, `exit` — turn completion or clean exit.
- `periodic` — optional periodic frames when `--snapshot-every <n>` is passed.

All milestones are stored in a single append-only compressed container **`<dir>/snapshots.onyxpack`** (`ONYXPK1` frames with zlib compression, canonical `ONYXAG\x01\0` graph payloads, and 64-bit FNV-1a checksums):

- **Crash tolerance**: truncated trailing bytes from interrupted writes are safely ignored on read and healed automatically on the next append.
- **Legacy compatibility**: older directories containing `tick-*.bin` files remain readable via transparent fallback.
- **Optional retention**: full history is kept by default so every tick stays rewindable; pass `--max-snapshots <n>` for opt-in sliding-window retention (keeps `init` + latest `n - 1` milestones).

---

## 3. Inspecting Sessions (`openonyx inspect`)

Audit any session with zero API keys and zero disk mutations:

```sh
# List all milestones: tick, tag, compressed/raw size, FNV-1a checksum, and Git tree_sha
openonyx inspect feat-auth --list

# Machine-readable JSON output
openonyx inspect feat-auth --list --json

# Inspect the latest milestone or a specific tick
openonyx inspect feat-auth
openonyx inspect feat-auth --tick 24
openonyx inspect feat-auth --tick 24 --json
```

---

## 4. Step-by-Step Rollback (`/undo`)

Before every `fs_write` or `fs_edit` in `BUILD` mode, `WorkspaceToolBridge` captures a lightweight **Git tree checkpoint** using a temporary `GIT_INDEX_FILE` (`git add -A` + `git write-tree`):

- **Never touches `HEAD` or your commits.**
- **Never alters your staging index (`git add`).**
- Works even in freshly initialized repositories with an unborn `HEAD`.

Running **`/undo`** in `openonyx chat`:
1. Marks the latest reversible effect as undone in the EAV graph (`ws.effect.undone`).
2. Restores the exact pre-mutation `tree_sha` on disk (`restore_git_tree_checkpoint_for_targets`).
3. **Safe cleanup of newly created files**: if the undone effect was an `fs_write` that created a new untracked file, `/undo` deletes it cleanly without touching unrelated untracked user files.
4. Invalidates session caches (`fs_read` and `scent_outline`) so the agent sees the restored file contents.

---

## 5. Live In-Session Forking & Rewind (Inside the TUI)

When running `chat` with `--auto-save`, `--snapshot-dir`, or `--resume`, you can branch and rewind **without leaving the TUI or overwriting the original timeline**:

### `/fork [name]` — Branch from Current Tick
```text
/fork parser-experiment
# or omit name (creates <dir>.fork-t<tick>):
/fork
```
- Copies all milestones up to the current tick into a sibling directory inside `.onyx/vault/`.
- Switches the live `SharedPackStore` to the new branch and updates the `vault:<session>` badge in the header.
- Leaves the original `snapshots.onyxpack` **100% untouched**.

### `/rewind [tick]` — Rewind Live Inside the TUI
```text
# 1. List available milestones with ticks, tags, and Git checkpoints
/rewind

# 2. Rewind to tick 18 (or nearest prior milestone <= 18)
/rewind 18
```
- **Active-work guard**: if any agent is busy or an LLM request is in flight, `/rewind` refuses to run until settled.
- **Non-destructive by default**: automatically forks into `<dir>.fork-t18`, restores the graph to the target tick, reconciles orphaned disk effects (restoring Git tree + cleaning post-tick `fs_write` files), and prints a summary in `Memory & Trace`.

---

## 6. CLI Time-Travel & Forking

### Resume from Latest Milestone
```sh
openonyx chat --resume feat-auth
```

### Resume from a Historical Tick into a Safe Fork
```sh
# Creates .onyx/vault/feat-auth.fork-t12 and opens the TUI at tick 12 (original preserved)
openonyx chat --resume feat-auth --tick 12

# Specify a custom fork name inside .onyx/vault/
openonyx chat --resume feat-auth --tick 12 --fork-dir feat-auth-alt

# Equivalent via rewind --interactive / --tui
openonyx rewind feat-auth 12 --interactive
openonyx rewind feat-auth 12 --tui --fork-dir feat-auth-alt
```

### Opt-In In-Place Truncation (`--truncate-future`)
To intentionally overwrite the original pack discarding ticks after `<tick>` instead of creating a sibling fork:

```sh
openonyx chat --resume feat-auth --tick 12 --truncate-future
```

### Headless Rewind with `--dry-run` & Transcript Replay
```sh
# 1. Preview orphaned effects and Git rollback plan without touching disk
openonyx rewind feat-auth 12 --dry-run

# 2. Rewind and replay deterministically from a transcript (0 HTTP calls)
openonyx rewind feat-auth 12 --transcript chat.tr
```

---

## 7. Transcripts & 100% Offline Replay (`ONYXTR2`)

Recording with `--auto-save` or `--record <file>` writes all LLM decisions, structured tool calls (`call_id`), slash commands (`Tab`, `/mode`, `/undo`, `/compact`, etc.), and checkpoint markers (`[checkpoint:<head>:<tree>]`). The time debugger's 0-token primitives are **sealed** in the transcript as `/__onyx_det_*` (shell `!cmd`, `/read`, `/inject`, `/step`, `/step-llm`, and `/graph` surgery): replay reproduces them **bit-for-bit with 0 I/O and 0 network** (no command is re-executed).

```sh
# Record a session in the vault
openonyx chat --auto-save

# Replay offline later (0 network calls, 0 API key, 0 disk writes)
openonyx replay 20261121-103500_qwen3-coder_main.tr --snapshot-dir replay-check
```

---

## 8. Health Audit (`openonyx doctor`)

After a crash, network interruption, or before a delicate rewind, run:

```sh
openonyx doctor --snapshot-dir feat-auth --transcript chat.tr
```

`doctor` verifies:
1. Git worktree health and any stray root `*.tr` / `snaps*` artifacts.
2. Session vault status (`.onyx/vault/` sessions, `unassigned/` transcripts, and `.onyx/vault/.gitignore`).
3. `snapshots.onyxpack` integrity (`ONYXPK1` header, FNV-1a checksums, `ONYXAG` graph payloads, truncated-tail detection) and legacy `tick-*.bin` files.
4. Validity of Git `tree_sha` objects referenced in the side-effect ledger.
5. Transcript header and parse integrity (`ONYXTR1` / `ONYXTR2`).
6. Host OS sandbox driver (`bwrap`/`sandbox-exec`/`win32-job`) and OCI container CLI.
7. `sprout.toml` syntax, acyclic DAG, container runtime, and occupied ports (if present).

---

## 9. Web Parity: the Time Debugger in `openonyx serve`

The embedded PWA has **full parity** with the TUI time debugger (SDD-07 Phase 4A): the same session state feeds both surfaces, so what you do from the browser affects the live session and vice versa.

- **Tick pill**: the header shows `#tick-pill` (an accessible button). Live it reads `tick N`; under freeze or with the cursor anchored in the past it reads `⏸ X/Y` and **auto-opens** the **amber Time Debugger bar** (you can close it; it reopens on the next freeze).
- **Amber bar**: status (`⏸ TIME FROZEN · tick X / present Y` / `LIVE · tick Y`), a slider that **snaps to the nearest available tick** (purged/future ticks are rejected with an actionable notice), ◀/▶ buttons, `Freeze`/`Present`, `Step`, `Step 1 LLM`, and `Resume` — the last three go through the **sealed command tunnel** (`/step 1`, `/step-llm`, `/go`), not the scrub API: the branch is applied in the session with its gates.
- **Ephemeral API**: `POST /api/v1/time/scrub` (JSON: `action: freeze|unfreeze|return_to_present|scrub_to|scrub_delta`) mutates only the debugger's RAM state — **0 disk, 0 bus events** — and replies with the updated `time_debug` snapshot. It inherits the Bearer auth and CSRF guard of every `/api/v1/*` endpoint (401 without credentials, 403 cross-site, actionable 400 for future/purged ticks or unknown actions).
- **Plain prompts under freeze**: `POST /api/v1/command` answers **`409 Conflict`** with an actionable message (`Paused in TIME FROZEN: use /go to resume or /inject to stage memory…`); `/go`, `/graph …`, `/inject …`, `/read …`, `/step …`, `/step-llm`, `/time` and `!cmd` keep working.
- **Memory surgery in the UI**: with the root agent frozen, each root memory exposes `✎` (edit via a dialog → flat `/graph edit-mem <seq> <text>`) and `×` (delete with confirmation → `/graph drop-mem <seq>`).
- **Projection preview & revert notice**: the Web snapshot carries the full `time_debug` block (`frozen`, `cursor_tick`, `present_tick`, `available_ticks`, `pending_revert_files` from the live graph's effects, 0 I/O) and, with `--snapshot-dir` + a real LLM target, the **`bench_hint`**: a copy-paste `openonyx bench --snapshot-dir … --tick … --models provider:model` command to compare the live model against a deterministic benchmark from the same cursor (SDD-05 §9.4 synergy).

> **Non-goal**: **Sprout** and **MCP server** effects are external process side effects: TimeFreeze (TUI or Web) **does not rewind** them.

---

[← 03. Modes, Permissions & OS Sandbox](03-modes-permissions-and-sandbox.md) · [English Index](index-en.md) · [Next: 05. Tools, Scent, Sprout & Skills →](05-tools-scent-sprout-and-skills.md)
