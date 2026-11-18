# onyx-scent

**Deterministic AST indexer, thermal impact distiller, and `@TOON v1` serializer for OnYX harnesses.**

Extracts multi-file symbol graphs from **Rust**, **Python**, and **Java** using `tree-sitter` with incremental `blake3` caching and integer-only basis-point (`0..=10_000`) heat diffusion, ensuring bit-exact output for deterministic replay.

## Features

- **Library-first (`default = []`)**: embeds directly into `onyx-workspace` without pulling CLI dependencies (`clap` is isolated behind `feature = "cli"`).
- **Multi-language AST & precision extraction (`lang.rs`, `index.rs`)**:
  - Supports `.rs`, `.py`, and `.java`, qualifying methods as `<file>::<Type>::<symbol>` and capturing 1-based line ranges (`start_line-end_line`).
  - **Rust**: public `trait` methods inherit `vis:pub`, `struct` signatures summarize fields (`{ field1, field2 }`), test functions/modules (`#[test]`, `#[cfg(test)]`) are tagged `scope:test` (`scope:src` otherwise), and intra-file call resolution avoids cross-file noise when local targets match.
  - **Java**: captures annotations in entity signatures alongside class/method declarations.
- **Incremental `blake3` cache**: skips reparsing unchanged files across ticks (`WorkspaceIndex::index_workspace` / `index_target`) while retaining full-workspace entries across scoped updates.
- **Deterministic integer heat diffusion (`distill_heat`)**: propagates impact along `Calls` (`5_500 bp`) and `CalledBy` (`9_000 bp`) edges sorted strictly by epicenter, temperature, hop distance, and qualified name.
- **`@TOON v1` protocol (`toon.rs`)**: compact positional tuples (`subject | predicate | object | meta`) emitting `vis:pub|priv` and `scope:src|test` metadata, deterministic escaping, `ToonPageOptions` filtering (`public_only`, `exclude_tests`), and cursor/byte pagination (`outline_toon`, `impact_toon`, `decode_toon`).

## Optional CLI

```sh
cargo run -p onyx-scent --features cli -- outline --root . --path onyx-core/src --public-only
cargo run -p onyx-scent --features cli -- impact --root . --target AgentHost::step --max-hops 3
```
