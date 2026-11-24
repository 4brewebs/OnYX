//! Orquestación de sesiones, superficies lógicas declaradas y bucle unificado `run_session`.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSpec, AgentState, ConceptId, EventBus, Graph, GraphView,
    HeadlessDriver, OnyxSystem, PlatformDriver, RecordingBridge, ReplayBridge, Reply, Responder,
    SubagentSystem, SurfaceBinding, SurfaceId, Vocab, pending_approvals,
};
use onyx_workspace::{
    ImpactAssessment, PendingCheckpoints, PermissionDecision, WorkspaceLedgerSystem, WorkspaceMode,
    WorkspaceModeHandle, WorkspacePolicy, WorkspaceTools, WorkspaceVocab, graph_workspace_mode,
    graph_workspace_services,
};

use crate::command::{PendingSessionAction, PendingSystemNotes, SessionAction};
use crate::external_driver::ExternalDriverApprovalPolicy;
use crate::ops::reconcile_rewind_effects_report;
use crate::pack::{
    MilestoneSnapshotSystem, PackSnapshotStore, SharedPackStore, default_fork_dir,
    fork_pack_until_tick,
};
use crate::subagent::{MergePreviewState, SubagentHandle, effective_merge_decision};

/// Interfaz mínima de bombeo UI compartida entre `TuiHandle` (`openonyx chat`) y
/// `WebHandle` (`openonyx serve`) para que ambos corran sobre el mismo bucle determinista
/// `run_session` sin fugas de ticks.
pub trait UiIdlePump {
    fn render(&self, graph: &dyn GraphView);
    fn has_queued_events(&self) -> bool;
    fn pump_idle(&self, graph: &dyn GraphView);
}

pub const SURFACE_STATUS: SurfaceId = SurfaceId(1);
pub const SURFACE_TRACE: SurfaceId = SurfaceId(2);
pub const SURFACE_APPROVAL: SurfaceId = SurfaceId(3);
pub const SURFACE_PLAN: SurfaceId = SurfaceId(4);
/// `SDD-03` Fase 3: superficie de servicios Sprout, activa solo si el grafo tiene
/// `ws.services.text` no vacío (vista `F3 Sprout`).
pub const SURFACE_SERVICES: SurfaceId = SurfaceId(5);

/// Declara las superficies lógicas activas para un tick según el estado del grafo.
/// `SURFACE_SERVICES` se añade cuando existen servicios registrados
/// (`ws.services.text` no vacío); el resto de constantes y bindings son
/// invariantes (orden estable para replay).
pub fn active_surfaces(
    graph: &dyn GraphView,
    vocab: &Vocab,
    ws_vocab: &WorkspaceVocab,
) -> Vec<SurfaceBinding> {
    let has_pending_approval = !pending_approvals(graph, vocab).is_empty();
    let any_active = graph.query(&[vocab.state]).into_iter().any(|id| {
        matches!(
            graph.get(id, vocab.state),
            Some(onyx_soul_agents::Value::Text(ref s))
                if AgentState::parse(s).is_some_and(|st| !st.is_terminal())
        )
    });

    let mut surfaces = vec![
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
    ];
    if graph_workspace_services(graph, ws_vocab).is_some() {
        surfaces.push(SurfaceBinding {
            surface: SURFACE_SERVICES,
            view: ws_vocab.services_text,
            focused: false,
        });
    }
    surfaces
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
    pub snapshot_every: Option<u64>,
    pub max_snapshots: Option<usize>,
}

/// Contexto necesario para ejecutar `/rewind <tick>`, `/fork`, `/merge` y `/discard` en caliente dentro de `chat` / `serve`.
#[derive(Clone)]
pub struct SessionActionContext {
    pub workspace_root: PathBuf,
    pub config: AgentConfig,
    pub ws_vocab: WorkspaceVocab,
    pub mode_handle: WorkspaceModeHandle,
    pub tools: WorkspaceTools,
    pub snapshot_store: Option<SharedPackStore>,
    pub subagents: Option<SubagentHandle>,
    pub pending_action: PendingSessionAction,
    pub pending_notes: PendingSystemNotes,
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
        ui: Box<dyn UiIdlePump>,
        llm_in_flight: Arc<AtomicUsize>,
        llm_ready: Arc<AtomicBool>,
        bridge_ready: Arc<AtomicBool>,
        action_ctx: Option<SessionActionContext>,
    },
}

