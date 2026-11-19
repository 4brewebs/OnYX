# Changelog

All notable changes to the **OnYX** workspace crates (`onyx-core`, `onyx-soul`, `onyx-soul-agents`, `onyx-scent`, `onyx-workspace`, and `open-onyx`) are documented in this file.

## [0.1.0] - 2026-11-19

Initial public release of the deterministic OnYX agent microkernel, reference souls, AST distiller, sandboxed workspace tools, and the `openonyx` CLI/TUI harness.

### Crates

- **`onyx-core` (`0.1.0`)**: Zero-dependency deterministic microkernel contracts (`Graph`, `GraphView`, `AttrRegistry`, `EventBus`, `OnyxSystem`, `PlatformDriver`, `SnapshotStore`, `DetRng`, ticks, and conformance suite).
- **`onyx-soul` (`0.1.0`)**: Zero-dependency reference soul implementing the core ports with an in-memory EAV graph, priority bus, supervisor, and snapshot store.
- **`onyx-soul-agents` (`0.1.0`)**: Zero-dependency multi-agent soul with `AgentHost`, `RecordingBridge` / `ReplayBridge` (`ONYXTR2` transcripts), `ToolSystem`, `SubagentSystem` (`spawn_agent`), `continue_agent`, `append_agent_memory`, and `HeadlessDriver`.
- **`onyx-scent` (`0.1.0`)**: Deterministic Tree-sitter AST indexer (`Rust`, `Python`, `Java`), integer-only thermal impact diffusion (`0..=10_000` bp), and `@TOON v1` compact serializer (`scent` binary behind the `cli` feature).
- **`onyx-workspace` (`0.1.0`)**: Deterministic workspace tools and EAV ledger (`fs_read` with deduplication/invalidation, `fs_list`, `fs_write`, multi-hunk atomic `fs_edit` with relative re-indentation, `code_search`, `git_status`, `git_diff`, `load_skill`, `plan_update`, `shell_exec` with credential scrubbing and timeouts, and optional `scent_outline` / `scent_impact`). Includes `[PLAN]`/`[BUILD]` mode enforcement and transactional git tree checkpoints for `/undo` and `rewind`.
- **`open-onyx` (`0.1.0`, bin `openonyx`)**:
  - Interactive `chat` TUI (`ratatui` + `crossterm`) with SSE token streaming (`[stream] ...▍`), live diff approval (`y`/`n`), `Tab` Plan/Build toggle, `/undo`, `/mode`, `/new`, full `Memory & Trace` scroll synchronization, and side `Live Plan` panel (`>= 100` cols).
  - Multi-provider LLM support (`--provider gemini|openai`, `--model`, `--api-base`) for Gemini and OpenAI-compatible / local endpoints (Ollama, vLLM, LM Studio) with context budgeting and `ONYX.md` / `AGENTS.md` / `.onyx/skills/*/SKILL.md` support.
  - Compressed append-only milestone snapshot container (`snapshots.onyxpack`, `ONYXPK1` zlib frames + FNV-1a checksums) with `MilestoneSnapshotSystem`, non-contiguous `inspect` (`--list`, `--json`) and `rewind` (`--dry-run`), pure offline `replay`, and `doctor` diagnostics.
  - Complete `--help` (`-h`, `help`) and `--version` (`-V`) CLI output (`exit code 0`).
