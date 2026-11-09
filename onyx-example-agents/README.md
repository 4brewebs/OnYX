# onyx-example-agents

Multi-agent application built on `onyx-soul-agents`.

Uses the architecture:

- stages loaded via `RustLoader`
- systems registered from the soul's `FACTORIES`
- tick-indexed snapshots
- agents spawned as events
- an external decider through `RecordingBridge`

The crate depends only on `onyx-soul-agents`. The soul reexports the
core contracts the app needs.

```bash
cargo run -p onyx-example-agents
```
