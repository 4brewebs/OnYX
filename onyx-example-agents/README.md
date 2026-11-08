# onyx-example-agents

Multi-agent application built on `onyx-soul-agents`.

Uses the architecture:

- stages loaded via `RustLoader`
- systems registered from the soul's `FACTORIES`
- tick-indexed snapshots
- agents spawned as events
- replies submitted through the driver

```bash
cargo run -p onyx-example-agents
```
