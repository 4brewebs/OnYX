# 05. Agent Tools, Scent, Sprout, Skills & Automation

> 🇪🇸 **[Ver este capítulo en Español](05-herramientas-scent-sprout-y-skills.md)**

This chapter catalogs all **24 tools** exposed to the agent in `openonyx` (19 base + `spawn_agent` + 4 async subagent tools in isolated Git worktrees), explains multi-language AST navigation (`onyx-scent` 2.0), covers local service & container orchestration (`sprout.toml`), shows how to customize behavior with `ONYX.md` / skills, and outlines headless automation (`openonyx run`) and multi-model benchmarking (`openonyx bench`).

---

## 1. Complete 24-Tool Catalog

### Read-Only & Coordination Tools (No Approval Required)

| Tool | Purpose | Key Notes |
| --- | --- | --- |
| `fs_read` | Read UTF-8 files (full or 1-based `start_line`/`end_line` range). | `64 KiB` limit. Deduplicates unchanged reads (`unchanged_since_tick`) and overlapping/subset ranges (`covered_by:start-end`). Emits actionable errors if pointed at binary OnYX artifacts (`ONYXTR1`/`2`, `ONYXPK1`, `ONYXAG`). |
| `fs_list` | List workspace directory tree up to `max_depth` (default `3`). | Skips external symlinks, `.git`, `target`, `node_modules`, `.onyx/vault/`, `*.tr`, `*.onyxpack`. |
| `glob_files` | Find workspace paths matching glob patterns (`*`, `**`, `?`, `[...]`, `{a,b}`). | Deterministic lexicographical order (or `sort_by_mtime`). Rejects path traversal. |
| `code_search` | Literal or `regex` text search across the workspace. | Supports `case_insensitive`, `include_glob`, `exclude_glob`, and `context_lines` (`0..=5`) with `scrub_secrets`. |
| `scent_outline` | Paginated `@TOON v1` AST map of symbols, visibility, and line ranges. | Supports `Rust`, `Python`, `Java`, `TypeScript` (`.ts/.tsx/.mts/.cts`), `JavaScript` (`.js/.jsx/.mjs/.cjs`), `Go`, and `C/C++` (`.c/.h/.cc/.cpp/.cxx/.hh/.hpp`); incremental `blake3` cache + persistent binary `.onyx/scent.idx` cache + per-session `seen_outlines` dedup invalidated on edits/shell/`/undo`; filters `public_only`, `exclude_tests`, `cursor`, `max_rows` (`1..=500`). |
| `scent_impact` | Semantic call-impact graph (integer thermal diffusion `0..=10_000` bp, `1..=6` hops). | Reveals callers/dependents before refactoring shared symbols. |
| `scent_references` | Resolves the exact AST definition of a symbol and its direct cross-references (callers, callees, implementors, type users) as `@TOON v1` tuples. | `target` (symbol name, `Type::method`, or `file::symbol`); optional `public_only`, `exclude_tests`, pagination `cursor`/`max_rows` (`1..=500`). |
| `git_status` | Read-only `git status --short --branch`. | Preferred over `shell_exec` for Git inspection. |
| `git_diff` | Read-only `git diff --no-color` with optional `path`, `staged`, and `context_lines`. | Passes `path` strictly after `--`. |
| `load_skill` | On-demand Markdown protocol/guide loader. | Searches `.onyx/skills/`, `skills/`, `~/.onyx/skills/`, plus built-in `scent` and `rust` guides. |
| `plan_update` | Updates structured checklist (`pending`, `in_progress`, `completed`). | Persisted to `ws.plan.text` and rendered in the right-hand `Live Plan` panel. |
| `service_status` | Returns `@TOON v1` status of all `sprout.toml` services. | Also projected into the `Services (Sprout)` side panel. |
| `service_logs` | Reads recent lines from a Sprout service ring-buffer (up to 2,000 lines). | Lossy UTF-8 decoding + `scrub_secrets` filtering. |
| `spawn_agent` | Delegates an independent subtask to a child subagent (`WaitingChildren`). | Default `32`-step cap (overridable via `max_steps` JSON arg). With `isolated: true` the child works in its own **isolated Git worktree** (`onyx/sub/agent-*`) and the root is untouched until the 3-way is applied (section 1.1). |

### Mutating Tools (Require Approval in `BUILD`; Blocked in `PLAN`/`EXPLORE`)

