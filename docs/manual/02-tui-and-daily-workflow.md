# 02. TUI & Daily Workflow

> 🇪🇸 **[Ver este capítulo en Español](02-interfaz-tui-y-flujo-diario.md)**

This chapter explains every visual component of `openonyx chat`, how to tune reasoning effort (`--effort`) and hot-swap the LLM target, how multi-turn memory and tiered context (`/compact`) work, how to navigate the five ephemeral `F1..F5` views (with the `Ctrl+K` palette and `/view`), and how to supervise subagents from the `F2 Agents` view (or the `/agent` picker).

---

## 1. TUI Anatomy

Running `openonyx chat` organizes the terminal into four zones:

```text
┌ F1 Chat · F2 Agents · F3 Sprout · F4 Plan & Diff · F5 Timeline · Esc:Quit ──────────────┐
│ openOnyx [BUILD] [SANDBOX:workspace] · gemini/gemini-2.5-pro (effort:high)               │
│ goal:  Refactor the parser… · phase: thinking / streaming… · #1 [root] [subagents:1]    │
│ ⠋ Tab: Plan/Build · F2: Agents · /rewind · /fork · /services · Esc: quit                 │
└─ ctx: 24% · in: 18.4k (cache 12.1k) · out: 1.9k · think: 0.6k · ~$0.024 ────────────────┘
┌─ Memory & Trace (Up/Down/PgUp/PgDn/Home/End) ──────────┐┌─ Live Plan ───────────────────┐
│ [user] Refactor the parser...                          ││ [x] Inspect AST               │
│ [call] scent_outline {"path":"src/parser.rs"}          ││ [>] Edit parser.rs            │
│ [obs]  @TOON v1 ...                                    ││ [ ] Run cargo test            │
│ [stream] Updating range validation...▍                 │├───────────────────────────────┤
│                                                        ││ Services (Sprout)             │
│                                                        ││ [READY] postgres (5432)       │
│                                                        ││ [READY] api      (8080)       │
└────────────────────────────────────────────────────────┘└───────────────────────────────┘
┌─ [BUILD] Goal / Command (Enter send · Tab Plan/Build · F2 Agents · /rewind · /fork …) ──┐
│ > _                                                                                     │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

### Compact Header (Tabs + Title + Body + Telemetry)
- **Tab strip (top row, right-aligned)**: `F1 Chat · F2 Agents · F3 Sprout · F4 Plan & Diff · F5 Timeline`, with the active view in bold white. It ends with `Esc:Quit` in `F1 Chat` or `Esc:Back` in the ephemeral views; when an approval is pending **outside** `F1 Chat`, it is replaced by the red **[F1 Chat !APPROVAL]** badge (blind approval is forbidden: `y`/`n` never approve or deny in the other views).
- **Title row**: `openOnyx [BUILD] [SANDBOX:workspace] · gemini/gemini-2.5-pro (effort:high)` — mode badge (`[BUILD]` / `[PLAN]` / `[EXPLORE]`), sandbox badge (`[SANDBOX:workspace|strict|off]`), the `[UNTRUSTED]` tag when the workspace is untrusted, and the **live** LLM target `provider/model (effort:low|medium|high|auto)`, hot-swapped by `/profile` / `/provider` / `/model` / `/effort` (section 2.1).
- **Body rows**:
  - **Animated Braille emblem** (`6×3` sub-pixel canvas, throttled to 90 ms) rotating smoothly while work is in progress (`thinking`, `acting`, `waiting_children`) and resting when idle or awaiting approval.
  - **Goal & phase**: the focused agent's `goal:` and `phase:` with its ID/name/role (`#1 openonyx-1 [root]`), the `[subagents:N · bg:M · F2]` (or `[N/M agents · F2]`) and `[queued:N]` tags, and — when session saving is active (`--auto-save` or `--snapshot-dir`) — the live `vault:<session>` badge (automatically updated when branching via `/fork` or `/rewind`).
  - **Agent strip or quick hints**: when multiple agents exist, renders a compact single-line agent strip; otherwise shows quick hints.
