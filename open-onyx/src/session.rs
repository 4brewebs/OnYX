//! Orquestación de sesiones, superficies lógicas declaradas y bucle unificado `run_session`.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use onyx_soul_agents::{
    AgentConfig, AgentGraph, AgentHost, AgentSpec, AgentState, ConceptId, EventBus, Graph,
    GraphView, HeadlessDriver, IngressEvent, OnyxEvent, OnyxSystem, PlatformDriver,
    RecordingBridge, ReplayBridge, Reply, Responder, SnapshotStore, SubagentSystem, SurfaceBinding,
    SurfaceId, Vocab, pending_approvals,
};
use onyx_workspace::{
    ImpactAssessment, PendingCheckpoints, PermissionDecision, WorkspaceLedgerSystem, WorkspaceMode,
    WorkspaceModeHandle, WorkspacePolicy, WorkspaceTools, WorkspaceVocab, graph_workspace_mode,
    graph_workspace_services, scrub_secrets,
};

use crate::command::{PendingSessionAction, PendingSystemNotes, SessionAction};
use crate::external_driver::ExternalDriverApprovalPolicy;
use crate::ops::reconcile_rewind_effects_report;
use crate::pack::{
    MilestoneSnapshotSystem, PackSnapshotStore, SharedPackStore, default_fork_dir,
    fork_pack_until_tick,
};
use crate::subagent::{MergePreviewState, SubagentHandle, effective_merge_decision};
use crate::time_debug::{
    DET_SEALED_OUTPUT_CAP_BYTES, SealedGraphOp, SealedInject, SealedReadResult, SealedShellResult,
    TimeDebugSnapshotStore, TimeDebugState, check_mutation_gate, confirm_branch_from_cursor,
    encode_det_graph_command, encode_det_inject_command, encode_det_read_command,
    encode_det_shell_command, truncate_utf8_safe,
};

/// Interfaz mínima de bombeo UI compartida entre `TuiHandle` (`openonyx chat`) y
/// `WebHandle` (`openonyx serve`) para que ambos corran sobre el mismo bucle determinista
/// `run_session` sin fugas de ticks.
pub trait UiIdlePump {
    fn render(&self, graph: &dyn GraphView);
    fn has_queued_events(&self) -> bool;
    fn pump_idle(&self, graph: &dyn GraphView);
    /// `SDD-07` Fase 2: encola un evento de ingress interno (comando sellado
    /// `/__onyx_det_*`) para que el **siguiente tick** del driver lo procese a
    /// través de la cadena normal (`bridge.push` → grabado `ONYXTR2`),
    /// conservando el EAV bit-a-bit en replay. Default no-op (p. ej. `MockUi`).
    fn queue_ingress(&self, _event: OnyxEvent) {}
}

/// `SDD-07`: vista que ve la UI en cada iteración del bucle — el grafo vivo, o
/// el **preview efímero** del `cursor_tick` cuando el depurador está congelado
/// mirando al pasado (read-only; no toca el `AgentHost`, Git ni `.onyxpack`).
enum SessionView<'a> {
    Live(&'a AgentGraph),
    Preview(Arc<AgentGraph>),
}

impl<'a> SessionView<'a> {
    fn current(host: &'a AgentHost, time_debug: &Option<TimeDebugState>) -> Self {
        if let Some(td) = time_debug
            .as_ref()
            .filter(|td| td.is_frozen() && td.cursor() < td.present())
        {
            if let Ok(preview) = td.preview_graph(td.cursor()) {
                return Self::Preview(preview);
            }
            // Preview no disponible (purgado y sin hito): se degrada al vivo.
        }
        Self::Live(host.graph())
    }

