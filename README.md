# OnYX

**A Lego of brains, not a closed solution.**

OnYX separates a small, dependency-free core of contracts from the souls that fulfill them.  
If your reality is different, write your own soul: the contract is what matters.

```
onyx-core   = contracts + tick + deterministic RNG            · 0 dependencies
onyx-soul   = default implementations of those contracts      · 0 external dependencies
onyx-example-app = minimal REPL that exercises the runtime    · uses soul
```

**Status: v0, under active design.**  
The public contract is not yet closed and will change in breaking ways.  
Do not use it where a failure has consequences.

## What exists today

### `onyx-core`
- `Graph` + `AttrRegistry` (concepts, attributes, directed relations, deterministic order)
- `OnyxEvent` (System / Ingress / Domain)
- `OnyxSystem` + `Ctx` + lifecycle hooks (`init`, `on_start`, `on_stop`, `on_restore`)
- Synchronous deterministic `tick` with `DetRng`
- Ports: `SnapshotStore`, `PlatformDriver`, `BlueprintLoader`, `SystemRegistry`
- `Supervisor` for system lifecycle

### `onyx-soul`
- `WorldGraph` (in-memory EAV)
- `FileSnapshotStore` (private binary format, 0 deps)
- `HeadlessDriver`
- `InMemoryBus`
- `InMemorySystemRegistry`
- `RustLoader` with `load_app` helper
- `Supervisor`

### `onyx-example-app`
- Minimal REPL that:
  - loads systems and stages via `RustLoader::load_app`
  - restores / saves graph snapshots
  - processes commands (`agree`, `status`, `exit`)

## What does NOT exist yet

- Real TUI / WASM drivers
- Stage change at runtime
- TOML / config loaders
- Recovery unit that includes system states and seed
- Multi-soul interoperability tests

## Quick start

```bash
cargo run -p onyx-example-app
```

```text
onyx> agree
[BOOT] NDA accepted. Clearance = 1
onyx> status
[BOOT] ConceptId(1), Clearance: Some(Int(1))
onyx> exit
```

## Design principles

- The brain only speaks `OnyxEvent`.
- The facade only projects.
- Execution is synchronous, ordered, and deterministic.
- Application state that must survive a snapshot lives in the graph.
- Concurrency lives in the adapters.

## License

MIT or Apache-2.0, at your choice.