- **Bottom border (Live Telemetry)**:
  - Approximate context window fill (`ctx: <1%` or `XX%`, plus `[compacted]` after `/compact`).
  - Cumulative provider counters: `in`, `cache` (prompt cache hits), `out`, and `think` (reasoning tokens when reported).
  - Estimated USD cost (`~$0.024`) for known cloud models, or `local` when using a custom `--api-base`.
  - Telemetry lives outside the EAV graph so it **never perturbs deterministic ticks, snapshots, or replay**.

### `Memory & Trace` Pane & History Navigation
- Displays the full trace of the focused agent: user prompts, tool calls (`[call]`), observations (`[obs]`), model replies, system notes (`[system]`), and live SSE previews (`[stream] ...▍`, rendered in `pump_idle` without advancing ticks).
- **Smooth scrolling**: use `Up`, `Down`, `PgUp`, `PgDn`, `Home`, `End`, `Ctrl+U`, and `Ctrl+D` even while an approval box is open. Scrolling up shows `[SCROLLED -N lines | End to follow]`; pressing `End` or submitting a prompt snaps back to auto-follow.

### Automatic Side Panels (`>= 100` Columns)
When terminal width is at least **100 columns**, the center area splits `70%` (`Memory & Trace`) / `30%` (right side):
- **`Live Plan`**: opens automatically when `plan_update` records steps (`[x]` done, `[>]` in progress, `[ ]` pending).
- **`Services (Sprout)`**: opens when a valid `sprout.toml` exists in the workspace, showing color-coded service states (`[READY]`, `[STARTING]`, `[PENDING]`, `[FAILED]`, `[OFF]`).
- When both are active, the right pane splits vertically `50%` / `50%`.

### Approval Box with Colorized Unified Diff (`y` / `n`)
Whenever an agent requests a mutating tool (`fs_write`, `fs_edit`, `shell_exec`, `service_start`, `service_stop`, `service_exec`) requiring confirmation:
- The banner identifies the requesting agent explicitly: **`[agent #id name]`**.
- For `fs_write` and `fs_edit` (including atomic multi-hunk edits), a **colorized unified diff preview** is rendered before anything touches disk.
- Press **`y`** to approve or **`n`** to deny.
- **Only from `F1 Chat`**: in the ephemeral `F2..F5` views, `y`/`n` **never** approve or deny; the tab strip shows the `[F1 Chat !APPROVAL]` badge to remind you to return to `F1` and review the diff and the `# impact:` line.

---

## 2. Tuning Reasoning Effort (`--effort`)

Control how much internal reasoning budget the model spends:

| Value | Gemini (`thinkingLevel`) | OpenAI-compatible (`reasoning_effort`) | Recommended Use |
| --- | --- | --- | --- |
| `low` | `LOW` | `low` | Quick lookups, read-only tasks, lightweight local models |
| `medium` | `MEDIUM` | `medium` | Everyday editing, debugging, and refactors |
| `high` | `HIGH` | `high` | Architecture design, subtle bugs, deep audits |

Set via flag or environment variable (precedence: `--effort` > `ONYX_EFFORT` > `GEMINI_THINKING_LEVEL` / `OPENAI_REASONING_EFFORT`):

```sh
openonyx chat --effort high --auto-save
# or via environment variable:
export ONYX_EFFORT=medium
```

> **For `anthropic`:** the effort maps to an extended-thinking budget (`thinking.budget_tokens`: `low` 2048 / `medium` 8192 / `high` 16384 tokens) when no explicit budget is set. You can pin it with `--thinking-budget <n>` (or `ANTHROPIC_THINKING_BUDGET`; `0` = off, range `1024..=60000`), which also raises `max_tokens` above the budget. The provider also supports prompt caching (`cache_control: ephemeral` on the system block and the last tool definition) and automatic retries on `429`/`529` (chapter 06).

### 2.1 Hot-Swapping the LLM Target (`/profile`, `/provider`, `/model`, `/effort`)

Beyond effort, the full LLM target (provider, model, `api_base`, and effort) is **hot-swappable** without restarting the session:

- **Profiles**: `[[llm_profiles]]` blocks in `.onyx/config.toml` (trusted workspace) with `id`, `provider` (`gemini` | `openai` | `anthropic`), `model`, `effort`, optional `api_base`, and optional `api_key_env`. **Never** literal keys in the file (details in chapter 03).
- **Slash commands**: `/profile [id]` lists or applies a profile, `/provider <gemini|openai|anthropic>`, `/model <name>`, and `/effort <auto|low|medium|high>`. Changes apply at the next tick boundary and are sealed in the `ONYXTR2` transcript (deterministic replay).
- **Live header**: the title row switches to showing `provider/model (effort:...)` of the active target instead of the startup one.
- **`Ctrl+K` palette**: a typing-free alternative — filter profiles, providers, and efforts, then run with `Enter` (LLM actions go through the deterministic bus exactly as if typed; a profile without credentials is shown as `unavailable` with its reason and is not selectable).
- **Web UI (`openonyx serve`)**: the *LLM target* button (brand subtitle) opens a modal dialog with profile/model/effort and Apply/Cancel buttons, without exposing credentials to the browser.

> Credentials **never** live in the config: each profile resolves its key via `api_key_env` (or the provider's standard variable → `ONYX_API_KEY`). A profile missing its key or with a rejected `api_base` is listed as `unavailable` with the actionable reason.

---

## 3. Multi-Turn Conversations, Active-Root Guard & `/new`

### Automatic Continuation (`continue_agent`)
Once a turn finishes (`Settled`), submitting a follow-up prompt **reactivates the same root agent** via `continue_agent`, preserving its entire `rel_memory` with monotonic step counts rather than spawning an amnesic agent.

### Follow-Up Prompt Queue While Busy (`queued:N`)
While the root agent is actively working (`thinking`, `acting`, `waiting_children`, or `waiting_approval`), you can type your next follow-up and press `Enter`: instead of racing a second root agent over the workspace, the TUI **queues it automatically** (`queued:N` shown in the header and input border, previewed when the input is empty, and clearable anytime with **`Ctrl+X`**) and dispatches it the moment the current turn settles. Immediate commands (`/agent`, `/mode`, `/services`, `/service`, `/undo`) still execute right away.

### `/` & `@file` Autocomplete, Cursor Editing & Input History
- **Visible UTF-8 cursor**: edit anywhere inside the prompt using `Left`, `Right`, `Home`, `End`, `Backspace`, and `Delete`.
- **Prompt history**: cycle previous prompts with **`Alt+Up` / `Alt+Down`** (or **`Ctrl+P` / `Ctrl+N`**), restoring your unsent draft when returning to the bottom.
- **`/` popup**: typing `/` at the start of the line opens a floating command menu with short descriptions (`↑`/`↓`, `Tab`/`Enter` to accept, `Esc` to dismiss).
- **`@` popup**: typing `@` at the start of a word (e.g. `review @tui`) fuzzy-filters cached workspace paths (respecting ignored directories and `.onyx/vault`) and inserts the clean relative path on `Tab`/`Enter`.

### Starting Fresh (`/new`)
- `/new`: marks the next prompt to spawn a clean root agent with fresh memory.
- `/new <goal>`: immediately spawns a fresh root agent with `<goal>` and auto-focuses it in the TUI.

---

## 4. Tiered Context & `/compact [focus]`

To prevent long sessions from overflowing context windows or burning unnecessary tokens, `openonyx` uses a shared **Tier 0 / Tier 1 / Tier 2 context engine** across both Gemini and OpenAI:

- **Tier 0 (Full)**: the latest 10 tool observations are kept verbatim.
- **Tier 1 (Coordinate-Preserving Summaries)**: older outputs from `fs_read`, `scent_outline`, `scent_impact`, `code_search`, `glob_files`, and `shell_exec` are deterministically summarized while keeping paths, line ranges, and exit codes, maintaining cache-stable prefixes for prompt caching.
- **Tier 2 (`/compact` & Auto-Compaction)**: upon reaching the auto-compact threshold (`80%` of context budget) or running `/compact [focus]`, `CommandSpawnerSystem` inserts a verified `[session_summary]` boundary.

```text
/compact
/compact jwt authentication and middleware
```

Providing a `[focus]`:
1. Rescues `files_found` and keyword-matched `focus_matches` from earlier turns.
2. Drops active `ctx:` fill immediately on the next turn.
3. Records a `cmd:/compact` milestone in `snapshots.onyxpack`.
4. **Loses nothing**: pre-compact ticks remain intact in the graph and `snapshots.onyxpack` for `inspect`, `replay`, and `rewind`.

---

## 5. Modular `F1..F5` Views, `Ctrl+K` Palette & `/view`

Besides `F1 Chat` (trace, side panels, input, approvals), the TUI exposes four ephemeral views you can jump to with the function key, `/view <name>`, or the `Ctrl+K` palette:

| View | Key | Slash | Contents |
| --- | --- | --- | --- |
| `F1 Chat` | `F1` | `/view chat` | `Memory & Trace`, `Live Plan` / `Services (Sprout)` panels, input and approvals |
| `F2 Agents` | `F2` | `/view agents` | Graph agents + async `[bg]` jobs (with a `→ <driver>` badge when an external CLI harness runs the worktree) |
| `F3 Sprout` | `F3` | `/view sprout` | `sprout.toml` services (status badges) + live logs of the selected service |
| `F4 Plan & Diff` | `F4` | `/view plan` | Active plan (`ws.plan.text`) on top + Git effects ledger (tick, tool, target, reversibility, `(undone)`) below |
| `F5 Timeline` | `F5` | `/view timeline` | `snapshots.onyxpack` milestones (tick, tag, size, checksum) for `/rewind` / `/fork` |

- **100% ephemeral navigation**: switching views writes **nothing** to the graph, emits no bus events, and advances no ticks — it is just a different "lens" over the same deterministic state.
- **`F2 Agents`**: `Up`/`Down` select (the focused agent carries the `*` marker), `Enter` focuses that agent's `Memory & Trace` and returns to `F1 Chat`, `Esc` returns to `F1 Chat`.
- **`F3 Sprout`**: `Up`/`Down`/`PgUp`/`PgDn` navigate services; `s`/`x`/`r`/`a` (start/stop/restart/all) and `f` (log follow) are **gated**: in `PLAN`/`EXPLORE` mode or with an `[UNTRUSTED]` workspace the view header shows `MUTATIONS BLOCKED` and the action is rejected with a notice.
- **`F4 Plan & Diff`**: `Up`/`Down` scroll the Git effects ledger.
- **`F5 Timeline`**: `Up`/`Down` select a milestone, `Enter` runs `/rewind <tick>`, `f` runs `/fork`.
- **`Esc`**: closes popups or the palette if open; in `F1 Chat` it quits cleanly; in `F2..F5` it returns to `F1 Chat` (the tab strip label reads `Esc:Quit` or `Esc:Back`).
- **`/view <chat|agents|sprout|plan|timeline>`** (case-insensitive): intercepted at `Enter` at the TUI level — 0 bus events, 0 ticks, 0 graph writes. `/view` with no argument shows usage; an unknown name shows a transient notice.
- **`Ctrl+K` palette**: centered popup (`Ctrl+K Palette — type to filter`) with three groups — **VIEW** (`F1..F5`), **LLM** (`[[llm_profiles]]` profiles, providers, and efforts) and **AGENT** (by ID/name). Type to filter (every printable character filters — `j`/`k` included, they never navigate), `Up`/`Down` (or `Ctrl+P`/`Ctrl+N`) select, `Enter` executes, `Esc`/`Ctrl+K` close. It has top priority (except `Ctrl+C`) and swallows every key while open; LLM actions are sent through the deterministic bus and sealed in `ONYXTR2`, exactly as if typed.

---

## 6. Multi-Agent Workflow: Subagents, `F2 Agents` View & `/agent`

The root agent can delegate subtasks in two ways: **synchronous** graph subagents (`spawn_agent`, sharing the workspace and awaited by the parent) and **asynchronous** jobs in isolated Git worktrees (`spawn_subagent` / `/spawn`, chapter 05).

- **Safe default step cap**: synchronous subagents inherit a default cap of **`32` steps** (`DEFAULT_SUBAGENT_MAX_STEPS`, overridable via `max_steps` in `spawn_agent` JSON args) to prevent runaway loops in local models. While children are active, the parent enters `WaitingChildren`.
- **Live inspection with the `F2 Agents` view**: press **`F2`** anytime to jump to the full-screen **Agents** view:
  - Lists every graph agent (root and subagents) with ID, name, role (`root`/`sub`), current state, and goal.
  - Also lists async `[bg]` jobs with their status (`running`, `done`, `merged`, `failed`), worktree branch, and — when an external harness runs the job — a `→ <driver>` badge.
  - **`Up`** / **`Down`** select, **`Enter`** focuses the agent's `Memory & Trace` (back to `F1 Chat`), **`Esc`** returns to `F1 Chat`.
- **Alternative shortcuts**:
  - **`/agent`** (or `/agents`): opens the centered **Agent Selector** popup (or cycles to the next agent when only one exists).
  - **`/agent <id|name>`**: jumps directly to an agent (e.g. `/agent 1` or `/agent researcher`).
  - **`Ctrl+K`**: the palette also lists agents; `Enter` focuses and returns to `F1 Chat`.
  - **`Ctrl+Tab`**: quick-cycles focused agents (terminal-dependent alias; `F2`, `/agent`, and the palette work everywhere).
- **External drivers from the Web UI**: in `openonyx serve`, the *Subagent driver* selector picks a declared `[[external_drivers]]`; sending free text with a non-native driver makes the browser emit `/spawn --driver <id> <text>` (chapter 05).

---

## 7. Quick Reference of Keys & Slash Commands (`chat`)

| Key / Command | Action |
| --- | --- |
| `Enter` | Submit goal, follow-up prompt, or slash command |
| `Tab` | Toggle `[BUILD]` ↔ `[PLAN]` in place |
| `F1..F5` | Switch ephemeral view (Chat, Agents, Sprout, Plan & Diff, Timeline) |
| `Ctrl+K` | Open the command palette (views + LLM slash commands + agents); type to filter, `Enter` executes |
| `Ctrl+Tab` | Quick-cycle focused agent (terminal-dependent alias for `F2` / `/agent`) |
| `y` / `n` | Approve or deny a pending mutating tool diff (**only in `F1 Chat`**; other views hint to press `F1`) |
| `Up`/`Down`/`PgUp`/`PgDn`/`Home`/`End`/`Ctrl+U`/`Ctrl+D` | Scroll `Memory & Trace` history |
| `Esc` | Close popup/palette; in `F2..F5` back to `F1 Chat`; in `F1 Chat` exit cleanly (flushing snapshot & transcript) |
| `Ctrl+C` | Exit cleanly (flushing `exit` snapshot & transcript, if configured) |
| `/view <chat\|agents\|sprout\|plan\|timeline>` | Switch views from the prompt (ephemeral: 0 bus events, 0 ticks, 0 graph writes) |
| `/agent [id\|name]` | Open agent selector or focus specific agent |
| `/mode [plan\|build\|explore]` | Switch mode explicitly or toggle `PLAN` ↔ `BUILD` |
| `/undo` | Revert latest reversible Git checkpoint (also cleans newly created `fs_write` files) |
| `/rewind [tick]` | List milestones or rewind live to `<tick>` into a safe sibling fork |
| `/fork [name]` | Branch snapshots up to current tick into a sibling directory |
| `/compact [focus]` | Compact older turns while rescuing focus matches and discovered files |
| `/services` | Print `@TOON v1` status of services defined in `sprout.toml` |
| `/service <start\|stop\|logs> <id\|all> [tail]` | Control Sprout services directly from the TUI |
| `/new [goal]` | Prepare or immediately launch a fresh root agent |
| `/profile [id]`, `/provider <gemini\|openai>`, `/model <name>`, `/effort <auto\|low\|medium\|high>` | Hot-swap the LLM target (section 2.1) |
| `/spawn [--driver <id>] <goal>` | Launch an async job in an isolated Git worktree (chapter 05) |
| `/subagents` | List async `[bg]` jobs (status, branch, touched files) |
| `/merge <job_id>` | 3-way merge the job's worktree (`Ask`/`Deny` governance, chapter 03) |
| `/discard <job_id>` | Cancel the job and remove its worktree and temp branch |

---

[← 01. First Steps](01-first-steps.md) · [English Index](index-en.md) · [Next: 03. Modes, Permissions & OS Sandbox →](03-modes-permissions-and-sandbox.md)