| Tool | Purpose | Key Notes |
| --- | --- | --- |
| `fs_write` | Create or overwrite a UTF-8 file. | Captures pre-mutation Git checkpoint, shows colorized diff preview, blocked on Control Plane RO. |
| `fs_edit` | Unique exact replacement (`old_text`/`new_text`) or atomic multi-hunk `edits: [...]`. | Requires prior `fs_read`. Line-trimmed fallback & relative re-indentation applied in memory before write. Captures Git checkpoint. |
| `shell_exec` | Run a shell command inside the workspace. | Configurable `--shell-timeout` (default `30s`), `env_clear()`, OS sandbox, RAII `ProcessJob`, `scrub_secrets`, `32 KiB` cap, invalidates `fs_read`/`scent_outline` caches. |
| `service_start` | Start one service (`id`) or full DAG (`all: true`) from `sprout.toml`. | Honors `depends_on` topological order and waits on real probes (`ready_port`, `ready_log`, `ready_cmd`). |
| `service_stop` | Stop one service (`id`) or all (`all: true`). | Clean teardown via `stop_cmd` / `ProcessJob`. |
| `service_exec` | Run a command inside a running container (`docker exec` / `nerdctl exec` / `podman exec`). | Applies `env_clear()`, timeout, and `scrub_secrets`. |

### Async Subagent Tools (Isolated Git Worktrees)

Besides `spawn_agent` (synchronous, same workspace), the agent can launch **async** jobs that run in isolated Git worktrees (`onyx/sub/<job_id>` branches) without blocking the root agent:

| Tool | Purpose | Key Notes |
| --- | --- | --- |
| `spawn_subagent` | Launch an async job in an isolated Git worktree. | `goal` required; `max_steps` default `32` (max `128`); optional `driver` (`native` by default, or an `[[external_drivers]]` id — an external driver requires approval or `allow spawn_subagent(<driver>)`, chapter 03). Parallelism capped by `--max-subagents <n>` (default `3`, or `ONYX_MAX_SUBAGENTS`). |
| `subagent_status` | Query async job status. | Optional `job_id`; shows status, branch, touched files, and summary. |
| `subagent_merge` | 3-way merge the job's worktree into the root workspace (*Scent-Heated Merge*). | `git merge-tree` **without moving `HEAD`** or the index. The only one of the 4 with `requires_approval: true`. Delta governance: `Deny` on Control Plane (blocks the whole merge) and `Ask` on `sprout.toml` / `.github/workflows/**` (chapter 03); external drivers force `Ask`. |
| `subagent_discard` | Cancel the job and remove its worktree and temp branch. | If the job is running, cancels the `ProcessJob` process tree. |

**TUI slash equivalents:** `/spawn [--driver <id>] <goal>` (launch), `/subagents` (list), `/merge <job_id>` (merge), and `/discard <job_id>` (discard). The full log of each job (job stdout/stderr, sealed through `scrub_secrets`) is stored in `.onyx/vault/subagent-logs/<job_id>.log`, and the `F2 Agents` view (chapter 02) lists the `[bg]` jobs with their status and `→ <driver>` badge.

> **External drivers:** with `driver = "opencode"` (or any declared `[[external_drivers]]`), the job is delegated to an external CLI harness working in the same isolated worktree; process isolation, the Trust gate, and the network policy are documented in chapter 03 (section 2.7).

### 1.1. `spawn_agent(isolated: true)` — synchronous children in isolated worktrees

Beyond the async jobs above, `spawn_agent` accepts the `isolated: true` argument: the **synchronous** child (the parent goes `WaitingChildren`) works in its own **isolated Git worktree**, so parallel edits by several children **never touch the root working tree** until merged.

- **Creation:** when a pending `spawn_agent(isolated: true)` is detected, the bridge lazily creates the `onyx/sub/agent-<parent>-<child>-<seq>` worktree via `WorktreeManager` (SDD-09) with `WorkspacePolicy::for_worktree` and `NativeOffline` networking; creation uses a `commit-tree` checkpoint of the current working tree, **without moving the root `HEAD` or index**. If the workspace is not a Git repo (or Git < 2.38), the parent receives a sticky, actionable error (root untouched).
- **Finalization:** when the child terminates (`Done`/`Failed`) or vanishes from the graph, the bridge finalizes it unconditionally: it captures the ephemeral commit of the child's final working tree, evaluates the **3-way** (`git merge-tree`, root intact), and records an `IsolatedWorktreeResult` (`Pending`/`Applied`/`Discarded`/`Transferred`).
- **Resolution:** with a clean 3-way the result is applied to the root (or discarded) **sequentially**; with **conflicts**, the worktree is transferred to `SubagentManager` and the work flows through the normal **`subagent_status` / `subagent_merge` / `subagent_discard`** path (same approval and governance as `spawn_subagent` jobs). Result notifications follow the async-job semantics (`last_completed_proposals` and, with `proactive_notifications`, an `IngressEvent::Command`).
- **Cleanup:** the worktree is removed on apply/discard, on the bridge `Drop`, and by the orphaned-worktree pruning in `openonyx doctor`; the temp branch `onyx/sub/<job_id>` is always deleted.