/// Bucle único de ejecución de ticks para `run`, `replay`, `rewind` y `chat`.
pub fn run_session<D: SessionDriver>(
    host: &mut AgentHost,
    driver: &mut D,
    opts: &SessionOptions,
    stop: StopPolicy,
) -> io::Result<()> {
    // `SDD-03` Fase 3: `SURFACE_SERVICES` depende de `ws.services.text`; el
    // vocabulario workspace es determinista por nombre (misma resolución que
    // `build_host_with_shared_store`).
    let ws_vocab = WorkspaceVocab::resolve_with(|name| host.attr(name));
    match stop {
        StopPolicy::Settled { max_ticks } => {
            for _ in 0..max_ticks {
                let surfaces = active_surfaces(host.graph(), &host.vocab(), &ws_vocab);
                host.step_with(driver, &surfaces);
                if host.is_settled() {
                    break;
                }
            }
            save_latest_snapshot(opts, host);
            save_transcript_if_configured(opts, driver)?;
        }
        StopPolicy::ReplayExhausted { max_ticks } => {
            for _ in 0..max_ticks {
                let surfaces = active_surfaces(host.graph(), &host.vocab(), &ws_vocab);
                host.step_with(driver, &surfaces);
                if driver.replay_remaining() == Some(0)
                    && (host.is_settled() || all_live_waiting_approval(host.graph(), &host.vocab()))
                {
                    break;
                }
            }
            save_latest_snapshot(opts, host);
        }
        StopPolicy::Quit {
            quit,
            ui,
            llm_in_flight,
            llm_ready,
            bridge_ready,
            action_ctx,
        } => {
            ui.render(host.graph());
            let mut needs_follow_up_tick = false;
            let mut stepped_in_session = false;
            let shared_store = action_ctx.as_ref().and_then(|c| c.snapshot_store.clone());
            // `SDD-04` 4C-audit: `Acquire` emparejado con el `Release` del
            // watcher de apago de `serve` (`spawn_shutdown_watcher`).
            while !quit.load(Ordering::Acquire) {
                let ui_queued = ui.has_queued_events();
                let ready =
                    llm_ready.load(Ordering::Acquire) || bridge_ready.load(Ordering::Acquire);
                let all_blocked = all_live_agents_blocked(
                    host.graph(),
                    &host.vocab(),
                    llm_in_flight.load(Ordering::Acquire),
                );
                let can_advance = !host.is_settled() && !all_blocked;
                if can_advance || ui_queued || ready || needs_follow_up_tick {
                    let was_settled = host.is_settled();
                    let surfaces = active_surfaces(host.graph(), &host.vocab(), &ws_vocab);
                    host.step_with(driver, &surfaces);
                    stepped_in_session = true;
                    // Un `IngressEvent::Command` tarda 1 tick en convertirse en
                    // `spawn_event` (por `CommandSpawnerSystem`) y otro en crear
                    // el agente en el grafo (por `AgentSpawner`).
                    needs_follow_up_tick = ui_queued;

                    if let Some(ctx) = &action_ctx {
                        if let Some(action) = ctx.pending_action.take() {
                            if matches!(action, SessionAction::Rewind { .. })
                                && llm_in_flight.load(Ordering::Acquire) > 0
                            {
                                emit_session_note(
                                    host,
                                    &ctx.pending_notes,
                                    "[/rewind] Cannot rewind while an LLM request is in flight; wait until settled.",
                                );
                            } else {
                                let _ = apply_session_action(host, action, ctx);
                                needs_follow_up_tick = false;
                            }
                            ui.render(host.graph());
                        }
                    }

                    if !was_settled && host.is_settled() {
                        save_latest_snapshot_with_store(opts, shared_store.as_ref(), host);
                        let _ = save_transcript_if_configured(opts, driver);
                    }
                } else {
                    ui.pump_idle(host.graph());
                }
            }
            if stepped_in_session {
                save_latest_snapshot_with_store(opts, shared_store.as_ref(), host);
            }
            save_transcript_if_configured(opts, driver)?;
        }
    }
    Ok(())
}

