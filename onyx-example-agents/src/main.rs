//! Example application on `onyx-soul-agents`.
//!
//! Walks one agent through the public API of this soul (the crate does not
//! depend on `onyx-core`):
//! - `AgentHost` with a deterministic `ToolExecutor`
//! - an external decider: `say` → `call` → `observe` → `done`
//! - `RecordingBridge::wrapping` a `HeadlessDriver`, driven with `step_with`
//!   and a `SurfaceBinding` (`run` and `run_until_settled` pass an empty list)
//! - a snapshot after `say`, requested before the step that writes it, so
//!   there is no in-flight tool event
//! - a second host that opens the same `AgentSnapshotStore` directory,
//!   restores, `set_tick` and `ReplayBridge::starting_at`

use std::fs;

use onyx_soul_agents::{
    AgentHost, AgentSnapshotStore, AgentSpec, AgentView, HeadlessDriver, PlatformDriver,
    RecordingBridge, ReplayBridge, Reply, SurfaceBinding, SurfaceId, ToolCall, ToolCtx,
    ToolExecutor,
};

const SEED: u64 = 1;
const SNAP_DIR: &str = "snapshots";
const SNAP_KEY: &str = "mid";
const MAX_STEPS: usize = 32;

struct Search;

impl ToolExecutor for Search {
    fn name(&self) -> &str {
        "search"
    }

    fn call(&mut self, call: &ToolCall, _: &mut ToolCtx) -> Result<String, String> {
        Ok(format!("{}:ok", call.tool))
    }
}

fn decide(req: &AgentView) -> Option<Reply> {
    let has = |kind: &str| req.memory.iter().any(|(k, _)| k == kind);
    if has("observe") {
        Some(Reply::done(req.agent, req.request, "ok"))
    } else if has("call") {
        // The tool result arrives as `observe` on a later tick; the bridge
        // must not invent another reply while that event is in flight.
        None
    } else if has("say") {
        Some(Reply::call(req.agent, req.request, "search q"))
    } else {
        Some(Reply::say(req.agent, req.request, "thinking"))
    }
}

fn declared_surfaces(host: &mut AgentHost) -> [SurfaceBinding; 1] {
    let view = host.attr("view.main");
    [SurfaceBinding {
        surface: SurfaceId(1),
        view,
        focused: true,
    }]
}

fn report(host: &AgentHost, label: &str) {
    let n = host.ticks().saturating_sub(1);
    match host.agents().first().copied() {
        Some(id) => {
            let view = host.view(id);
            let memory: Vec<&str> = view.memory.iter().map(|(kind, _)| kind.as_str()).collect();
            println!(
                "{label} tick {n}: state={:?} memory={memory:?} result={:?}",
                host.agent_state(id),
                host.result(id)
            );
        }
        None => println!("{label} tick {n}: no agents yet"),
    }
}

fn drive_until_settled(
    host: &mut AgentHost,
    driver: &mut dyn PlatformDriver,
    surfaces: &[SurfaceBinding],
    label: &str,
    max: usize,
) -> bool {
    for _ in 0..max {
        if host.is_settled() {
            return true;
        }
        host.step_with(driver, surfaces);
        report(host, label);
    }
    host.is_settled()
}

fn build_host() -> Result<AgentHost, Box<dyn std::error::Error>> {
    Ok(AgentHost::builder(SEED)
        .tool(Box::new(Search))
        .store(Box::new(AgentSnapshotStore::new(SNAP_DIR)))
        .build()?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = fs::remove_dir_all(SNAP_DIR);
    fs::create_dir_all(SNAP_DIR)?;

    let mut recorded = build_host()?;
    recorded.spawn(
        &AgentSpec::new("alice", "research topic A")
            .with_tools(["search"])
            .with_max_steps(8)
            .with_timeout_ticks(16),
    );

    let surfaces = declared_surfaces(&mut recorded);
    let headless = HeadlessDriver::new();
    let monitor = headless.handle();
    let mut rec = RecordingBridge::new(recorded.vocab(), decide).wrapping(headless);

    // First step: the spawn lands and the agent becomes Thinking. Ask for
    // the snapshot before the step that writes `say`.
    recorded.step_with(&mut rec, &surfaces);
    report(&recorded, "record");
    recorded.request_snapshot(SNAP_KEY);
    recorded.step_with(&mut rec, &surfaces);
    report(&recorded, "record");
    let n = recorded.ticks();
    let mid = recorded.fingerprint();
    println!("snapshot {SNAP_KEY} at next n={n}");

    if !drive_until_settled(&mut recorded, &mut rec, &surfaces, "record", MAX_STEPS) {
        return Err("recording did not settle".into());
    }
    let done = recorded.fingerprint();
    let transcript = rec.transcript().clone();
    let id = recorded.agents()[0];
    println!(
        "recorded: result={:?} present_count={} last_surfaces={} transcript={}",
        recorded.result(id),
        monitor.present_count(),
        monitor.last_surfaces().len(),
        transcript.entries.len()
    );
    if monitor.last_surfaces().is_empty() {
        return Err("headless saw no surfaces; the loop must use step_with".into());
    }
    if mid == done {
        return Err("snapshot was taken too late; mid equals the finished graph".into());
    }

    // Fresh host, same directory, empty bus. Restore does not realign `n`.
    let mut restored = build_host()?;
    let surfaces = declared_surfaces(&mut restored);
    restored.request_restore(SNAP_KEY);
    restored.step_with(&mut HeadlessDriver::new(), &surfaces);
    report(&restored, "restore");
    if restored.fingerprint() != mid {
        return Err("restore did not return to the snapshotted graph".into());
    }
    if restored.ticks() == n {
        return Err("request_restore must not realign n".into());
    }
    restored.set_tick(n);
    let mut replay = ReplayBridge::new(transcript).starting_at(n);
    if !drive_until_settled(&mut restored, &mut replay, &surfaces, "replay", MAX_STEPS) {
        return Err("replay after restore did not settle".into());
    }
    let match_fp = restored.fingerprint() == done;
    println!(
        "restore+set_tick+starting_at: fingerprints_match={match_fp} result={:?}",
        restored.result(restored.agents()[0])
    );
    if !match_fp {
        return Err("fingerprints diverged after restore".into());
    }
    println!("ok");
    Ok(())
}
