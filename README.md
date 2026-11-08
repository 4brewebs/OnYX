# OnYX

**A Lego of brains, not a closed solution.**

OnYX separates a small, dependency-free core of contracts from the souls that fulfill them. If your reality is different, write your own soul: the contract is what matters.

> **Status: v0, under active design.** The public contract is not yet closed and will change in breaking ways. Do not use it where a failure has consequences. This README separates what exists today from what is only designed.

```
onyx-core   = contracts + tick + deterministic RNG            · 0 dependencies
onyx-soul   = default implementations of those contracts      · 0 external dependencies
adapters    = TUI, WASM, TOML, Redis, Kafka...                · (do not exist yet)
```

## Status as of November 7, 2026

The last code modification of this freeze is `c22927e`.
This README update does not change the contracts or the implementation.
The workspace contains only `onyx-core` and `onyx-soul`, with no external dependencies.

### Freeze verification

Checked with Rust 1.85.1 and edition 2024:

- 63 tests and 3 doctests pass in both debug and release.
- The 63 tests are 29 core contract tests, 8 defective controls,
  3 bus tests, 13 from the suite on WorldGraph, and 10 private codec tests.
- Each defective control passes the same case with the defect deactivated and
  causes the suite to fail when activated. They cover ID reuse,
  unordered queries, implicit creation from `set`, partial import,
  isolated name leak, isolated high-water ID advance, and the two
  `destroy` cleanup omissions: `Concept` properties and incoming relations.
- The `conformance` API is not available in the default build.
  Enabling the feature makes it available. Soul enables it only for tests.
- Clippy keeps a single known warning: `tick` receives 8 arguments and
  exceeds the `too_many_arguments` threshold of 7. It has not been silenced.

Commands to repeat the checks from the root:

```sh
cargo fmt --all -- --check
cargo check --workspace --lib
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace
cargo test --workspace --release
cargo test -p onyx-core --features conformance --test conformance_probes
cargo test -p onyx-core --no-default-features
cargo doc --workspace --no-deps --all-features
cargo tree --workspace
```

### Correspondence between contract and tests

The cases are in [the graph suite](onyx-core/src/conformance/graph.rs).
WorldGraph runs them in [its conformance tests](onyx-soul/tests/graph_contract.rs).

| Contract guarantee | Suite cases |
| --- | --- |
| Exact names, idempotent registration and `Attr` stability after import | `registry`, `import_remaps_attributes` |
| Empty concepts, ascending IDs without reuse and AND queries by presence | `creation_and_queries`, `high_water` |
| `set` does not create concepts nor store non-existent destinations; `get` returns owned text | `set_and_missing_references` |
| Order, uniqueness, separation of properties and relations and exact triple removal | `ordering_and_relations` |
| `destroy` cleans references without deleting owners or foreign data | `destruction` |
| Preservation of variants, integer extremes and floating-point bits | `values_roundtrip`, `float_bits` |
| Self-ref, cycles and repeated import | `cyclic_roundtrip` |
| Exact substitution, remapping, maximum of both graphs and continuity of the restored graph | `import_remaps_attributes`, `high_water`, `continuation` |
| Rejection without changes to concepts, names or max ID | `rejected_import_is_atomic`, `modified_import_preserves_state_on_error` |

Error cases receive a private probe supplied by the soul.
They do not assume that any modified byte is invalid nor compare `Attr` numbers
between instances. WorldGraph uses its private export as a probe on
the same instance. No canonical serialization is imposed on other souls.

The [defective controls](onyx-core/tests/conformance_probes.rs) verify
detection of the eight previous failures. The [private codec tests](onyx-soul/src/graph/snapshot.rs)
cover truncations, corruptions, lengths, duplicates, dangling references,
late failures and `u64` exhaustion without modifying state.
Capability adapters are also checked in
[graph_api](onyx-core/tests/graph_api.rs) and in the three failed-compilation doctests.

### Limits and resumption point

- These tests do not demonstrate exchange with a second soul nor the
  complete recovery of the runtime. The defective doubles are not another soul.