/// Ejecuta de forma atómica una acción `/fork` o `/rewind <tick>` sobre el host activo (`REQ-03.2`, `REQ-03.3`).
///
/// Garantiza:
/// - Nunca muta ni sobrescribe el `snapshots.onyxpack` de la línea temporal anterior.
/// - Reconcilia el working tree Git en silencio (sin `println!` sobre la TUI).
/// - Resetea `seen_reads` y sincroniza `WorkspaceModeHandle` tras restaurar.
pub fn apply_session_action(
    host: &mut AgentHost,
    action: SessionAction,
    ctx: &SessionActionContext,
) -> String {
    match &action {
        SessionAction::MergeSubagent { job_id } => {
            let Some(ref sub) = ctx.subagents else {
                let msg = "[/merge] Async subagents are unavailable in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return msg;
            };
            let msg = match sub.merge_job(job_id) {
                Ok(report) => {
                    ctx.pending_notes
                        .push_subagent_status(report.job_id.clone(), "merged");
                    let impact_suffix = report
                        .impact
                        .as_ref()
                        .map(|imp| format!(" {}", imp.summary_line))
                        .unwrap_or_default();
                    if report.was_noop {
                        format!(
                            "[/merge] Subagent `{}` had no net file changes; worktree cleaned.{impact_suffix}",
                            report.job_id
                        )
                    } else {
                        format!(
                            "[/merge] Merged subagent `{}` into root ({} file(s): {}).{impact_suffix}",
                            report.job_id,
                            report.changed_files.len(),
                            report.changed_files.join(", ")
                        )
                    }
                }
                Err(err) => format!("[/merge] ERROR: {err}"),
            };
            emit_session_note(host, &ctx.pending_notes, &msg);
            return msg;
        }
        SessionAction::DiscardSubagent { job_id } => {
            let Some(ref sub) = ctx.subagents else {
                let msg = "[/discard] Async subagents are unavailable in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return msg;
            };
            let msg = match sub.discard_job(job_id) {
                Ok(summary) => {
                    ctx.pending_notes
                        .push_subagent_status(job_id.as_str(), "discarded");
                    format!("[/discard] {summary}")
                }
                Err(err) => format!("[/discard] ERROR: {err}"),
            };
            emit_session_note(host, &ctx.pending_notes, &msg);
            return msg;
        }
        SessionAction::SpawnSubagent { goal, driver } => {
            // `SDD-10` Fase 4: `/spawn` crea un worktree → bloqueado fuera de BUILD
            // (mismo gate que `/merge`).
            if !ctx.mode_handle.get().allows_mutation() {
                let msg = format!(
                    "[/spawn] Blocked in `{}` mode: switch to BUILD to spawn a background subagent.",
                    ctx.mode_handle.get().as_label()
                );
                emit_session_note(host, &ctx.pending_notes, &msg);
                return msg;
            }
            let Some(ref sub) = ctx.subagents else {
                let msg = "[/spawn] Async subagents are unavailable in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return msg;
            };
            // El job queda registrado en el grafo al completar (notificación proactiva);
            // aquí solo lanzamos: 0 trabajo del grafo, 0 turno de `root`.
            let msg = match sub.spawn_job(
                goal,
                None,
                None,
                host.ticks(),
                Vec::new(),
                driver.as_deref(),
            ) {
                Ok(summary) => {
                    let driver_note = match driver.as_deref() {
                        Some(d) if !d.eq_ignore_ascii_case("native") => format!(" on driver `{d}`"),
                        _ => String::new(),
                    };
                    format!(
                        "[/spawn] Launched background worktree job `[{job_id}]`{driver_note}; it runs without spending a root turn (check `/subagents`).",
                        job_id = summary.job_id
                    )
                }
                Err(err) => format!("[/spawn] ERROR: {err}"),
            };
            emit_session_note(host, &ctx.pending_notes, &msg);
            return msg;
        }
        SessionAction::Rewind { .. } | SessionAction::Fork { .. } => {}
    }

    let Some(store) = &ctx.snapshot_store else {
        let msg =
            "[session] Snapshot persistence is disabled; pass `--snapshot-dir <dir>`.".to_string();
        emit_session_note(host, &ctx.pending_notes, &msg);
        return msg;
    };

    match action {
        SessionAction::Fork { label } => {
            let src_dir = store.current_dir();
            let current_tick = if host.ticks() > 0 {
                let tick = host.ticks().saturating_sub(1);
                let _ = store.save_milestone(tick, "cmd:/fork", &host.graph().export());
                tick
            } else {
                store.latest_tick().unwrap_or(0)
            };
            let dest_dir = default_fork_dir(&src_dir, current_tick, label.as_deref());
            let note = match fork_pack_until_tick(&src_dir, &dest_dir, current_tick) {
                Ok(report) => {
                    store.switch_dir(&report.dest_dir);
                    format!(
                        "[/fork] Created branch `{}` with {} milestone(s) up to tick {} (original `{}` preserved).",
                        report.dest_dir.display(),
                        report.copied_snapshots,
                        report.resolved_tick,
                        report.source_dir.display()
                    )
                }
                Err(err) => format!("[/fork] ERROR: {err}"),
            };
            emit_session_note(host, &ctx.pending_notes, &note);
            note
        }
        SessionAction::Rewind { requested_tick } => {
            if !host.is_settled() {
                let msg = "[/rewind] Cannot rewind while an agent is active; wait until settled."
                    .to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return msg;
            }
            let src_dir = store.current_dir();
            let Some(target_tick) = store.resolve_tick_or_floor(requested_tick) else {
                let msg = format!(
                    "[/rewind] No milestone snapshot found at or before tick {requested_tick} in `{}`.",
                    src_dir.display()
                );
                emit_session_note(host, &ctx.pending_notes, &msg);
                return msg;
            };
            let latest_tick = store.latest_tick();

            let rewind_report = match reconcile_rewind_effects_report(
                &ctx.workspace_root,
                &src_dir,
                target_tick,
                &ctx.config,
                false,
            ) {
                Ok(r) => r,
                Err(err) => {
                    let msg = format!("[/rewind] ERROR reconciling workspace git state: {err}");
                    emit_session_note(host, &ctx.pending_notes, &msg);
                    return msg;
                }
            };

            let mut branch_note = String::new();
            if latest_tick.is_some_and(|latest| latest > target_tick) {
                let fork_dir = default_fork_dir(&src_dir, target_tick, None);
                match fork_pack_until_tick(&src_dir, &fork_dir, target_tick) {
                    Ok(report) => {
                        store.switch_dir(&report.dest_dir);
                        branch_note = format!(
                            " Branched snapshots into `{}` ({} milestones, original `{}` preserved).",
                            report.dest_dir.display(),
                            report.copied_snapshots,
                            src_dir.display()
                        );
                    }
                    Err(err) => {
                        let msg = format!("[/rewind] ERROR forking snapshot pack: {err}");
                        emit_session_note(host, &ctx.pending_notes, &msg);
                        return msg;
                    }
                }
            }

            host.request_restore(&target_tick.to_string());
            host.step(&mut HeadlessDriver::new());
            host.set_tick(target_tick.saturating_add(1));

            if let Some(restored_mode) = graph_workspace_mode(host.graph(), &ctx.ws_vocab) {
                ctx.mode_handle.set(restored_mode);
            }
            ctx.tools.clear_seen_reads();

            let mut summary = if target_tick != requested_tick {
                format!(
                    "[/rewind] Restored milestone tick {target_tick} (requested {requested_tick}); clock at tick {}.",
                    host.ticks()
                )
            } else {
                format!(
                    "[/rewind] Restored milestone tick {target_tick}; clock at tick {}.",
                    host.ticks()
                )
            };
            if let Some(sha) = &rewind_report.restored_tree_sha {
                summary.push_str(&format!(
                    " Restored git tree `{sha}` ({} orphaned effect(s)).",
                    rewind_report.orphaned_count
                ));
            }
            summary.push_str(&branch_note);
            emit_session_note(host, &ctx.pending_notes, &summary);
            summary
        }
        SessionAction::MergeSubagent { .. }
        | SessionAction::DiscardSubagent { .. }
        | SessionAction::SpawnSubagent { .. } => {
            unreachable!("subagent session actions return early above")
        }
    }
}

