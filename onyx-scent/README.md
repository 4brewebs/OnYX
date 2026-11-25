# onyx-scent

**Deterministic AST indexer, thermal impact distiller, and `@TOON v1` serializer for OnYX harnesses.**

Extracts multi-file symbol graphs from **7 languages** — **Rust**, **Python**, **Java**, **TypeScript** (`.ts`/`.tsx`/`.mts`/`.cts`), **JavaScript** (`.js`/`.jsx`/`.mjs`/`.cjs`), **Go**, and **C/C++** (`.c`/`.h`/`.cc`/`.cpp`/`.cxx`/`.hh`/`.hpp`) — using `tree-sitter` with incremental `blake3` caching, a persistent binary cache (`.onyx/scent.idx`, `ONYXSCI1`), and integer-only basis-point (`0..=10_000`) heat diffusion, ensuring bit-exact output for deterministic replay.

## Features

- **Library-first (`default = []`)**: embeds directly into `onyx-workspace` without pulling CLI dependencies (`clap` is isolated behind `feature = "cli"`).
- **Multi-language AST & precision extraction (`lang.rs`, `index.rs`)**:
  - Supports `.rs`, `.py`, `.java`, `.ts`/`.tsx`/`.mts`/`.cts`, `.js`/`.jsx`/`.mjs`/`.cjs`, `.go`, and `.c`/`.h`/`.cc`/`.cpp`/`.cxx`/`.hh`/`.hpp`, qualifying methods as `<file>::<Type>::<symbol>` and capturing 1-based line ranges (`start_line-end_line`).
  - **Rust**: public `trait` methods inherit `vis:pub`, `struct` signatures summarize fields (`{ field1, field2 }`), test functions/modules (`#[test]`, `#[cfg(test)]`) are tagged `scope:test` (`scope:src` otherwise), and intra-file call resolution avoids cross-file noise when local targets match.
  - **Java**: captures annotations in entity signatures alongside class/method declarations.
  - **JS/TS**: `EntityKind::TypeAlias` for type aliases and `EntityKind::Component` for PascalCase functions with JSX (`.tsx`/`.jsx`); both are callable (`is_callable`).
- **Incremental `blake3` cache**: skips reparsing unchanged files across ticks (`WorkspaceIndex::index_workspace` / `index_target`) while retaining full-workspace entries across scoped updates.
- **Persistent binary cache (`cache.rs`, `.onyx/scent.idx`)**: `ONYXSCI1` format (magic + format version + entries sorted by `rel_path` + final `blake3` checksum) written atomically (temp file + rename) with a hygiene `.onyx/.gitignore`; any anomaly (missing/truncated file, wrong magic/version, checksum mismatch) is silently ignored and the index is rebuilt clean; incremental reloads only reparse files whose mtime/content changed (hash-verified), so large repos reload in tens of milliseconds.
- **Deterministic integer heat diffusion (`distill_heat`)**: propagates impact along `Calls` (`5_500 bp`) and `CalledBy` (`9_000 bp`) edges sorted strictly by epicenter, temperature, hop distance, and qualified name.
- **`@TOON v1` protocol (`toon.rs`)**: compact positional tuples (`subject | predicate | object | meta`) emitting `vis:pub|priv` and `scope:src|test` metadata, deterministic escaping, `ToonPageOptions` filtering (`public_only`, `exclude_tests`), and cursor/byte pagination (`outline_toon`, `impact_toon`, `references_toon`, `decode_toon`).

## Optional CLI

```sh
cargo run -p onyx-scent --features cli -- outline --root . --path onyx-core/src --public-only
cargo run -p onyx-scent --features cli -- impact --root . --target AgentHost::step --max-hops 3
cargo run -p onyx-scent --features cli -- references --root . --target AgentHost::step
```
