# 02. TUI & Daily Workflow

> 🇪🇸 **[Ver este capítulo en Español](02-interfaz-tui-y-flujo-diario.md)**

This chapter explains every visual component of `openonyx chat`, how to tune reasoning effort (`--effort`), how multi-turn memory and tiered context (`/compact`) work, and how to supervise delegated subagents with `F2` / `/agent`.

---

## 1. TUI Anatomy

Running `openonyx chat` organizes the terminal into four zones:

```text
┌─ openOnyx [BUILD] [SANDBOX:workspace] · gemini-2.5-pro (effort:high) ───────────────────┐
│ ⠋ phase: thinking · #1 root [root] · vault:20261121-103500_gemini_main                  │
│   Tab: Plan/Build · F2: Agents · /rewind · /fork · /services · Esc: quit                │
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

### Compact 3-Row Header
- **Top border**: displays `[BUILD]` / `[PLAN]` / `[EXPLORE]`, `[SANDBOX:workspace|strict|off]`, active model, and reasoning effort (`effort:low|medium|high|auto`).
- **Body rows**:
  - **Animated Braille emblem** (`6×3` sub-pixel canvas, throttled to 90 ms) rotating smoothly while work is in progress (`thinking`, `acting`, `waiting_children`) and resting when idle or awaiting approval.
  - **Focused agent & vault status**: agent ID, name, role, current phase, and — when session saving is active (`--auto-save` or `--snapshot-dir`) — the live `vault:<session>` badge (automatically updated when branching via `/fork` or `/rewind`).
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

## 5. Multi-Agent Workflow: Subagents, `F2` & `/agent`

The root agent can delegate independent subtasks via `spawn_agent`, entering `WaitingChildren` until child agents settle:

- **Safe default step cap**: subagents inherit a default cap of **`32` steps** (`DEFAULT_SUBAGENT_MAX_STEPS`, overridable via `max_steps` in `spawn_agent` JSON args) to prevent runaway loops in local models.
- **Live inspection with `F2`**: press **`F2`** anytime to open the **Agent Selector** popup showing every root/subagent ID, name, role, and state (`Up`/`Down`, `Enter` to focus its `Memory & Trace`, `Esc` to close).
- **Alternative shortcuts**:
  - **`/agent`** (or `/agents`): opens the selector popup.
  - **`/agent <id|name>`**: jumps directly to an agent (e.g. `/agent 1` or `/agent researcher`).
  - **`Ctrl+Tab`**: quick-cycles focused agents (terminal-dependent; `F2` and `/agent` work everywhere).

---

## 6. Quick Reference of Keys & Slash Commands (`chat`)

| Key / Command | Action |
| --- | --- |
| `Enter` | Submit goal, follow-up prompt, or slash command |
| `Tab` | Toggle `[BUILD]` ↔ `[PLAN]` in place |
| `F2` | Open/close interactive **Agent Selector** popup |
| `Ctrl+Tab` | Quick-cycle focused agent (terminal-dependent alias for `F2`) |
| `y` / `n` | Approve or deny a pending mutating tool diff |
| `Up`/`Down`/`PgUp`/`PgDn`/`Home`/`End`/`Ctrl+U`/`Ctrl+D` | Scroll `Memory & Trace` history |
| `Esc` / `Ctrl+C` | Close open popup or exit cleanly (flushing `exit` snapshot & transcript) |
| `/agent [id\|name]` | Open agent selector or focus specific agent |
| `/mode [plan\|build\|explore]` | Switch mode explicitly or toggle `PLAN` ↔ `BUILD` |
| `/undo` | Revert latest reversible Git checkpoint (also cleans newly created `fs_write` files) |
| `/rewind [tick]` | List milestones or rewind live to `<tick>` into a safe sibling fork |
| `/fork [name]` | Branch snapshots up to current tick into a sibling directory |
| `/compact [focus]` | Compact older turns while rescuing focus matches and discovered files |
| `/services` | Print `@TOON v1` status of services defined in `sprout.toml` |
| `/service <start\|stop\|logs> <id\|all> [tail]` | Control Sprout services directly from the TUI |
| `/new [goal]` | Prepare or immediately launch a fresh root agent |

---

[← 01. First Steps](01-first-steps.md) · [English Index](index-en.md) · [Next: 03. Modes, Permissions & OS Sandbox →](03-modes-permissions-and-sandbox.md)
