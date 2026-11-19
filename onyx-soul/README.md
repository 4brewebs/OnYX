# onyx-soul

Zero-dependency reference soul implementing the `onyx-core` microkernel ports for general deterministic applications and games.

It depends only on `onyx-core`.

## Components

- **`WorldGraph` (`graph`)**: In-memory Entity-Attribute-Value (`Graph`, `GraphView`, `AttrRegistry`) store with deterministic binary snapshot import/export and an internal incoming-edge index.
- **`InMemoryBus` (`bus`)**: Deterministic priority/tick `EventBus`.
- **`FileSnapshotStore` (`store`)**: File-backed `SnapshotStore` implementation.
- **`Supervisor` (`supervisor`)**: System lifecycle and panic isolation wrapper around `OnyxSystem`.
- **`RustLoader` (`loader`) & `InMemorySystemRegistry` (`registry`)**: Blueprint loading and factory registry for stages and systems.
- **`adapters` (`HeadlessDriver`, `PromptDriver`, `tui`)**: Lightweight `PlatformDriver` adapters for headless execution and simple terminal prompts.

For agent-oriented multi-turn harnesses, recording/replaying `ONYXTR2` transcripts, and tool execution, see [`onyx-soul-agents`](../onyx-soul-agents) and [`open-onyx`](../open-onyx).