fn emit_session_note(host: &mut AgentHost, pending_notes: &PendingSystemNotes, note: &str) {
    pending_notes.push(note.to_string());
    let preserved_tick = host.ticks();
    host.step(&mut HeadlessDriver::new());
    host.set_tick(preserved_tick);
}

fn save_latest_snapshot(opts: &SessionOptions, host: &AgentHost) {
    save_latest_snapshot_with_store(opts, None, host);
}

fn save_latest_snapshot_with_store(
    opts: &SessionOptions,
    shared_store: Option<&SharedPackStore>,
    host: &AgentHost,
) {
    if host.ticks() == 0 {
        return;
    }
    let tick = host.ticks().saturating_sub(1);
    if let Some(store) = shared_store {
        let _ = store.save_milestone(tick, "exit", &host.graph().export());
    } else if let Some(dir) = &opts.snapshot_dir {
        let mut store = PackSnapshotStore::new(dir).with_max_snapshots(opts.max_snapshots);
        let _ = store.save_milestone(tick, "exit", &host.graph().export());
    }
}

fn save_transcript_if_configured<D: SessionDriver>(
    opts: &SessionOptions,
    driver: &D,
) -> io::Result<()> {
    if let Some(path) = &opts.save_transcript {
        if let Some(bytes) = driver.transcript_bytes() {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::write(path, bytes)?;
        }
    }
    Ok(())
}

