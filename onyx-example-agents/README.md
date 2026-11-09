# onyx-example-agents

Example application on `onyx-soul-agents`. It depends only on that soul;
the soul reexports the core contracts the app needs.

The binary walks one agent through the harness:

- `AgentHost` with a deterministic `search` tool (`ToolExecutor`)
- an external decider: `say` → `call` → `observe` → `done`
- `RecordingBridge::wrapping` a `HeadlessDriver`, driven with `step_with` and a `SurfaceBinding` (`run` and `run_until_settled` pass an empty surface list)
- a snapshot after `say`, requested before the step that writes it, so there is no in-flight tool event
- a second host that opens the same `AgentSnapshotStore` directory (`snapshots/`), restores, `set_tick` and `ReplayBridge::starting_at`; the fingerprints must match

It prints the sequence, `present_count`, that `last_surfaces` was not empty, and whether the fingerprints match.

```bash
cargo run -p onyx-example-agents
```