- Real exhaustion of the `u32` attribute registry has not been executed.
  The implementation uses a checked conversion before adding a name.
- `destroy` traverses the remaining concepts and their references. There are no
  performance measurements nor guarantees of latency or absence of allocations.
- The snapshot format is private and does not incorporate a checksum.
  An alteration that still describes a valid graph may be accepted.
- `SystemTable`, `TickOutcome`, `Shutdown`, `TickState`,
  the complete recovery unit and the stage manager remain pending. There are also no
  implementations of `SnapshotStore` nor of `PlatformDriver`.
- To resume, the next proposed design is `SystemTable` and `TickOutcome`
  with the `Shutdown` policy, before implementing complete recovery.
  The treatment of pending events in the capture still needs to be agreed.
  Any contract change requires prior agreement and an update of this README.

## Design criterion

The core fixes the **observable meaning** (concepts, attributes, relations, order and execution rules), not how it is stored or indexed. A soul may use SQL, ECS or OCS if it respects that meaning. Core also fixes capability and execution constraints (for example, what each hook may do), but never the internal representation of storage. **If it is neither observable nor an execution constraint, it is not an obligation of the core.**

The internal structures of the default soul, its stage membership schema and its snapshot format are soul decisions, not core ones.

## Model commitments

OnYX is opinionated in the execution model and agnostic in the representation. Execution commitments are part of the design; the type and signature decisions of v0 are revisable.

**Model commitments.** Changing them requires redesigning and re-agreeing the contract:

- Synchronous, ordered brain without wall-clock time.
- The brain only speaks `OnyxEvent`, and the facade only projects.
- System lifecycle with states and tick boundaries.
- Reproducibility based on explicit seed and `n` as counter.
- Model of concepts, attributes and relations.

**Revisable v0 decisions.** They may change in a major version:

- `Value` as a closed enum, `ConceptId(u64)` and `Attr(u32)`.
- `drain` returns a `Vec`, and graph mutations are infallible.
- A single graph and a single brain thread.
- No backpressure nor partitioning, and no real-time guarantees: v0 does not offer them, and it is not promised that changing the soul is enough to obtain them.

**Agnostic in:** graph storage and indexing, snapshot and store format, platform, origin of blueprints and concurrency within adapters.

## What exists today

**`onyx-core`**

- `OnyxEvent` (System / Ingress / Domain).
- `EventBus` port (FIFO, no backpressure).
- `Graph`, `SnapshotStore` and `PlatformDriver` ports (core defines their contracts; soul already provides `WorldGraph`). `Graph` separates reading (`GraphView`) and attribute registration (`AttrRegistry`), with `ReadOnly` and `AttrOnly` adapters compatible with Rust 1.85.1.
- `get` returns `Option<Value>`. The contract documents queries, IDs and import; `exists`, `props` and `links` allow inspecting the state. `Value` compares floats by their bits, and `OnyxError` implements `Display` and `Error`.
- `OnyxSystem`, states `Active` / `Stopped` / `Blocked`, and blueprint types (data only).
- Idempotent `init` receives only `AttrRegistry`. After import, the tick calls `init` on all systems: only the Active ones with success receive `on_restore`, with `GraphView` and never `on_start`. A Blocked with success moves to Stopped, without hook. The traversal follows the system order: `init` and the possible `on_restore` of one finish before the `init` of the next.
- `PlatformDriver::present` receives `GraphView`: it exposes only queries, without attribute registration nor mutating methods.
- `BlueprintLoader` and `SystemRegistry` ports (defined, the tick does not use them yet).
- Synchronous, ordered `tick` without wall-clock time, and `DetRng` (non-cryptographic RNG with explicit seed).

**`onyx-soul`**

- `InMemoryBus`.
- `WorldGraph`: in-memory EAV graph with concepts, properties, directed relations, ordered queries and atomic export/import in a private format. Its [design and limits](docs/world-graph.md) do not form part of the contract of other souls.

**Graph suite (`onyx-core`, feature `conformance`)**