/// Driver CLI para aprobaciones en modo `run`: se envuelve dentro de
/// `RecordingBridge` para que `Approve`/`Deny` queden grabados en `Transcript`.
pub struct CliApprovalDriver {
    vocab: Vocab,
    policy: Option<WorkspacePolicy>,
    tools: Option<WorkspaceTools>,
    subagents: Option<SubagentHandle>,
    inner: Box<dyn PlatformDriver>,
    seen: BTreeSet<(u64, u64)>,
    queued: Vec<Reply>,
}

impl CliApprovalDriver {
    pub fn new(vocab: Vocab, inner: impl PlatformDriver + 'static) -> Self {
        Self {
            vocab,
            policy: None,
            tools: None,
            subagents: None,
            inner: Box::new(inner),
            seen: BTreeSet::new(),
            queued: Vec::new(),
        }
    }

    pub fn with_policy(mut self, policy: WorkspacePolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn with_tools(mut self, tools: WorkspaceTools) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn with_subagents(mut self, subagents: SubagentHandle) -> Self {
        self.subagents = Some(subagents);
        self
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
            let Some(pending) = &view.pending_approval else {
                continue;
            };
            let mut preview_diff: Option<String> = None;
            let impact = if pending.tool == "subagent_merge" {
                let static_dec = self.policy.as_ref().map_or(PermissionDecision::Ask, |pol| {
                    pol.evaluate_permission_in_mode(
                        WorkspaceMode::Build,
                        &pending.tool,
                        &pending.args,
                    )
                });
                let job_id = serde_json::from_str::<serde_json::Value>(&pending.args)
                    .ok()
                    .and_then(|v| v.get("job_id").and_then(|j| j.as_str()).map(str::to_string));
                let preview = job_id.and_then(|jid| {
                    self.subagents.as_ref().and_then(|h| {
                        h.preview_merge_for_request(&jid, view.agent.0, view.request)
                            .ok()
                    })
                });
                match preview {
                    Some(MergePreviewState::Clean(ticket)) => {
                        // `SDD-10` Fase 4: la decisión efectiva es la misma que en las
                        // superficies TUI/Web (delta de gobernanza → `Ask` obligatorio);
                        // se calcula **antes** de mover los campos del ticket.
                        if effective_merge_decision(static_dec, &ticket) != PermissionDecision::Ask
                        {
                            continue;
                        }
                        let imp = ticket.impact;
                        preview_diff = Some(ticket.unified_diff);
                        imp
                    }
                    Some(MergePreviewState::NoOp) => {
                        if static_dec != PermissionDecision::Ask {
                            continue;
                        }
                        None
                    }
                    // `Conflict`, `Err` o `job_id` ausente: no pedir `y/n`.
                    // `SubagentManagerDriver` auto-aprueba la solicitud para que
                    // `subagent_merge` se ejecute y devuelva el error exacto (rutas en
                    // conflicto, estado del job) a `root`, conservando el worktree
                    // (igual que TUI y Web; `SDD-10` Paso 0).
                    // `ControlPlaneDenied`: no hay nada que aprobar; `SubagentManagerDriver`
                    // niega la solicitud de plano (`SDD-10` Fase 3).
                    Some(
                        MergePreviewState::Conflict { .. }
                        | MergePreviewState::ControlPlaneDenied { .. },
                    )
                    | None => continue,
                }
            } else if let Some(tools) = &self.tools {
                let (decision, imp) = tools.evaluate_permission_with_impact(
                    WorkspaceMode::Build,
                    &pending.tool,
                    &pending.args,
                );
                if decision != PermissionDecision::Ask {
                    continue;
                }
                imp
            } else {
                if let Some(policy) = &self.policy {
                    if policy.evaluate_permission_in_mode(
                        WorkspaceMode::Build,
                        &pending.tool,
                        &pending.args,
                    ) != PermissionDecision::Ask
                    {
                        continue;
                    }
                }
                None
            };
            if !self.seen.insert((view.agent.0, view.request)) {
                continue;
            }
            let reply = if prompt_approval(
                &pending.tool,
                &pending.args,
                impact.as_ref(),
                preview_diff.as_deref(),
            ) {
                Reply::approve(view.agent, view.request)
            } else {
                Reply::deny(view.agent, view.request, "denied by user")
            };
            self.queued.push(reply);
        }
    }
}

