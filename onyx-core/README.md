# onyx-core

Zero-dependency deterministic microkernel contracts for **OnYX**. Only traits, data types, the tick loop contract, and conformance suites live here; implementations live in souls (`onyx-soul`, `onyx-soul-agents`).

## Core Contracts

- **EAV Graph & Snapshots (`graph`)**: `Graph`, `GraphView`, `AttrRegistry`, `Attr`, `ConceptId`, `Value`, `SnapshotStore`, `OnyxError`, plus view wrappers `ReadOnly` and `AttrOnly`.
- **Events & Input (`event`)**: `OnyxEvent`, `SystemEvent`, `IngressEvent`, `DomainEvent`, `Key`, `KeyCode`, `Mods`.
- **Event Bus (`bus`)**: `EventBus`.
- **Systems & Blueprints (`system`)**: `OnyxSystem`, `Ctx`, `StageBlueprint`, `SystemSpec`, `EntitySpec`, `SystemState`, `SystemRegistry`, `BlueprintLoader`.
- **Deterministic RNG (`detrng`)**: `DetRng`.
- **Tick Execution (`tick`)**: `tick` advances one deterministic simulation step through the registered stages and systems.
- **Surfaces & Platform Ports (`surface`, `driver`)**: `SurfaceId`, `SurfaceBinding`, `PlatformDriver`.

## Feature Flags

- `conformance`: Exposes `onyx_core::conformance` with reusable contract tests for any soul implementing `Graph` and `EventBus`.

## Architecture Invariant

`onyx-core` declares **zero external dependencies** and performs no disk, network, or terminal I/O.
