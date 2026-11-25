# openOnyx (`openonyx`) User Manual

> 🇪🇸 **[Leer el manual completo en Español](index.md)**

Welcome to the official manual for **`openonyx`**, the deterministic coding-agent **CLI, TUI, and embedded Web PWA** harness built on the **OnYX** microkernel (`onyx-core` → `onyx-soul-agents` → `onyx-workspace` → `open-onyx`).

Unlike conventional terminal coding assistants backed by mutable message arrays, `openonyx` records every decision, read, edit, and command inside a **deterministic EAV graph** featuring:

- **Compressed milestone snapshots** (`snapshots.onyxpack`) with 64-bit FNV-1a checksums and truncated-tail crash tolerance.
- **Centralized session vault** (`.onyx/vault/` + `--auto-save`) auto-ignored by Git so workspace roots stay clean.
- **Non-destructive time-travel & forking** (`/rewind`, `/fork`, `chat --resume --tick`, `rewind --interactive`).
- **Asynchronous subagents in isolated Git worktrees** (`spawn_subagent` / `/spawn`, 3-way merge with *Scent-Heated Merge*, `--max-subagents`, and optional external CLI harness drivers via `[[external_drivers]]`), plus **`spawn_agent(isolated: true)`**: synchronous children in isolated `onyx/sub/agent-*` worktrees with 3-way reconciliation that **never touches `HEAD` or the root** until merged.
- **Multi-model benchmarking** (`openonyx bench`): identical snapshot/tick, throwaway per-candidate worktrees (rewind + RAII), live `provider:model` or offline `replay:<label>=<file.tr>` candidates, sandboxed `--verify`, 9-column Markdown table + JSON report via `--out`; the root stays 100 % intact.
- **Verify-on-Edit** (`[verify]` / `[[verify.rules]]` in `.onyx/config.toml`): after each successful `fs_edit`/`fs_write`, runs the rules whose `glob` matches the edited file (plus Scent-impacted `scope:test` files) in the sandbox and injects `[verify: PASS|FAIL|BLOCKED]` into the tool observation, subject to Workspace Trust and `[permissions].deny`.
- **Hot-swappable LLM targets** (`[[llm_profiles]]`, `/profile` / `/provider` / `/model` / `/effort`, live `provider/model` header and credential-free Web selector), including a **native Anthropic provider** (Messages API, ephemeral prompt caching, and budgeted extended thinking via `--thinking-budget`).
- **Dynamic MCP tools** (`[mcp.servers.<name>]`, `stdio` and `http` transports): each tool is exposed as `mcp__<server>__<tool>`, always approval-gated, `Deny` in `PLAN`/`EXPLORE`, behind the Workspace Trust gate, with per-server failure isolation and `openonyx doctor` auditing.
- **Modular `F1..F5` TUI views** (Chat, Agents, Sprout, Plan & Diff, Timeline) with a `Ctrl+K` command palette and `/view`.
- **Transactional Git checkpoints (`/undo`)** without mutating `HEAD` or your staging index.
- **100% offline deterministic replay** (`ONYXTR2`, 0 network calls, 0 disk writes).
- **Defense-in-depth security**: **`blake3` Workspace Trust** (`openonyx trust` / `/trust`), `PLAN` / `BUILD` / `EXPLORE` modes, `.onyx/config.toml` rules (`deny > ask > allow`), *Scent-Heated Approvals*, OS sandboxing with `env_clear()`, Control Plane RO protection, and RAII `ProcessJob` process-tree teardown.
- **Embedded Web UI / PWA (`openonyx serve`)**: pure-`std` HTTP/1.1 + SSE server (0 `tokio`) ready for desktop browsers, mobile phones, or HTTPS reverse proxies, with `blake3` constant-time auth (`HttpOnly` cookie / `Bearer`) and explicit `--cors-origin` support for external frontends.
- **Semantic AST navigation, Scent 2.0** (`onyx-scent`: `scent_outline` + `scent_impact` + `scent_references`): **7 languages** (Rust, Python, Java, TypeScript, JavaScript, Go, C/C++), `EntityKind::{TypeAlias, Component}` kinds, incremental binary cache `.onyx/scent.idx` (`ONYXSCI1`, sub-20 ms reloads) and the `scent outline|impact|references` CLI; plus **local process/container orchestration** (`sprout.toml`).

---

## Table of Contents