fn prompt_approval(
    tool: &str,
    args: &str,
    impact: Option<&ImpactAssessment>,
    diff_preview: Option<&str>,
) -> bool {
    println!("\n[openOnyx] Approval required for `{tool}`:");
    if let Some(imp) = impact {
        println!("  {}", imp.summary_line);
    }
    println!("  args: {args}");
    if let Some(diff) = diff_preview {
        for line in diff.lines().take(24) {
            println!("  {line}");
        }
    }
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
    let live_ids: Vec<(ConceptId, AgentState)> = graph
        .query(&[v.state])
        .into_iter()
        .filter_map(|id| {
            let s = match graph.get(id, v.state) {
                Some(onyx_soul_agents::Value::Text(s)) => s,
                _ => return None,
            };
            let state = AgentState::parse(&s)?;
            (!state.is_terminal()).then_some((id, state))
        })
        .collect();

    if live_ids.is_empty() {
        return false;
    }

    for &(id, state) in &live_ids {
        let blocked = match state {
            AgentState::WaitingApproval => true,
            AgentState::WaitingChildren => {
                // Solo bloqueamos si existe al menos un subagente vivo en el grafo.
                // Si hay 0 hijos vivos, hay un `ev_spawn`, `ev_rejected` o `ev_tool_result`
                // en el EventBus esperando el siguiente tick para crear al hijo o despertar al padre.
                live_ids.iter().any(|(child, _)| {
                    *child != id && graph.related(*child, v.rel_parent).contains(&id)
                })
            }
            AgentState::Thinking => llm_in_flight > 0,
            _ => false,
        };
        if !blocked {
            return false;
        }
    }
    true
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
    let opts = SessionOptions {
        snapshot_dir: snapshot_dir.cloned(),
        ..SessionOptions::default()
    };
    build_host_with_options(seed, config, &opts, extra_system, checkpoints)
}

