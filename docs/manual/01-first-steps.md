# 01. First Steps with openOnyx (Quickstart)

Go from building `openonyx` to completing your first supervised coding session — with live SSE streaming, colorized diff approvals, `PLAN`/`BUILD` mode toggling, and transactional Git `/undo` — in under 10 minutes.

---

## 1. Build or Install `openonyx`

From the OnYX repository root:

```sh
# Optimized build (recommended for daily use)
cargo build --release -p open-onyx

# Or install into ~/.cargo/bin so `openonyx` is available in any project
cargo install --path open-onyx
```

Verify the binary and audit your local environment:

```sh
openonyx --version
openonyx --help
openonyx doctor
```

> **Tip:** If you did not install globally, replace `openonyx` with `cargo run --release -p open-onyx --` or `./target/release/openonyx` (Windows: `.\target\release\openonyx.exe`).

---

## 2. Configure an LLM Provider

`openonyx` natively supports **Google Gemini** (default) and any **OpenAI-compatible `/v1/chat/completions`** endpoint (OpenAI, OpenRouter, Ollama, vLLM, LM Studio).

### Option A — Google Gemini (default)

**Windows (PowerShell):**
```powershell
$env:GEMINI_API_KEY = "your-api-key"
openonyx chat --snapshot-dir .onyx_snaps --record session.tr
```

**Linux / macOS (bash/zsh):**
```sh
export GEMINI_API_KEY="your-api-key"
openonyx chat --snapshot-dir .onyx_snaps --record session.tr
```

To pick a specific model or reasoning effort (`low`, `medium`, `high`):
```sh
openonyx chat --model gemini-2.5-pro --effort high --snapshot-dir .onyx_snaps
```

### Option B — OpenAI / OpenRouter

**Windows (PowerShell):**
```powershell
$env:OPENAI_API_KEY = "sk-..."
openonyx chat --provider openai --model gpt-4o --effort medium --snapshot-dir .onyx_snaps
```

**Linux / macOS:**
```sh
export OPENAI_API_KEY="sk-..."
openonyx chat --provider openai --model gpt-4o --effort medium --snapshot-dir .onyx_snaps
```

With OpenRouter or any compatible gateway:
```sh
openonyx chat --provider openai --api-base https://openrouter.ai/api/v1 --model anthropic/claude-sonnet-4
```

### Option C — Local Models (Ollama, vLLM, LM Studio)

Whenever `--api-base` (or `OPENAI_API_BASE` / `OPENAI_BASE_URL`) is set, **`OPENAI_API_KEY` is optional**.

**Ollama:**
```sh
openonyx chat --provider openai --api-base http://localhost:11434/v1 --model qwen2.5-coder:14b --effort low --snapshot-dir .onyx_snaps
```

**vLLM / LM Studio:**
```sh
openonyx chat --provider openai --api-base http://localhost:8000/v1 --model Qwen/Qwen3-Coder-30B-A3B --snapshot-dir .onyx_snaps
```

> **Tip for local models:** Start with `--effort low` or `--effort medium`, and always enable `--snapshot-dir` so you can branch or `/rewind` at any milestone.

---

## 3. Your First Interactive Session (`openonyx chat`)

1. Open a terminal inside any Git repository (or pass `-w /path/to/repo`).
2. Run `openonyx chat --snapshot-dir .snaps_demo --record demo.tr`.
3. The TUI displays:
   - **Compact 3-row header**: Braille activity emblem, `[BUILD]` / `[PLAN]` badge, `[SANDBOX:workspace]`, active model & effort, and live telemetry (`ctx`, `in`/`cache`/`out`/`think` tokens, and estimated cost or `local`).
   - **Main `Memory & Trace` pane**: conversation turns, tool calls, observations, and live `[stream] ...▍` token previews.
   - **Bottom input bar**: prompt input and key hints.

### Step 1 — Explore Safely in `[PLAN]` Mode

Press **`Tab`** to toggle to **`[PLAN]`** (read-only; mutating tools are unconditionally blocked). Type an analysis prompt and press **`Enter`**:

```text
Inspect this repository structure and propose a concise plan to add a helper function to the main module.
```

The agent uses read-only tools (`glob_files`, `scent_outline`, `code_search`, `fs_read`, `plan_update`) without interrupting you for approval. When your terminal is at least `100` columns wide and `plan_update` is used, the **`Live Plan`** panel opens automatically on the right.

### Step 2 — Switch to `[BUILD]` and Review Diffs

Press **`Tab`** again to switch back to **`[BUILD]`** and send a follow-up prompt:

```text
Implement the change and run the test suite.
```

Whenever the agent proposes `fs_edit`, `fs_write`, or `shell_exec`, the TUI pauses in **`WaitingApproval`**:

- The banner shows which agent requested the action (`[agent #1 ...]`).
- File edits show a **colorized unified diff preview** before writing to disk.
- You can scroll `Memory & Trace` freely (`Up`/`Down`/`PgUp`/`PgDn`) while inspecting the diff.
- Press **`y`** to approve or **`n`** to deny (denials are fed back to the agent cleanly).

### Step 3 — Revert the Last Edit with `/undo`

If you want to roll back the latest `fs_edit` or `fs_write`, type:

```text
/undo
```

`openonyx` restores the lightweight Git tree checkpoint captured right before the edit — without touching `HEAD` or your staged index — and cleanly removes newly created untracked `fs_write` files while preserving unrelated untracked files.

### Step 4 — Exit Cleanly

Press **`Esc`** or **`Ctrl+C`**. `openonyx` flushes the final `exit` milestone into `.snaps_demo/snapshots.onyxpack` and writes `demo.tr`.

---

## 4. Resume, Inspect, or Replay Offline

After closing your session, try these zero-token commands:

```sh
# 1. List all recorded milestones and Git tree checkpoints
openonyx inspect .snaps_demo --list

# 2. Deterministically replay the session offline (0 HTTP calls, 0 disk writes)
openonyx replay demo.tr

# 3. Resume from the latest milestone
openonyx chat --resume .snaps_demo

# Or branch safely from an earlier tick into a sibling fork directory
openonyx chat --resume .snaps_demo --tick 8
```

---

## 5. Single-Turn Headless Execution (`openonyx run`)

For scripts or CI pipelines where you don't want a TUI:

```sh
openonyx run "summarize the pending git diff" --record run.tr --snapshot-dir .snaps_run
```

`openonyx` prints the final agent response along with `[openOnyx telemetry]` token/cost counters.

---

[← Manual Index](index.md) · [Versión en Español](01-primeros-pasos.md) · [Next: 02. Interfaz TUI y Flujo Diario →](02-interfaz-tui-y-flujo-diario.md)
