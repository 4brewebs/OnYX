# 05. Agent Tools, Scent, Sprout, Skills & Automation

> 🇪🇸 **[Ver este capítulo en Español](05-herramientas-scent-sprout-y-skills.md)**

This chapter catalogs all **23 tools** exposed to the agent in `openonyx` (19 base + `spawn_agent` + 4 async subagent tools in isolated Git worktrees), explains AST navigation (`onyx-scent`), covers local service & container orchestration (`sprout.toml`), shows how to customize behavior with `ONYX.md` / skills, and outlines headless automation (`openonyx run`).

---

## 1. Complete 23-Tool Catalog

### Read-Only & Coordination Tools (No Approval Required)

| Tool | Purpose | Key Notes |
| --- | --- | --- |
| `fs_read` | Read UTF-8 files (full or 1-based `start_line`/`end_line` range). | `64 KiB` limit. Deduplicates unchanged reads (`unchanged_since_tick`) and overlapping/subset ranges (`covered_by:start-end`). Emits actionable errors if pointed at binary OnYX artifacts (`ONYXTR1`/`2`, `ONYXPK1`, `ONYXAG`). |
| `fs_list` | List workspace directory tree up to `max_depth` (default `3`). | Skips external symlinks, `.git`, `target`, `node_modules`, `.onyx/vault/`, `*.tr`, `*.onyxpack`. |
| `glob_files` | Find workspace paths matching glob patterns (`*`, `**`, `?`, `[...]`, `{a,b}`). | Deterministic lexicographical order (or `sort_by_mtime`). Rejects path traversal. |
| `code_search` | Literal or `regex` text search across the workspace. | Supports `case_insensitive`, `include_glob`, `exclude_glob`, and `context_lines` (`0..=5`) with `scrub_secrets`. |
| `scent_outline` | Paginated `@TOON v1` AST map of symbols, visibility, and line ranges. | Supports `Rust`, `Python`, and `Java`; incremental `blake3` cache + per-session `seen_outlines` dedup invalidated on edits/shell/`/undo`; filters `public_only`, `exclude_tests`, `cursor`, `max_rows` (`1..=500`). |
| `scent_impact` | Semantic call-impact graph (integer thermal diffusion `0..=10_000` bp, `1..=6` hops). | Reveals callers/dependents before refactoring shared symbols. |
| `git_status` | Read-only `git status --short --branch`. | Preferred over `shell_exec` for Git inspection. |
| `git_diff` | Read-only `git diff --no-color` with optional `path`, `staged`, and `context_lines`. | Passes `path` strictly after `--`. |
| `load_skill` | On-demand Markdown protocol/guide loader. | Searches `.onyx/skills/`, `skills/`, `~/.onyx/skills/`, plus built-in `scent` and `rust` guides. |
| `plan_update` | Updates structured checklist (`pending`, `in_progress`, `completed`). | Persisted to `ws.plan.text` and rendered in the right-hand `Live Plan` panel. |
| `service_status` | Returns `@TOON v1` status of all `sprout.toml` services. | Also projected into the `Services (Sprout)` side panel. |
| `service_logs` | Reads recent lines from a Sprout service ring-buffer (up to 2,000 lines). | Lossy UTF-8 decoding + `scrub_secrets` filtering. |
| `spawn_agent` | Delegates an independent subtask to a child subagent (`WaitingChildren`). | Default `32`-step cap (overridable via `max_steps` JSON arg). |

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

---

## 2. Semantic AST Navigation with `onyx-scent`

Instead of burning tokens reading entire multi-thousand-line files, `openonyx` encourages a two-step workflow:
1. **`scent_outline`**: fetches symbol signatures, visibility, and exact line ranges in compact `@TOON v1` format (paginated via `cursor` / `next_cursor`). Repeated calls on unchanged files hit the per-session `seen_outlines` cache, which invalidates automatically after `fs_edit`, `fs_write`, `shell_exec`, or `/undo`.
2. **`scent_impact`** + **`fs_read(start_line, end_line)`**: checks the blast radius before editing public symbols, then reads only the exact lines required.

> **Currently supported languages in `onyx-scent`:** Rust (`.rs`), Python (`.py`), and Java (`.java`). For other languages, the agent uses `glob_files` + `code_search` v2 + `fs_read`.

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