    fn view(&self) -> &dyn GraphView {
        match self {
            Self::Live(graph) => *graph,
            Self::Preview(graph) => graph.as_ref(),
        }
    }
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
    /// `SDD-07` Fase 1: motor del depurador temporal (ring RAM + gate `frozen`).
    pub time_debug: Option<TimeDebugState>,
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
            let time_debug = action_ctx.as_ref().and_then(|c| c.time_debug.clone());
            ui.render(SessionView::current(host, &time_debug).view());
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
                // `SDD-07`: congelado → 0 ticks autónomos; solo se avanza si hay
                // trabajo real pendiente (comando colado, respuesta LLM/tool).
                let frozen = time_debug.as_ref().is_some_and(|t| t.is_frozen());
                let can_advance = if frozen {
                    ui_queued || ready || needs_follow_up_tick
                } else {
                    (!host.is_settled() && !all_blocked)
                        || ui_queued
                        || ready
                        || needs_follow_up_tick
                };
                if can_advance {
                    let was_settled = host.is_settled();
                    let surfaces = active_surfaces(host.graph(), &host.vocab(), &ws_vocab);
                    host.step_with(driver, &surfaces);
                    stepped_in_session = true;
                    // Un `IngressEvent::Command` tarda 1 tick en convertirse en
                    // `spawn_event` (por `CommandSpawnerSystem`) y otro en crear
                    // el agente en el grafo (por `AgentSpawner`).
                    needs_follow_up_tick = ui_queued;
                    // `SDD-07`: captura la frontera de tick en el anillo RAM.
                    if let Some(td) = &time_debug {
                        if host.ticks() > 0 {
                            td.record_tick(host.ticks() - 1, &host.graph().export());
                        }
                    }

                    if let Some(ctx) = &action_ctx {
                        if let Some(action) = ctx.pending_action.take() {
                            let inflight = llm_in_flight.load(Ordering::Acquire);
                            if matches!(action, SessionAction::Rewind { .. }) && inflight > 0 {
                                emit_session_note(
                                    host,
                                    &ctx.pending_notes,
                                    "[/rewind] Cannot rewind while an LLM request is in flight; wait until settled.",
                                );
                            } else {
                                let result = apply_session_action(host, action, ctx, inflight);
                                // `SDD-07` Fase 2: el comando sellado se reinyecta
                                // por la cadena de drivers normal (`queue_ingress` →
                                // próximo `driver.poll()` → `bridge.push` → grabado
                                // en `ONYXTR2`); el spawner lo procesa en el siguiente
                                // tick (EAV bit-a-bit en replay, 0 I/O).
                                needs_follow_up_tick = match result {
                                    Some(sealed) => {
                                        ui.queue_ingress(OnyxEvent::Ingress(
                                            IngressEvent::Command(sealed),
                                        ));
                                        true
                                    }
                                    None => false,
                                };
                            }
                            ui.render(SessionView::current(host, &time_debug).view());
                        }
                    }

                    if !was_settled && host.is_settled() {
                        save_latest_snapshot_with_store(opts, shared_store.as_ref(), host);
                        let _ = save_transcript_if_configured(opts, driver);
                    }
                } else {
                    ui.pump_idle(SessionView::current(host, &time_debug).view());
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
fn running_subagents(ctx: &SessionActionContext) -> usize {
    ctx.subagents.as_ref().map_or(0, |s| s.active_count())
}

/// `SDD-07` Fase 2: si el cursor está por debajo del presente (rama pendiente),
/// confirma la bifurcación **antes** de mutar la realidad (gate incluido en
/// [`confirm_branch_from_cursor`]). Devuelve la nota de bifurcación o `Err`.
fn ensure_branch_from_cursor(
    host: &mut AgentHost,
    ctx: &SessionActionContext,
    llm_in_flight: usize,
) -> Result<Option<String>, String> {
    let Some(td) = &ctx.time_debug else {
        return Ok(None);
    };
    if td.cursor() >= td.present() {
        return Ok(None);
    }
    let summary = confirm_branch_from_cursor(
        host,
        td,
        &ctx.workspace_root,
        &ctx.ws_vocab,
        &ctx.tools,
        ctx.snapshot_store.as_ref(),
        llm_in_flight,
        running_subagents(ctx),
    )
    .map_err(|e| e.0.clone())?;
    Ok(Some(summary))
}

pub fn apply_session_action(
    host: &mut AgentHost,
    action: SessionAction,
    ctx: &SessionActionContext,
    llm_in_flight: usize,
) -> Option<String> {
    match &action {
        SessionAction::MergeSubagent { job_id } => {
            let Some(ref sub) = ctx.subagents else {
                let msg = "[/merge] Async subagents are unavailable in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
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
            return None;
        }
        SessionAction::DiscardSubagent { job_id } => {
            let Some(ref sub) = ctx.subagents else {
                let msg = "[/discard] Async subagents are unavailable in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
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
            return None;
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
                return None;
            }
            let Some(ref sub) = ctx.subagents else {
                let msg = "[/spawn] Async subagents are unavailable in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
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
            return None;
        }
        SessionAction::DetShell { command } => {
            // `SDD-07` Fase 2: gate de mutación (contadores reales) → bifurcación
            // si `cursor < present` → `WorkspaceMode` → `[permissions]` → ejecución
            // sandboxed → comando sellado reinyectado por la cadena de drivers.
            if let Err(check) = check_mutation_gate(
                host.graph(),
                &host.vocab(),
                llm_in_flight,
                running_subagents(ctx),
            ) {
                let msg = format!("[!] Blocked: {}", check.reason().unwrap_or_default());
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            if !ctx.mode_handle.get().allows_mutation() {
                let msg = format!(
                    "[!] Blocked in `{}` mode: switch to BUILD to run `! <cmd>`.",
                    ctx.mode_handle.get().as_label()
                );
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            // `[permissions]` sobre `shell_exec`: `Deny` bloquea incondicionalmente;
            // `Ask` se satisface por la verificación explícita del usuario (el
            // prefijo `!` **es** la aprobación: el comando no lo pide el LLM).
            let args_json = format!(
                r#"{{"command":{}}}"#,
                serde_json::to_string(&command).unwrap_or_else(|_| "\"\"".to_string())
            );
            if ctx
                .tools
                .policy()
                .evaluate_permission("shell_exec", &args_json)
                == PermissionDecision::Deny
            {
                let msg = format!(
                    "[!] Blocked by permission policy (deny rule): `! {command}`. Adjust `[permissions]` or run it inside an agent turn."
                );
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            let branch_note = match ensure_branch_from_cursor(host, ctx, llm_in_flight) {
                Ok(Some(note)) => Some(note),
                Ok(None) => None,
                Err(err) => {
                    emit_session_note(host, &ctx.pending_notes, &err);
                    return None;
                }
            };
            return match ctx.tools.exec_det_shell(command) {
                Ok(exec) => {
                    let mut output = if exec.timed_out {
                        format!("timed_out: true\nstderr:\n{}", exec.stderr)
                    } else {
                        let mut buf = format!(
                            "exit_code: {}\n",
                            exec.exit_code
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "-1".into())
                        );
                        if !exec.stdout.is_empty() {
                            buf.push_str("stdout:\n");
                            buf.push_str(&exec.stdout);
                        }
                        if !exec.stderr.is_empty() {
                            buf.push_str("stderr:\n");
                            buf.push_str(&exec.stderr);
                        }
                        buf
                    };
                    // Cap sellado 8 KiB (valor de la Fase 2; UTF-8 safe).
                    output = truncate_utf8_safe(&output, DET_SEALED_OUTPUT_CAP_BYTES);
                    let sealed = encode_det_shell_command(&SealedShellResult {
                        command: command.clone(),
                        exit_code: exec.exit_code,
                        timed_out: exec.timed_out,
                        output,
                    });
                    let mut msg = format!("[!] Executed (sealed, 0 LLM tokens): `! {command}`.");
                    if let Some(note) = branch_note {
                        msg.push_str(&note);
                    }
                    emit_session_note(host, &ctx.pending_notes, &msg);
                    return Some(sealed);
                }
                Err(err) => {
                    let msg = format!("[!] ERROR: {err}");
                    emit_session_note(host, &ctx.pending_notes, &msg);
                    None
                }
            };
        }
        SessionAction::DetRead {
            path,
            start_line,
            end_line,
        } => {
            // `SDD-07` Fase 2: gate + bifurcación + lectura verificada
            // (`resolve_existing`, rango 1-based inclusivo, `scrub_secrets`,
            // cap 8 KiB) → comando sellado reinyectado.
            if let Err(check) = check_mutation_gate(
                host.graph(),
                &host.vocab(),
                llm_in_flight,
                running_subagents(ctx),
            ) {
                let msg = format!("[/read] Blocked: {}", check.reason().unwrap_or_default());
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            let branch_note = match ensure_branch_from_cursor(host, ctx, llm_in_flight) {
                Ok(Some(note)) => Some(note),
                Ok(None) => None,
                Err(err) => {
                    emit_session_note(host, &ctx.pending_notes, &err);
                    return None;
                }
            };
            let resolved = match ctx.tools.policy().resolve_existing(path) {
                Ok(p) => p,
                Err(err) => {
                    let msg = format!("[/read] {err}");
                    emit_session_note(host, &ctx.pending_notes, &msg);
                    return None;
                }
            };
            let display_path = resolved
                .strip_prefix(&ctx.tools.policy().root)
                .unwrap_or(&resolved)
                .to_string_lossy()
                .replace('\\', "/");
            let content_raw = match fs::read_to_string(&resolved) {
                Ok(c) => c,
                Err(err) => {
                    let msg = format!("[/read] cannot read `{display_path}`: {err}");
                    emit_session_note(host, &ctx.pending_notes, &msg);
                    return None;
                }
            };
            let lines: Vec<&str> = content_raw.lines().collect();
            let total = lines.len();
            let start = start_line.unwrap_or(1).max(1);
            let end = end_line.unwrap_or(total).min(total);
            let content = if start > end || total == 0 {
                String::new()
            } else {
                lines[(start - 1)..end].join("\n")
            };
            let content = truncate_utf8_safe(&scrub_secrets(&content), DET_SEALED_OUTPUT_CAP_BYTES);
            let sealed = encode_det_read_command(&SealedReadResult {
                path: display_path.clone(),
                start_line: *start_line,
                end_line: *end_line,
                content,
            });
            let mut msg = format!("[/read] Loaded `{display_path}` (sealed, 0 LLM tokens).");
            if let Some(note) = branch_note {
                msg.push_str(&note);
            }
            emit_session_note(host, &ctx.pending_notes, &msg);
            return Some(sealed);
        }
        SessionAction::Inject { text } => {
            // Fix de auditoría Fase 2: gate + bifurcación + memoria `user`
            // sellada (reinyectada en-tick por el spawner, 0 LLM, 0 I/O).
            if let Err(check) = check_mutation_gate(
                host.graph(),
                &host.vocab(),
                llm_in_flight,
                running_subagents(ctx),
            ) {
                let msg = format!("[/inject] Blocked: {}", check.reason().unwrap_or_default());
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            let branch_note = match ensure_branch_from_cursor(host, ctx, llm_in_flight) {
                Ok(Some(note)) => Some(note),
                Ok(None) => None,
                Err(err) => {
                    emit_session_note(host, &ctx.pending_notes, &err);
                    return None;
                }
            };
            let sealed = encode_det_inject_command(&SealedInject { text: text.clone() });
            let mut msg = format!("[/inject] Staged user memory (sealed, 0 LLM tokens): `{text}`.");
            if let Some(note) = branch_note {
                msg.push_str(&note);
            }
            emit_session_note(host, &ctx.pending_notes, &msg);
            return Some(sealed);
        }
        SessionAction::GraphDropMem { .. }
        | SessionAction::GraphEditMem { .. }
        | SessionAction::GraphSetGoal { .. } => {
            // Fix de auditoría Fase 2: gate + bifurcación + mutación sellada
            // (`/graph drop-mem|edit-mem|set-goal` reejecutada en-tick por el
            // spawner; bit-a-bit en `openonyx replay`).
            let op = match &action {
                SessionAction::GraphDropMem { start, end } => SealedGraphOp::DropMem {
                    start: *start,
                    end: *end,
                },
                SessionAction::GraphEditMem { seq, text } => SealedGraphOp::EditMem {
                    seq: *seq,
                    text: text.clone(),
                },
                SessionAction::GraphSetGoal { text } => {
                    SealedGraphOp::SetGoal { text: text.clone() }
                }
                _ => unreachable!("brazo /graph de apply_session_action"),
            };
            if let Err(check) = check_mutation_gate(
                host.graph(),
                &host.vocab(),
                llm_in_flight,
                running_subagents(ctx),
            ) {
                let msg = format!("[/graph] Blocked: {}", check.reason().unwrap_or_default());
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            let branch_note = match ensure_branch_from_cursor(host, ctx, llm_in_flight) {
                Ok(Some(note)) => Some(note),
                Ok(None) => None,
                Err(err) => {
                    emit_session_note(host, &ctx.pending_notes, &err);
                    return None;
                }
            };
            let sealed = encode_det_graph_command(&op);
            let mut msg = format!("[/graph {:}] Applied (sealed, 0 LLM tokens).", op.label());
            if let Some(note) = branch_note {
                msg.push_str(&note);
            }
            emit_session_note(host, &ctx.pending_notes, &msg);
            return Some(sealed);
        }
        SessionAction::Step(n) => {
            // `SDD-07` Fase 2 (fix de auditoría): N ticks deterministas con
            // 0 LLM (requiere frozen); gate de mutación + bifurcación si
            // `cursor < present` **antes** de avanzar (misma disciplina que
            // `DetShell`/`DetRead`).
            let Some(td) = &ctx.time_debug else {
                let msg = "[/step] Time debug is not available in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            };
            if !td.is_frozen() {
                let msg =
                    "[/step] Not frozen: use `/time` (or F1) to freeze first, then `/step [n]`."
                        .to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            if let Err(check) = check_mutation_gate(
                host.graph(),
                &host.vocab(),
                llm_in_flight,
                running_subagents(ctx),
            ) {
                let msg = format!("[/step] Blocked: {}", check.reason().unwrap_or_default());
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            let branch_note = match ensure_branch_from_cursor(host, ctx, llm_in_flight) {
                Ok(Some(note)) => Some(note),
                Ok(None) => None,
                Err(err) => {
                    emit_session_note(host, &ctx.pending_notes, &err);
                    return None;
                }
            };
            let n = (*n).clamp(1, 64);
            for _ in 0..n {
                host.step(&mut HeadlessDriver::new());
                td.record_tick(host.ticks() - 1, &host.graph().export());
            }
            let mut msg = format!(
                "[step] Advanced {n} deterministic tick(s) with 0 LLM; clock at tick {} (frozen).",
                host.ticks()
            );
            if let Some(note) = branch_note {
                msg.push_str(&note);
            }
            emit_session_note(host, &ctx.pending_notes, &msg);
            return None;
        }
        SessionAction::StepLlm => {
            // `SDD-07` Fase 2: gate + bifurcación + presupuesto 1 turno LLM +
            // reactivación `Done`/`Failed` → `Idle` (sin memoria `user`): el
            // runtime transita a `Thinking` al siguiente tick y el gate
            // (`try_acquire_llm_turn`, budget 1) despacha exactamente 1 turno.
            let Some(td) = &ctx.time_debug else {
                let msg = "[/step-llm] Time debug is not available in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            };
            if !td.is_frozen() {
                let msg = "[/step-llm] Not frozen: use `/time` (or F1) to freeze first, then `/step-llm`.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            if let Err(check) = check_mutation_gate(
                host.graph(),
                &host.vocab(),
                llm_in_flight,
                running_subagents(ctx),
            ) {
                let msg = format!(
                    "[/step-llm] Blocked: {}",
                    check.reason().unwrap_or_default()
                );
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            if let Err(err) = ensure_branch_from_cursor(host, ctx, llm_in_flight) {
                emit_session_note(host, &ctx.pending_notes, &err);
                return None;
            }
            td.grant_step_llm(1);
            // La reactivación del root (`Done`/`Failed` → `Idle`, sin memoria
            // `user`) viaja sellada por la cadena de drivers: el spawner la
            // aplica en-tick (determinista, 0 I/O, bit-a-bit en replay) y el
            // gate `try_acquire_llm_turn` (presupuesto 1) limita el despacho a
            // exactamente 1 turno LLM.
            let msg = "[step-llm] Granted 1 LLM turn under freeze.".to_string();
            emit_session_note(host, &ctx.pending_notes, &msg);
            return Some(crate::time_debug::DET_STEP_LLM_CMD_PREFIX.to_string());
        }
        SessionAction::Go => {
            // `SDD-07` Fase 2: bifurca si `cursor < present` (gate incluido) y
            // descongela; el LLM reanuda en el siguiente tick.
            let Some(td) = &ctx.time_debug else {
                let msg = "[go] Time debug is not available in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            };
            let branch_note = match ensure_branch_from_cursor(host, ctx, llm_in_flight) {
                Ok(Some(note)) => Some(note),
                Ok(None) => None,
                Err(err) => {
                    emit_session_note(host, &ctx.pending_notes, &err);
                    return None;
                }
            };
            let was_frozen = td.is_frozen();
            td.unfreeze();
            let mut msg = if was_frozen {
                "[go] Unfrozen; resuming normal ticks and LLM.".to_string()
            } else {
                "[go] Already at present (not frozen).".to_string()
            };
            if let Some(note) = branch_note {
                msg.push_str(&note);
            }
            emit_session_note(host, &ctx.pending_notes, &msg);
            return None;
        }
        SessionAction::TimeToggle => {
            // `SDD-07` Fase 2: conmuta congelación (0 I/O, sin gate).
            let Some(td) = &ctx.time_debug else {
                let msg = "[/time] Time debug is not available in this session.".to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            };
            let msg = if td.is_frozen() {
                td.unfreeze();
                "[time] Unfrozen (frozen=false); resuming normal ticks.".to_string()
            } else {
                td.freeze();
                format!(
                    "[time] Frozen at tick {} (frozen=true); 0 autonomous ticks — use `/step [n]`, `/step-llm`, `/go` or `/graph`.",
                    td.present()
                )
            };
            emit_session_note(host, &ctx.pending_notes, &msg);
            return None;
        }
        SessionAction::Rewind { .. } | SessionAction::Fork { .. } => {}
    }

    let Some(store) = &ctx.snapshot_store else {
        let msg =
            "[session] Snapshot persistence is disabled; pass `--snapshot-dir <dir>`.".to_string();
        emit_session_note(host, &ctx.pending_notes, &msg);
        return None;
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
            None
        }
        SessionAction::Rewind { requested_tick } => {
            if !host.is_settled() {
                let msg = "[/rewind] Cannot rewind while an agent is active; wait until settled."
                    .to_string();
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
            }
            let src_dir = store.current_dir();
            let Some(target_tick) = store.resolve_tick_or_floor(requested_tick) else {
                let msg = format!(
                    "[/rewind] No milestone snapshot found at or before tick {requested_tick} in `{}`.",
                    src_dir.display()
                );
                emit_session_note(host, &ctx.pending_notes, &msg);
                return None;
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
                    return None;
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
                        return None;
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

            // `SDD-07` (fix de auditoría): el depurador temporal se sincroniza
            // con la nueva línea temporal: el futuro antiguo (anillo) dejó de
            // existir, la caché de preview es obsoleta y presente = cursor =
            // `target_tick` (último tick completado de la línea activa).
            if let Some(td) = &ctx.time_debug {
                if let Ok(mut ring) = td.ring_arc().lock() {
                    ring.truncate_after(target_tick);
                }
                td.clear_preview_cache();
                td.set_present(target_tick);
            }

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
            None
        }
        SessionAction::MergeSubagent { .. }
        | SessionAction::DiscardSubagent { .. }
        | SessionAction::SpawnSubagent { .. }
        | SessionAction::DetShell { .. }
        | SessionAction::DetRead { .. }
        | SessionAction::Inject { .. }
        | SessionAction::GraphDropMem { .. }
        | SessionAction::GraphEditMem { .. }
        | SessionAction::GraphSetGoal { .. }
        | SessionAction::Step(_)
        | SessionAction::StepLlm
        | SessionAction::Go
        | SessionAction::TimeToggle => {
            unreachable!("these session actions return early above")
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
    build_host_with_shared_store(seed, config, opts, extra_system, checkpoints, None, None)
}

#[allow(clippy::too_many_arguments)]
pub fn build_host_with_shared_store(
    seed: u64,
    config: AgentConfig,
    opts: &SessionOptions,
    extra_system: Option<Box<dyn OnyxSystem>>,
    checkpoints: Option<PendingCheckpoints>,
    shared_store: Option<SharedPackStore>,
    // `SDD-07` Fase 1: si está presente, es el store del `AgentHost`
    // (`load` ring RAM → pack; `save` solo pack).
    time_debug_store: Option<TimeDebugSnapshotStore>,
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
    let store: Option<Box<dyn SnapshotStore>> = match (time_debug_store, shared_store) {
        (Some(td), _) => Some(Box::new(td)),
        (None, Some(shared)) => Some(Box::new(shared)),
        (None, None) => None,
    };
    if let Some(store) = store {
        if opts.save_step_snapshots {
            builder = builder.system(Box::new(MilestoneSnapshotSystem::new(opts.snapshot_every)));
        }
        builder = builder.store(store);
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
