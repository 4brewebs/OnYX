# OnYX

**A Lego of brains, not a closed solution.**

OnYX separates a small, dependency-free core of contracts from the souls that fulfill them.
If your reality is different, write your own soul: the contract is what matters.

**Status: v0, under active design.**
The public contract is not yet closed and will change in breaking ways.
Do not use it where a failure has consequences.

The full project description, including proposed design that is not yet code, is in Spanish in [README_es.md](README_es.md).

```
onyx-core           = contracts, tick, deterministic RNG     · no dependencies
onyx-soul           = default implementations of those ports · no external dependencies
onyx-soul-agents    = agent-oriented soul, same core ports   · no external dependencies
onyx-example-app    = minimal REPL on onyx-soul              · depends only on that soul
onyx-example-agents = host, tool, snapshot, and replay demo  · depends only on onyx-soul-agents
```

## Workspace

The workspace has these five member crates. The minimum Rust version is 1.85, the edition is 2024, and the workspace uses resolver 3.

`onyx-core`, `onyx-soul`, `onyx-soul-agents`, and both example crates declare no external crates. Souls depend on `onyx-core`. Each example depends on its soul, which reexports the core types the app needs, so the app does not declare `onyx-core`. Future souls and apps may add external dependencies. There is no CI workflow in this tree that enforces the rule.

## What exists today

### `onyx-core`

- `Graph`, `GraphView`, and `AttrRegistry` (concepts, attributes, directed relations, deterministic order)
- `OnyxEvent` (`System`, `Ingress`, `Domain`)
- `OnyxSystem` and `Ctx`, with lifecycle hooks (`init`, `on_start`, `on_stop`, `on_restore`)
- Synchronous, ordered `tick` with `DetRng` and `Tick { n }` as a counter, not a wall clock
- Ports: `EventBus`, `SnapshotStore`, `PlatformDriver`, `BlueprintLoader`, `SystemRegistry`
- Capability adapters `ReadOnly` and `AttrOnly` (Rust 1.85 has no trait upcasting)
- Graph conformance cases behind the `conformance` feature, off by default

`tick` does not use `BlueprintLoader` or `SystemRegistry`. Those ports exist so a soul or an app can load stages. There is no `Supervisor` type in core. System states live in a slice the caller passes into `tick`.

### `onyx-soul`

- `WorldGraph`, an in-memory EAV graph with a private incoming-edge index and surgical `destroy`
- `FileSnapshotStore` (private graph bytes on disk, standard library only)
- `HeadlessDriver` and `PromptDriver` (`TuiDriver` is not compiled)
- `InMemoryBus`, `InMemorySystemRegistry`, `RustLoader` with `load_app`, and `Supervisor`

The WorldGraph design and its limits are in [docs/world-graph.md](docs/world-graph.md). They are not obligations on other souls.

### `onyx-soul-agents`

A second soul, specialized for deterministic multi-agent apps, documented in [onyx-soul-agents/README.md](onyx-soul-agents/README.md). It does not depend on `onyx-soul`. It includes `AgentGraph` (same observable graph contract, its own private snapshot format), `AgentHost`, the TR1 record and replay bridges, `HeadlessDriver` with a handle that survives `wrapping`, in-brain `ToolExecutor` tools, and `AgentSnapshotStore`.

### Examples

- `onyx-example-app` loads a boot stage, runs a small REPL (`agree`, `status`, `exit`), and saves or restores the graph through `FileSnapshotStore`.
- `onyx-example-agents` walks one agent through the host, a deterministic tool, a snapshot, `set_tick`, and replay. See [onyx-example-agents/README.md](onyx-example-agents/README.md).

## Recovery today

Graph snapshots are implemented. The format is private to each soul, has no checksum, and has no migration scheme. An incompatible capture is rejected and left untouched. A snapshot of the graph does not include the bus, in-flight events, the tick counter `n`, the seed, or system states. The events the outbox returns to the bus for the next tick stay with the caller.

After a successful import, `tick` re-runs `init` on every system and calls `on_restore` only on `Active` systems whose `init` succeeded. `AgentHost::request_restore` does not realign `n`. After restore, the caller uses `set_tick` so the next `step` presents the intended `n` to `tick` and `DetRng`. `RecordingBridge` and `ReplayBridge` expose `starting_at` to align the TR1 transcript with that next poll. To continue like an uninterrupted run after a cut that left events in the outbox, the caller must also re-inject those events.

Within one compatible configuration, the recovery tests of both souls cut every tick from 0 through 28 of a 30-tick run and re-inject the pending outbox (`Reinsert` in `onyx-soul`, `AgentHost::push` in `onyx-soul-agents`), then set the next `n` (`set_tick` in `AgentHost`) and continue. They compare the observable graph (and, in the agents soul, `fingerprint`) and the contract events, not snapshot bytes. Without that re-injection, the match holds only for a quiet cut, when the outbox is empty. A dedicated control in each recovery suite checks that case, and `onyx-example-agents` snapshots after `say`, with no in-flight tool event. That is not the complete recovery unit described as proposed design in [README_es.md](README_es.md).

## Designed, not implemented

These items are proposed design or explicit implementation gaps, not current capabilities. The Spanish README keeps the full specifications.

- `SystemTable`, `TickOutcome`, and a `Shutdown` policy in core
- `TickState` (next `n`, seed, and system states) and a recovery unit that captures the graph, `TickState`, and compatible stage, configuration, and system order at the same tick frontier
- Runtime stage change (`LoadStage`); `RustLoader` still ignores `SystemSpec::persistent` and `StageBlueprint::entities` (TODO in both souls)
- TR2 and external tools whose results must be recorded and replayed (TOIMPLEMENT). In-brain deterministic `ToolExecutor` tools are implemented. TR1 records decider `Reply` values, not a complete external-tool protocol.
- Real TUI and WASM drivers, and TOML blueprint loaders
- Interoperability tests that swap two soul implementations under the same systems and blueprint
- CI workflows (none are in the tree)

v0 does not offer bounded latency or a tick that allocates nothing. Changing the soul is not enough to obtain those properties.

## Quick start

```bash
cargo run -p onyx-example-app
```

```text
[CORE] Arranque limpio.
========================================
  OnYX Engine REPL (v0.1.0)
  Type 'agree', 'status', or 'exit'
========================================
onyx> agree
[BOOT] System online. Awaiting 'agree'...
[BOOT] NDA accepted. Clearance = 1
onyx> status
[BOOT] ConceptId(1), Clearance: Some(Int(1))
onyx> exit
Apagado seguro...
[CORE] Guardado en ./snapshots/repl_savegame.snap
Engine stopped after 2 ticks.
```

The first tick both creates the player and handles `agree`, so those two `[BOOT]` lines appear together. The demo reads stdin in `main` and injects commands through `HeadlessDriver`. It does not go through a TUI driver.

```bash
cargo run -p onyx-example-agents
```

The run ends with `fingerprints_match=true` and `ok` when restore, `set_tick`, and replay match the uninterrupted recording.

## Design principles

- The brain only speaks `OnyxEvent`.
- The facade only projects.
- Execution is synchronous, ordered, and deterministic.
- Application state that must survive a snapshot lives in the graph.
- Concurrency lives in the adapters.

## License

MIT or Apache-2.0, at your choice. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
