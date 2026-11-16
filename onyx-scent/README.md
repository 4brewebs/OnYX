# onyx-scent

**Deterministic AST indexer, thermal impact distiller, and `@TOON v1` serializer for OnYX harnesses.**

Extracts multi-file symbol graphs from **Rust**, **Python**, and **Java** using `tree-sitter` with incremental `blake3` caching and integer-only basis-point (`0..=10_000`) heat diffusion, ensuring bit-exact output for deterministic replay.

## Features

- **Library-first (`default = []`)**: embeds directly into `onyx-workspace` without pulling CLI dependencies (`clap` is isolated behind `feature = "cli"`).
- **Multi-language AST & qualified names**: supports `.rs`, `.py`, and `.java`, qualifying methods as `<file>::<Type>::<symbol>` and capturing signatures + 1-based line ranges (`start_line-end_line`).
- **Incremental `blake3` cache**: skips reparsing unchanged files across ticks (`WorkspaceIndex::index_workspace` / `index_target`).
- **Deterministic integer heat diffusion (`distill_heat`)**: propagates impact along `Calls` (`5_500 bp`) and `CalledBy` (`9_000 bp`) edges sorted strictly by epicenter, temperature, hop distance, and qualified name.
- **`@TOON v1` protocol (`toon.rs`)**: compact positional tuples (`subject | predicate | object | meta`) with deterministic escaping and cursor/byte pagination (`outline_toon`, `impact_toon`, `decode_toon`).

## Optional CLI

```sh
cargo run -p onyx-scent --features cli -- outline --root . --path onyx-core/src
cargo run -p onyx-scent --features cli -- impact --root . --target AgentHost::step --max-hops 3
```