### Dynamic MCP Tools (`mcp__<server>__<tool>`)

On top of the fixed catalog above, every MCP server declared in `[mcp.servers.<name>]` (chapter 03, section 2.8) injects its tools **dynamically** into the LLM under the name `mcp__<server>__<tool>`:

- **Discovery at startup:** the `initialize` → `tools/list` handshake (paginated, max 16 pages) retrieves each tool's name, description, and `inputSchema`; schemas are normalized before being declared to the provider (Gemini, OpenAI, and Anthropic).
- **Always approval-gated:** `requires_approval = true` on every `mcp__*` tool: they ask for interactive approval in `BUILD` (unless a more specific `allow` rule) and are **`Deny` in `PLAN`/`EXPLORE`**, exactly like mutations.
- **Isolation & sanitization:** a dead server never aborts the session, and results pass through `truncate_utf8` + `scrub_secrets`.
- **0-I/O replay:** under `openonyx replay` the `mcp__*` tools are reconstructed from the transcript itself, without contacting any server.

> **Subagents do not receive `mcp__*` tools:** only the session's root agent sees the MCP registry (worktree jobs do not inherit it).

---

## 2. Semantic AST Navigation with `onyx-scent` (Scent 2.0)

Instead of burning tokens reading entire multi-thousand-line files, `openonyx` encourages a two-step workflow:
1. **`scent_outline`**: fetches function, struct, trait, class, method, *type alias* and *component* signatures, visibility, and exact line ranges in compact `@TOON v1` format (paginated via `cursor` / `next_cursor`). Repeated calls on unchanged files hit the per-session `seen_outlines` cache, which invalidates automatically after `fs_edit`, `fs_write`, `shell_exec`, or `/undo`.
2. **`scent_impact`** / **`scent_references`** + **`fs_read(start_line, end_line)`**: checks the blast radius (or resolves a symbol's exact definition and direct cross-references) before editing public symbols, then reads only the exact lines required.

> **Languages supported by `onyx-scent` (Scent 2.0):** **7 languages** — Rust (`.rs`), Python (`.py`), Java (`.java`), TypeScript (`.ts`, `.tsx`, `.mts`, `.cts`), JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`), Go (`.go`), and C/C++ (`.c`, `.h`, `.cc`, `.cpp`, `.cxx`, `.hh`, `.hpp`). In JS/TS it also extracts *type aliases* (`EntityKind::TypeAlias`) and **components** (`EntityKind::Component`: PascalCase functions with JSX in `.tsx`/`.jsx`), and `is_callable` includes both. For other languages, the agent uses `glob_files` + `code_search` v2 + `fs_read`.

### Incremental binary cache `.onyx/scent.idx`

- **`ONYXSCI1`**: the workspace AST index is persisted to **`.onyx/scent.idx`** (machinery `ONYXSCI1`: magic + version + entries sorted by `rel_path` + final `blake3` checksum) alongside a hygiene `.onyx/.gitignore` that ignores `scent.idx` and `scent.idx.tmp*`; any anomaly (truncated file, wrong magic/version, invalid checksum) is silently ignored and the index is rebuilt clean.
- **Incremental reload**: each refresh only re-parses files whose mtime/content changed (hash-verified), so repos with thousands of files update in tens of milliseconds; the file is written atomically (temp file + rename).
- **Excluded from Git artifacts**: subagent ephemeral checkpoints (`commit-tree`) and worktree impact evaluation **exclude** `.onyx/scent.idx*` and its `.gitignore`, so a child's work is never polluted by OnYX's own operational artifacts.
- **`scent` CLI**: the `onyx-scent` crate (`cli` feature) exposes the **`scent`** binary with `outline`, `impact`, and **`references`** subcommands (same `@TOON v1` output as the agent tools, for direct use in scripts/CI).

---

## 3. Local Process & Container Orchestration (`sprout.toml`)

If your project relies on local services (Docker/Rancher databases, backend APIs, workers, dev servers), add a **`sprout.toml`** at the workspace root.

`onyx-workspace` runs a pure-`std` supervisor (**0 `tokio`**) that validates the dependency DAG (rejecting cycles), starts services in topological order, checks real readiness probes, and tears down all child trees cleanly on exit via RAII `ProcessJob`.

### Modern Format (`[runtime]` + `[services.<id>]`)

```toml
[runtime]
# Optional: "docker" | "nerdctl" | "podman" (auto-detected if omitted)
container_cli = "docker"

[services.db]
name = "Local Postgres"
kind = "container"
cmd = "docker compose up -d postgres"
stop_cmd = "docker compose stop postgres"
logs_cmd = "docker compose logs --tail=100 postgres"
container = "myapp-postgres"
# Optional: run `service_exec` as a specific user (e.g. "1000:1000" or "app")
# exec_user = "1000:1000"
ready_port = 5432

[services.api]
name = "Backend API"
kind = "process"
cwd = "backend"
cmd = "cargo run --bin api"
depends_on = ["db"]
ready_port = 8080
ready_log = "Listening on 127.0.0.1:8080"
```

The legacy Sprout format (`[global]` + `[[service]]` with `env_wrapper`, `dir`, `debug_port`, `depends_on`, `optional`, `enabled`) is also supported.

### Interactive TUI Control
- `/services`: prints the `@TOON v1` service status table into `Memory & Trace`.
- `/service start <id|all>`: starts a service (plus dependencies) or the entire stack.
- `/service stop <id|all>`: stops a service or all services.
- `/service logs <id> [tail]`: prints recent lines from the service log buffer.
- On terminals `>= 100` columns wide, statuses appear live in the right-hand **`Services (Sprout)`** panel, and `openonyx doctor` audits `sprout.toml` syntax, DAG cycles, container runtime availability, and occupied ports.

> **Sandbox network, trust & container note:** if the agent needs `shell_exec` to hit a local Sprout endpoint (e.g. `http://127.0.0.1:8080/health`), launch with `--sandbox-net localhost` (or set `network = "localhost"` in `.onyx/config.toml`). In addition, when a workspace is not yet trusted (`openonyx trust add` or `/trust`), Sprout displays `[UNTRUSTED — run /trust]` and blocks `start`, `stop`, and `exec` until the current `sprout.toml` hash is approved (any 1-byte edit revokes trust automatically). `container_cli` strictly accepts `docker`, `nerdctl`, or `podman`, and optional `exec_user` runs `service_exec` as the specified user.

---

## 4. Project Instructions (`ONYX.md` / `AGENTS.md`)

On startup (`chat`, `run`, and live `rewind`), `openonyx` automatically loads Markdown rules in order (up to `16 KiB` total, filtered through `scrub_secrets`):

1. `~/.onyx/AGENTS.md` and `~/.onyx/ONYX.md` *(personal global rules)*
2. `<workspace>/AGENTS.md` and `<workspace>/ONYX.md` *(project rules)*

It also injects a host OS shell hint (Windows PowerShell vs Unix/POSIX) so the model emits valid commands for your machine.

Example `<workspace>/ONYX.md`:
```markdown
# Project Conventions
- Always run `cargo test --workspace` after editing Rust code.
- Never edit generated files under `dist/`.
- Keep public API errors concise and actionable.
```

---

## 5. Authoring Custom Skills (`SKILL.md`)

Rather than bloating `ONYX.md` on every turn, package domain checklists as **on-demand skills**. At startup `openonyx` injects only a compact summary (`discover_skills_summary`), and the agent loads the full text via `load_skill("<name>")` only when relevant.

### Skill Discovery Priority
1. `<workspace>/.onyx/skills/<name>/SKILL.md` *(recommended per-project location)* or `<name>.md`
2. `<workspace>/skills/<name>/SKILL.md` or `<name>.md`
3. `~/.onyx/skills/<name>/SKILL.md` or `<name>.md` *(global user skills)*
4. Built-in skills: `scent` and `rust`

### Example Custom Skill (`.onyx/skills/deploy-checks/SKILL.md`)
```markdown
---
name: deploy-checks
description: Pre-deploy verification checklist and safe SQL migration guidelines.
---

# Pre-Deploy Verification

1. Check `git_diff` for destructive migrations lacking a rollback path.
2. Verify `cargo clippy --workspace --all-targets -- -D warnings` passes cleanly.
3. Confirm no staging tokens or internal URLs remain in config files.
```

Best practices:
- Include `name:` and `description:` within the first 20 lines.
- Never include real secrets (all content is filtered through `scrub_secrets`).
- Keep files well below `64 KiB`.

---

## 6. Headless Automation & CI (`openonyx run`)

Run a single goal non-interactively with full vault recording:

```sh
# Read-only security/code audit
openonyx run "review git_diff and list potential security regressions" \
  --effort medium \
  --max-steps 32 \
  --auto-save

# Scoped automated fix with offline sandbox
openonyx run "fix clippy warnings and verify with cargo test" \
  --auto-approve \
  --sandbox workspace-write \
  --sandbox-net offline \
  --shell-timeout 120 \
  --auto-save
```

Even with `--auto-approve`, `[permissions].deny` rules in `.onyx/config.toml` and Control Plane RO protection remain enforced.

---

[← 04. Time-Travel, Snapshots & Forking](04-time-travel-snapshots-and-forking.md) · [English Index](index-en.md) · [Next: 06. CLI Reference & Troubleshooting →](06-cli-reference-and-troubleshooting.md)