| Chapter | Topics |
| --- | --- |
| **[01. First Steps (Quickstart)](01-first-steps.md)** | Build/install, configure Gemini / OpenAI / Ollama / vLLM, `--auto-save`, first interactive TUI session to `Settled`, diff approvals, `Tab` Plan/Build, and `/undo`. |
| **[02. TUI & Daily Workflow](02-tui-and-daily-workflow.md)** | Compact 3-row header with live `provider/model`, Braille emblem, `--effort`, live `vault:<session>` badge, live telemetry, `Memory & Trace` scroll, `Live Plan` & `Services (Sprout)` panels, tiered context + `/compact [focus]`, `/new`, modular `F1..F5` views + `Ctrl+K` palette + `/view`, multi-agent supervision (`spawn_agent`, `/agent`, `Ctrl+Tab`), and hot-swapping the LLM target (`/profile`, `/provider`, `/model`, `/effort`). |
| **[03. Modes, Permissions & OS Sandbox](03-modes-permissions-and-sandbox.md)** | `PLAN`, `BUILD`, and `EXPLORE` modes; declarative `.onyx/config.toml` (`[permissions]`, `[sandbox]`, and `[verify]` Verify-on-Edit), `deny > ask > allow`, `--ignore-deny`, `--auto-approve` elevation, MCP servers (`[mcp.servers.<name>]`, `mcp__<server>__<tool>`, approval gate, `Deny` in `PLAN`/`EXPLORE`), Control Plane RO protection, `env_clear()`, RAII `ProcessJob`, and Linux (`bwrap` + AppArmor fallback) / macOS / Windows drivers. |
| **[04. Time-Travel, Snapshots & Forking](04-time-travel-snapshots-and-forking.md)** | Session vault (`.onyx/vault/` & `--auto-save`), `snapshots.onyxpack`, automatic milestone tags, `/undo` (including untracked `fs_write` cleanup), live `/rewind [tick]` & `/fork [name]`, `chat --resume --tick`, `rewind --interactive`, offline `replay` (`ONYXTR2`), `inspect`, and `doctor`. |
| **[05. Tools, Scent, Sprout & Skills](05-tools-scent-sprout-and-skills.md)** | Full 24-tool reference (including the async subagent tools `spawn_subagent` / `subagent_status` / `subagent_merge` / `subagent_discard`, `spawn_agent(isolated: true)` isolated worktrees, and external CLI harness drivers) plus dynamic `mcp__*` tools, Scent 2.0 AST navigation (7 languages, `scent_outline` + `scent_impact` + `scent_references`, `.onyx/scent.idx` `ONYXSCI1` cache, `scent` CLI), local service & container orchestration (`sprout.toml`, `/services`, `/service`), project instructions (`ONYX.md`/`AGENTS.md`), authoring custom skills, and headless automation (`openonyx run`). |
| **[06. CLI Reference & Troubleshooting](06-cli-reference-and-troubleshooting.md)** | Complete subcommand, flag, and environment-variable tables (including `--thinking-budget`, `ANTHROPIC_*`, and the full `openonyx bench` reference: `--models` live/replay candidates, `--verify`, `--out`, `--goal`), OpenAI/vLLM and Anthropic SSE EOF resilience & retries, `openonyx doctor` checklist (with MCP server audit), and practical Windows/Linux/macOS troubleshooting. |

---

## Recommended Reading Paths

- **First day with `openonyx`**: start with [01. First Steps](01-first-steps.md), then read [02. TUI & Daily Workflow](02-tui-and-daily-workflow.md).
- **Running local LLMs (Ollama, vLLM, LM Studio)**: see local provider setup in [01. First Steps](01-first-steps.md), `--effort` in [02. TUI & Daily Workflow](02-tui-and-daily-workflow.md), and SSE retry resilience in [06. CLI Reference & Troubleshooting](06-cli-reference-and-troubleshooting.md).
- **Strict security / corporate environments**: read [03. Modes, Permissions & OS Sandbox](03-modes-permissions-and-sandbox.md).
- **Branching experiments & reproducible audits**: read [04. Time-Travel, Snapshots & Forking](04-time-travel-snapshots-and-forking.md).
- **Local microservices & containers**: see Sprout in [05. Tools, Scent, Sprout & Skills](05-tools-scent-sprout-and-skills.md).

---

## Prerequisites

- **Rust `1.85` or newer** (the repository pins `1.85.1` in `rust-toolchain.toml` with `edition = "2024"`).
- **Git** on `PATH` (required for reversible `/undo` checkpoints, `rewind`, `git_status`, and `git_diff`).
- **Access to an LLM**:
  - A **Google AI Studio (`GEMINI_API_KEY`)** key, or
  - An **OpenAI / OpenRouter (`OPENAI_API_KEY`)** key, or
  - An **Anthropic (`ANTHROPIC_API_KEY`)** key for the native provider (`--provider anthropic`), or
  - Any local OpenAI-compatible `/v1/chat/completions` server such as **Ollama**, **vLLM**, or **LM Studio** (no API key required).
- **Optional**: `docker`, `nerdctl`, or `podman` for `--sandbox strict-container` or `kind = "container"` services in `sprout.toml`; `bwrap` (Bubblewrap) on Linux for filesystem sandboxing.

---

## Other Repository Documentation

- [Repository README (EN)](https://github.com/4brewebs/OnYX/blob/main/README.md) · [README en Español](https://github.com/4brewebs/OnYX/blob/main/README_es.md)
- [`open-onyx` Technical README](https://github.com/4brewebs/OnYX/blob/main/open-onyx/README.md)
- [`onyx-workspace` Technical README](https://github.com/4brewebs/OnYX/blob/main/onyx-workspace/README.md)
- [Agent Operational Skill (`SKILL.md`)](https://github.com/4brewebs/OnYX/blob/main/.onyx/skills/openonyx/SKILL.md)
- [Changelog (`CHANGELOG.md`)](https://github.com/4brewebs/OnYX/blob/main/CHANGELOG.md)
