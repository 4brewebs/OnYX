# openOnyx (`openonyx`) User Manual

> 🇪🇸 **[Leer el manual completo en Español](index.md)**

Welcome to the official manual for **`openonyx`**, the deterministic coding-agent **CLI, TUI, and embedded Web PWA** harness built on the **OnYX** microkernel (`onyx-core` → `onyx-soul-agents` → `onyx-workspace` → `open-onyx`).

Unlike conventional terminal coding assistants backed by mutable message arrays, `openonyx` records every decision, read, edit, and command inside a **deterministic EAV graph** featuring:

- **Compressed milestone snapshots** (`snapshots.onyxpack`) with 64-bit FNV-1a checksums and truncated-tail crash tolerance.
- **Centralized session vault** (`.onyx/vault/` + `--auto-save`) auto-ignored by Git so workspace roots stay clean.
- **Non-destructive time-travel & forking** (`/rewind`, `/fork`, `chat --resume --tick`, `rewind --interactive`).
- **Transactional Git checkpoints (`/undo`)** without mutating `HEAD` or your staging index.
- **100% offline deterministic replay** (`ONYXTR2`, 0 network calls, 0 disk writes).
- **Defense-in-depth security**: **`blake3` Workspace Trust** (`openonyx trust` / `/trust`), `PLAN` / `BUILD` / `EXPLORE` modes, `.onyx/config.toml` rules (`deny > ask > allow`), *Scent-Heated Approvals*, OS sandboxing with `env_clear()`, Control Plane RO protection, and RAII `ProcessJob` process-tree teardown.
- **Embedded Web UI / PWA (`openonyx serve`)**: pure-`std` HTTP/1.1 + SSE server (0 `tokio`) ready for desktop browsers, mobile phones, or HTTPS reverse proxies, with `blake3` constant-time auth (`HttpOnly` cookie / `Bearer`) and explicit `--cors-origin` support for external frontends.
- **Semantic AST navigation** (`onyx-scent`: `scent_outline` + `scent_impact`) and **local process/container orchestration** (`sprout.toml`).

---

## Table of Contents

| Chapter | Topics |
| --- | --- |
| **[01. First Steps (Quickstart)](01-first-steps.md)** | Build/install, configure Gemini / OpenAI / Ollama / vLLM, `--auto-save`, first interactive TUI session to `Settled`, diff approvals, `Tab` Plan/Build, and `/undo`. |
| **[02. TUI & Daily Workflow](02-tui-and-daily-workflow.md)** | Compact 3-row header, Braille emblem, `--effort`, live `vault:<session>` badge, live telemetry, `Memory & Trace` scroll, `Live Plan` & `Services (Sprout)` panels, tiered context + `/compact [focus]`, `/new`, and multi-agent supervision (`spawn_agent`, `F2`, `/agent`, `Ctrl+Tab`). |
| **[03. Modes, Permissions & OS Sandbox](03-modes-permissions-and-sandbox.md)** | `PLAN`, `BUILD`, and `EXPLORE` modes; declarative `.onyx/config.toml` (`[permissions]` and `[sandbox]`), `deny > ask > allow`, `--ignore-deny`, `--auto-approve` elevation, Control Plane RO protection, `env_clear()`, RAII `ProcessJob`, and Linux (`bwrap` + AppArmor fallback) / macOS / Windows drivers. |
| **[04. Time-Travel, Snapshots & Forking](04-time-travel-snapshots-and-forking.md)** | Session vault (`.onyx/vault/` & `--auto-save`), `snapshots.onyxpack`, automatic milestone tags, `/undo` (including untracked `fs_write` cleanup), live `/rewind [tick]` & `/fork [name]`, `chat --resume --tick`, `rewind --interactive`, offline `replay` (`ONYXTR2`), `inspect`, and `doctor`. |
| **[05. Tools, Scent, Sprout & Skills](05-tools-scent-sprout-and-skills.md)** | Full 19-tool reference, AST navigation (`scent_outline` + `seen_outlines` cache + `@TOON v1`, `scent_impact`), local service & container orchestration (`sprout.toml`, `/services`, `/service`), project instructions (`ONYX.md`/`AGENTS.md`), authoring custom skills, and headless automation (`openonyx run`). |
| **[06. CLI Reference & Troubleshooting](06-cli-reference-and-troubleshooting.md)** | Complete subcommand, flag, and environment-variable tables, OpenAI/vLLM SSE EOF resilience & retries, `openonyx doctor` checklist, and practical Windows/Linux/macOS troubleshooting. |

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
  - Any local OpenAI-compatible `/v1/chat/completions` server such as **Ollama**, **vLLM**, or **LM Studio** (no API key required).
- **Optional**: `docker`, `nerdctl`, or `podman` for `--sandbox strict-container` or `kind = "container"` services in `sprout.toml`; `bwrap` (Bubblewrap) on Linux for filesystem sandboxing.

---

## Other Repository Documentation

- [Repository README (EN)](https://github.com/4brewebs/OnYX/blob/main/README.md) · [README en Español](https://github.com/4brewebs/OnYX/blob/main/README_es.md)
- [`open-onyx` Technical README](https://github.com/4brewebs/OnYX/blob/main/open-onyx/README.md)
- [`onyx-workspace` Technical README](https://github.com/4brewebs/OnYX/blob/main/onyx-workspace/README.md)
- [Agent Operational Skill (`SKILL.md`)](https://github.com/4brewebs/OnYX/blob/main/.onyx/skills/openonyx/SKILL.md)
- [Changelog (`CHANGELOG.md`)](https://github.com/4brewebs/OnYX/blob/main/CHANGELOG.md)