- Generic cases that receive factories, without requiring `Default`. The feature is off by default and adds no dependencies. `onyx-soul` enables it only in `[dev-dependencies]`.
- Public verification API, outside the stability of v0: its cases may grow or harden between versions. Eight defective test graphs demonstrate that it detects ID reuse, unordered queries, implicit creation from `set` and non-atomic import, including leaks that only alter names or the counter, as well as Concept references and incoming relations that `destroy` does not clean. Each case first passes with the defect deactivated.

Reproducible verification: `cargo test --workspace` and `cargo test --workspace --release`. `WorldGraph` passes the generic cases of queries, references, destruction, attribute remapping and continuity after restore, as well as private tests of atomic rejection of invalid snapshots. Byte mutations are not assumed invalid: only if `import` returns `Err` is conservation of the observable and of a private state probe supplied by the soul required, which covers the complete registry and the counter. The suite does not require snapshot equality between instances. This does not yet demonstrate exchange with a second graph implementation nor complete runtime recovery. RNG, tick, bus and port capabilities are also tested. Idempotence of `init` is checked with a factory of test consumer systems, not for any external system. `cargo tree --workspace` allows verifying that there are no external dependencies. To run only the defective controls: `cargo test -p onyx-core --features conformance --test conformance_probes`.

## What does NOT exist yet

The soul already provides graph and bus, but an implementation of `SnapshotStore` and of `PlatformDriver` is still missing. The application must provide those ports and its systems to execute the tick; complete recovery and stage change remain pending.

## What is designed, but not implemented

These decisions are agreed in design and **are not available code**:

- `SystemTable` in core that imposes the lifecycle, and `TickOutcome` with the `Shutdown` policy.
- `TickState`: plain data with the next `n`, the seed and the states by unique name. The table exposes its states; the caller provides `n` and seed. There is no free setter: restoration validates system names and order and applies the `init` / `on_restore` protocol, without being able to skip transitions; compatibility of the stage and configuration is the caller's responsibility.
- Recovery unit: graph, `TickState` and the identification of the stage, the configuration and the order of compatible systems, **captured at the same tick boundary**. Keeping them together is the caller's obligation: core imposes no format on the store nor can it verify that they correspond to the same capture point.
- Stage change (`LoadStage`) via a manager provided by the soul.
- Extension of the generic suite to other ports and to the lifecycle; it currently covers the graph.

## Write your own soul

A soul provides one or more implementations of the ports (graph, snapshot persistence, platform driver and, when they exist, stage manager and system registry). It does not have to cover all of them: the application composes the rest with other components and adapters. During `run`, systems receive `Ctx` (graph, inbox, outbox, RNG). Hooks receive their lifecycle ports. None receives the bus nor the soul.

Rules that protect soul interchange:

1. Application state that must survive a snapshot lives in the graph. Execution metadata (next `n`, seed, system states, stage and configuration) travel separately, in the recovery unit kept by the caller.
2. The tick does not depend on wall-clock time.
3. Concurrency lives in the adapters; the brain is synchronous.

Reproducing an execution requires the same initial graph state, the same inputs, seed, next `n`, configuration, order and system state, as well as implementations with the same semantics.

Interchanging souls seeks to preserve systems and blueprints compatible with the contract, not to reuse snapshots. Each soul defines its format and rejects incompatible captures without deleting data.

`WorldGraph` passes the available graph suite. It does not equate to demonstrating all soul contracts nor interchange with another implementation.

## What it is not

- It is not a UI framework nor another ECS.
- It does not offer real-time guarantees in v0: neither bounded latency nor absence of allocations in the tick. Core fixes synchronous, ordered execution and control boundaries, and leaves the implementation open within those rules; it does not fix the storage representation. A bounded worst case would require verifying the entire execution path, not just changing the soul.
- It is not a closed solution: the default soul is generalist and opinionated.

## Contributing

Judge the contract, not the résumé. If you build a soul, publish it: it is the best validation of the design.

## License

MIT or Apache-2.0, at your choice. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
