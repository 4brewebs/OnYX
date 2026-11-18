//! Orquestación de sesiones, superficies lógicas declaradas y bucle unificado `run_session`.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSnapshotStore, AgentSpec, AgentState, EventBus, Graph, GraphView,
    OnyxSystem, PlatformDriver, RecordingBridge, ReplayBridge, Reply, Responder, SubagentSystem,
    SurfaceBinding, SurfaceId, Vocab, pending_approvals,
};
use onyx_workspace::{PendingCheckpoints, WorkspaceLedgerSystem, WorkspaceTools, WorkspaceVocab};

use crate::tui::TuiHandle;

pub const SURFACE_STATUS: SurfaceId = SurfaceId(1);
pub const SURFACE_TRACE: SurfaceId = SurfaceId(2);
pub const SURFACE_APPROVAL: SurfaceId = SurfaceId(3);
pub const SURFACE_PLAN: SurfaceId = SurfaceId(4);

/// Declara las cuatro superficies lógicas activas para un tick según el estado del grafo.
pub fn active_surfaces(graph: &dyn GraphView, vocab: &Vocab) -> [SurfaceBinding; 4] {
    let has_pending_approval = !pending_approvals(graph, vocab).is_empty();
    let any_active = graph.query(&[vocab.state]).into_iter().any(|id| {
        matches!(
            graph.get(id, vocab.state),
            Some(onyx_soul_agents::Value::Text(ref s))
                if AgentState::parse(s).is_some_and(|st| !st.is_terminal())
        )
    });

    [
        SurfaceBinding {
            surface: SURFACE_STATUS,
            view: vocab.state,
            focused: !has_pending_approval && !any_active,
        },
        SurfaceBinding {
            surface: SURFACE_TRACE,
            view: vocab.rel_memory,
            focused: !has_pending_approval && any_active,
        },
        SurfaceBinding {
            surface: SURFACE_APPROVAL,
            view: vocab.pending_tool,
            focused: has_pending_approval,
        },
        SurfaceBinding {
            surface: SURFACE_PLAN,
            view: vocab.goal,
            focused: false,
        },
    ]
}

/// Extensión interna sobre `PlatformDriver` para consultar progreso de replay o
/// extraer la transcripción grabada sin depender de downcasting de trait objects.
pub trait SessionDriver: PlatformDriver {
    fn replay_remaining(&self) -> Option<usize> {
        None
    }

    fn transcript_bytes(&self) -> Option<Vec<u8>> {
        None
    }
}

impl<R: Responder> SessionDriver for RecordingBridge<R> {
    fn transcript_bytes(&self) -> Option<Vec<u8>> {
        Some(self.transcript().to_bytes())
    }
}

impl SessionDriver for ReplayBridge {
    fn replay_remaining(&self) -> Option<usize> {
        Some(self.remaining())
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    pub snapshot_dir: Option<PathBuf>,
    pub save_transcript: Option<PathBuf>,
    pub save_step_snapshots: bool,
}

pub enum StopPolicy {
    Settled {
        max_ticks: usize,
    },
    ReplayExhausted {
        max_ticks: usize,
    },
    Quit {
        quit: Arc<AtomicBool>,
        tui: TuiHandle,
        llm_in_flight: Arc<AtomicUsize>,
        llm_ready: Arc<AtomicBool>,
    },
}

/// Bucle único de ejecución de ticks para `run`, `replay`, `rewind` y `chat`.
pub fn run_session<D: SessionDriver>(
    host: &mut AgentHost,
    driver: &mut D,
    opts: &SessionOptions,
    stop: StopPolicy,
) -> io::Result<()> {
    match stop {
        StopPolicy::Settled { max_ticks } => {
            for _ in 0..max_ticks {
                if opts.save_step_snapshots && opts.snapshot_dir.is_some() {
                    host.request_snapshot(&host.ticks().to_string());
                }
                let surfaces = active_surfaces(host.graph(), &host.vocab());
                host.step_with(driver, &surfaces);
                if host.is_settled() {
                    break;
                }
            }
            save_transcript_if_configured(opts, driver)?;
        }
        StopPolicy::ReplayExhausted { max_ticks } => {
            for _ in 0..max_ticks {
                if opts.save_step_snapshots && opts.snapshot_dir.is_some() {
                    host.request_snapshot(&host.ticks().to_string());
                }
                let surfaces = active_surfaces(host.graph(), &host.vocab());
                host.step_with(driver, &surfaces);
                if driver.replay_remaining() == Some(0)
                    && (host.is_settled() || all_live_waiting_approval(host.graph(), &host.vocab()))
                {
                    break;
                }
            }
        }
        StopPolicy::Quit {
            quit,
            tui,
            llm_in_flight,
            llm_ready,
        } => {
            tui.render(host.graph());
            let mut needs_follow_up_tick = false;
            let mut stepped_in_session = false;
            while !quit.load(Ordering::Relaxed) {
                let ui_queued = tui.has_queued_events();
                let ready = llm_ready.load(Ordering::Acquire);
                let all_blocked = all_live_agents_blocked(
                    host.graph(),
                    &host.vocab(),
                    llm_in_flight.load(Ordering::Acquire),
                );
                let can_advance = !host.is_settled() && !all_blocked;
                if can_advance || ui_queued || ready || needs_follow_up_tick {
                    let was_settled = host.is_settled();
                    if opts.save_step_snapshots && opts.snapshot_dir.is_some() {
                        host.request_snapshot(&host.ticks().to_string());
                    }
                    let surfaces = active_surfaces(host.graph(), &host.vocab());
                    host.step_with(driver, &surfaces);
                    stepped_in_session = true;
                    // Un `IngressEvent::Command` tarda 1 tick en convertirse en
                    // `spawn_event` (por `CommandSpawnerSystem`) y otro en crear
                    // el agente en el grafo (por `AgentSpawner`).
                    needs_follow_up_tick = ui_queued;
                    if !was_settled && host.is_settled() {
                        save_latest_snapshot(opts, host);
                        let _ = save_transcript_if_configured(opts, driver);
                    }
                } else {
                    tui.pump_idle(host.graph());
                }
            }
            if stepped_in_session {
                save_latest_snapshot(opts, host);
            }
            save_transcript_if_configured(opts, driver)?;
        }
    }
    Ok(())
}

fn save_latest_snapshot(opts: &SessionOptions, host: &AgentHost) {
    if let Some(dir) = &opts.snapshot_dir {
        if host.ticks() > 0 {
            let _ = AgentSnapshotStore::new(dir)
                .save_tick(host.ticks().saturating_sub(1), &host.graph().export());
        }
    }
}

fn save_transcript_if_configured<D: SessionDriver>(
    opts: &SessionOptions,
    driver: &D,
) -> io::Result<()> {
    if let Some(path) = &opts.save_transcript {
        if let Some(bytes) = driver.transcript_bytes() {
            fs::write(path, bytes)?;
        }
    }
    Ok(())
}

/// Driver CLI para aprobaciones en modo `run`: se envuelve dentro de
/// `RecordingBridge` para que `Approve`/`Deny` queden grabados en `Transcript`.
pub struct CliApprovalDriver {
    vocab: Vocab,
    inner: Box<dyn PlatformDriver>,
    seen: BTreeSet<(u64, u64)>,
    queued: Vec<Reply>,
}

impl CliApprovalDriver {
    pub fn new(vocab: Vocab, inner: impl PlatformDriver + 'static) -> Self {
        Self {
            vocab,
            inner: Box::new(inner),
            seen: BTreeSet::new(),
            queued: Vec::new(),
        }
    }
}

impl PlatformDriver for CliApprovalDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        self.inner.poll(bus);
        for reply in self.queued.drain(..) {
            bus.push(reply.to_event());
        }
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        self.inner.present(graph, surfaces);
        for view in pending_approvals(graph, &self.vocab) {
            if !self.seen.insert((view.agent.0, view.request)) {
                continue;
            }
            if let Some(pending) = &view.pending_approval {
                let reply = if prompt_approval(&pending.tool, &pending.args) {
                    Reply::approve(view.agent, view.request)
                } else {
                    Reply::deny(view.agent, view.request, "denied by user")
                };
                self.queued.push(reply);
            }
        }
    }
}

