# onyx-example-agents

Example application on `onyx-soul-agents`. It depends only on that soul.
The soul reexports the core contracts the app needs.

The binary walks one agent through the public API of this soul:

- `AgentHost` with a deterministic `search` tool (`ToolExecutor`)
- an external decider that replies `say`, then `call`, then `done` after the tool result arrives as `observe`
- `RecordingBridge::wrapping` a `HeadlessDriver`, driven with `step_with` and a `SurfaceBinding` (`run` and `run_until_settled` pass an empty surface list, so this demo does not use them for the recorded path)
- a snapshot after `say`, requested before the step that writes it, so there is no in-flight tool event
- a second host that opens the same `AgentSnapshotStore` directory (`snapshots/`), restores, calls `set_tick`, and continues with `ReplayBridge::starting_at`

It prints the sequence, `present_count`, that `last_surfaces` was not empty, and whether the fingerprints match. A successful run ends with `fingerprints_match=true` and `ok`. Restore alone must not leave `ticks` already equal to the recorded next `n`; the binary treats that as an error and calls `set_tick` itself.

The store writes under `./snapshots` relative to the current working directory. That path is gitignored at the repository root.

This successful path never consults the decider while the agent is `Acting`. `pending_requests` only lists agents in `Thinking`. The `has("call") => None` arm in `src/main.rs` is therefore unused here. After a `tool_error` that arm would leave the new request unanswered and the agent would fail by timeout. Updating it to treat `tool_error` with `Reply::fail` is pending in the example code.

This example does not exercise TR2 or external tools. Those are proposed work, not a current capability. See [onyx-soul-agents/README.md](../onyx-soul-agents/README.md).

```bash
cargo run -p onyx-example-agents
```
