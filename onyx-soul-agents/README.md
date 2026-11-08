# onyx-soul-agents

**Agent harness soul for OnYX.**

Implements the core ports with the peculiarities needed to build deterministic multi-agent applications:

- `AgentGraph` — wraps `WorldGraph` and adds agent helpers
- `AgentSnapshotStore` — tick-indexed snapshots (rewind to any tick)
- `AgentBus` — in-memory bus
- `AgentDriver` — projects pending requests and accepts replies
- `AgentSpawner` + `AgentRuntime` — systems that only speak `OnyxEvent`

The application decides stages, systems, tools, and policies.  
This soul closes the framework so the app can be written against it.

## Usage

```rust
use onyx_soul_agents::{AgentGraph, AgentSnapshotStore, AgentBus, AgentDriver, FACTORIES, stage};

let mut graph = AgentGraph::new();
let mut store = AgentSnapshotStore::new("snapshots");
let mut bus = AgentBus::new();
let mut driver = AgentDriver::new();

// Register factories, load stages, run ticks...
```