fn prompt_approval(tool: &str, args: &str) -> bool {
    println!("\n[openOnyx] Approval required for `{tool}`:");
    println!("  args: {args}");
    print!("Approve? [y/N]: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn all_live_waiting_approval(graph: &dyn GraphView, v: &Vocab) -> bool {
    all_live_agents_blocked(graph, v, 0)
}

pub fn all_live_agents_blocked(graph: &dyn GraphView, v: &Vocab, llm_in_flight: usize) -> bool {
    let mut any_live = false;
    for id in graph.query(&[v.state]) {
        if let Some(onyx_soul_agents::Value::Text(s)) = graph.get(id, v.state) {
            if let Some(state) = AgentState::parse(&s) {
                if !state.is_terminal() {
                    any_live = true;
                    let blocked = matches!(
                        state,
                        AgentState::WaitingApproval | AgentState::WaitingChildren
                    ) || (state == AgentState::Thinking && llm_in_flight > 0);
                    if !blocked {
                        return false;
                    }
                }
            }
        }
    }
    any_live
}

pub fn build_host(
    seed: u64,
    config: AgentConfig,
    snapshot_dir: Option<&PathBuf>,
    extra_system: Option<Box<dyn OnyxSystem>>,
) -> Result<(AgentHost, WorkspaceVocab), Box<dyn std::error::Error>> {
    build_host_with_checkpoints(
        seed,
        config,
        snapshot_dir,
        extra_system,
        Some(PendingCheckpoints::default()),
    )
}

pub fn build_host_with_checkpoints(
    seed: u64,
    config: AgentConfig,
    snapshot_dir: Option<&PathBuf>,
    extra_system: Option<Box<dyn OnyxSystem>>,
    checkpoints: Option<PendingCheckpoints>,
) -> Result<(AgentHost, WorkspaceVocab), Box<dyn std::error::Error>> {
    let mut ledger = WorkspaceLedgerSystem::new();
    if let Some(cp) = checkpoints {
        ledger = ledger.with_checkpoints(cp);
    }
    let mut builder = AgentHost::builder(seed)
        .config(config.clone())
        .system(Box::new(SubagentSystem::new(config)))
        .system(Box::new(ledger));
    if let Some(sys) = extra_system {
        builder = builder.system(sys);
    }
    if let Some(dir) = snapshot_dir {
        builder = builder.store(Box::new(AgentSnapshotStore::new(dir)));
    }
    let mut host = builder.build()?;
    let ws_vocab = WorkspaceVocab::resolve_with(|name| host.attr(name));
    Ok((host, ws_vocab))
}

pub fn make_spec(goal: &str, auto_approve: bool) -> AgentSpec {
    let mut spec = AgentSpec::new("openonyx", goal).with_tools(
        WorkspaceTools::all_tool_names_with_subagents()
            .iter()
            .copied(),
    );
    if !auto_approve {
        spec = spec.with_approval_tools(WorkspaceTools::approval_required_names().iter().copied());
    }
    spec
}