pub fn build_host_with_options(
    seed: u64,
    config: AgentConfig,
    opts: &SessionOptions,
    extra_system: Option<Box<dyn OnyxSystem>>,
    checkpoints: Option<PendingCheckpoints>,
) -> Result<(AgentHost, WorkspaceVocab), Box<dyn std::error::Error>> {
    build_host_with_shared_store(seed, config, opts, extra_system, checkpoints, None)
}

pub fn build_host_with_shared_store(
    seed: u64,
    config: AgentConfig,
    opts: &SessionOptions,
    extra_system: Option<Box<dyn OnyxSystem>>,
    checkpoints: Option<PendingCheckpoints>,
    shared_store: Option<SharedPackStore>,
) -> Result<(AgentHost, WorkspaceVocab), Box<dyn std::error::Error>> {
    let mut ledger = WorkspaceLedgerSystem::new();
    if let Some(cp) = checkpoints {
        ledger = ledger.with_checkpoints(cp);
    }
    let mut builder = AgentHost::builder(seed)
        .config(config.clone())
        .system(Box::new(SubagentSystem::new(config)))
        .system(Box::new(ledger))
        // `SDD-10` Fase 1: fuerza `WaitingApproval` solo para `spawn_subagent` con un
        // driver externo no-nativo (el gate de trust/reglas decide Allow/Deny/Ask después).
        .approval(Box::new(ExternalDriverApprovalPolicy));
    if let Some(sys) = extra_system {
        builder = builder.system(sys);
    }
    if let Some(store) = shared_store {
        if opts.save_step_snapshots {
            builder = builder.system(Box::new(MilestoneSnapshotSystem::new(opts.snapshot_every)));
        }
        builder = builder.store(Box::new(store));
    } else if let Some(dir) = &opts.snapshot_dir {
        if opts.save_step_snapshots {
            builder = builder.system(Box::new(MilestoneSnapshotSystem::new(opts.snapshot_every)));
        }
        builder = builder.store(Box::new(
            PackSnapshotStore::new(dir).with_max_snapshots(opts.max_snapshots),
        ));
    }
    let mut host = builder.build()?;
    let ws_vocab = WorkspaceVocab::resolve_with(|name| host.attr(name));
    Ok((host, ws_vocab))
}

pub fn make_spec(goal: &str, auto_approve: bool, mcp_tools: &[String]) -> AgentSpec {
    let mut tool_names: Vec<&str> = WorkspaceTools::all_tool_names_with_subagents().to_vec();
    // `SDD-04 REQ-04.3`: tools MCP `mcp__*` del registro de arranque (orden
    // determinista: built-in primero, MCP después).
    tool_names.extend(mcp_tools.iter().map(|t| t.as_str()));
    let mut spec = AgentSpec::new("openonyx", goal).with_tools(tool_names);
    if !auto_approve {
        // `SDD-04 REQ-04.4`: los tools MCP `mcp__*` requieren aprobación cuando no
        // hay auto-approve (misma regla que los built-in mutantes).
        let mut approval: Vec<&str> = WorkspaceTools::approval_required_names().to_vec();
        approval.extend(mcp_tools.iter().map(|t| t.as_str()));
        spec = spec.with_approval_tools(approval);
    }
    spec
}
