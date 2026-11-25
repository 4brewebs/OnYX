//! Interactive TUI adapter (`ratatui` + `crossterm`) for `openOnyx`.
//! Lives outside `onyx-core` and `onyx-soul-agents` to keep souls dependency-free.

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::f64::consts::{PI, TAU};
use std::io;
#[cfg(not(test))]
use std::io::Stdout;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode as CtKeyCode, KeyEvent, KeyEventKind, KeyModifiers};
// `SDD-07` Fase 3: en tests la TUI corre sobre `TestBackend` (0 raw mode,
// 0 alternate screen) — estos imports solo existen en la build de
// producción (CI `-D warnings` con `--all-targets` incluye el target test).
#[cfg(not(test))]
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use onyx_soul_agents::{
    ConceptId, EventBus, GraphView, IngressEvent, OnyxEvent, PlatformDriver, Reply, SurfaceBinding,
    Vocab, pending_approvals,
};
use onyx_workspace::{
    ImpactAssessment, PermissionDecision, SproutSupervisor, WorkspaceMode, WorkspaceModeHandle,
    WorkspacePolicy, WorkspaceTools, WorkspaceVocab, graph_workspace_plan,
    graph_workspace_services, preview_tool_diff,
};
use ratatui::{
    Frame, Terminal,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Wrap,
        canvas::{Canvas, Line as CanvasLine, Points},
    },
};

use crate::context::{
    COMPACT_MARKER, ReasoningEffort, SessionTelemetryHandle, TieredContextConfig, ToolOutputTier,
    project_memory,
};
use crate::gemini::StreamPreviewHandle;
use crate::llm_profile::LlmProfileHandle;
use crate::pack::SharedPackStore;
use crate::subagent::{
    MergePreviewState, SUBAGENT_PROPOSAL_CMD_PREFIX, SubagentHandle, SubagentJobSummary,
    effective_merge_decision,
};
use crate::time_debug::TimeDebugState;
use crate::tui_input::{
    InputBuffer, InputHistory, PopupState, QueuedPrompts, WorkspaceFileIndex, render_input_popup,
};
use crate::tui_palette::{
    PaletteAction, PaletteItem, PaletteState, build_palette_items, render_palette,
};
use crate::tui_views::{
    ApprovalKeyAction, EffectRow, SproutGate, SproutServiceRow, TuiView, ViewCommandOutcome,
    approval_key_action, parse_sprout_panel_lines, parse_view_command, render_agents_view,
    render_plan_diff_view, render_sprout_view, render_timeline_view, sprout_mutation_gate,
    view_tabs_line,
};
use crate::vault::session_display_label;
use onyx_workspace::{all_recorded_effects, is_effect_undone};

/// `SDD-07` Fase 3: en tests la TUI corre sobre `ratatui::backend::TestBackend`
/// (0 terminal real); en producción, `CrosstermBackend` sobre `stdout`.
#[cfg(not(test))]
type TuiTerminal = Terminal<ratatui::backend::CrosstermBackend<Stdout>>;
#[cfg(test)]
type TuiTerminal = Terminal<ratatui::backend::TestBackend>;

const SPINNER_FRAME_INTERVAL: Duration = Duration::from_millis(90);
const SPINNER_STEP_RAD: f64 = 0.35;
/// Caducidad de los avisos transitorios (`status_hint`) en la cabecera de input.
const STATUS_HINT_TTL: Duration = Duration::from_secs(4);

enum FocusSwitch {
    Next,
    Target(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentRow {
    pub id: ConceptId,
    pub name: String,
    pub is_root: bool,
    pub state: String,
    pub goal: String,
}

struct TuiInner {
    terminal: TuiTerminal,
    vocab: Vocab,
    ws_vocab: Option<WorkspaceVocab>,
    workspace_root: Option<PathBuf>,
    policy: Option<WorkspacePolicy>,
    tools: Option<WorkspaceTools>,
    sprout: Option<SproutSupervisor>,
    subagents: Option<SubagentHandle>,
    mode_handle: WorkspaceModeHandle,
    stream_preview: StreamPreviewHandle,
    telemetry: SessionTelemetryHandle,
    context_config: TieredContextConfig,
    model_name: String,
    reasoning_effort: Option<ReasoningEffort>,
    session_label: Option<String>,
    snapshot_store: Option<SharedPackStore>,
    /// `SDD-07` Fase 3: motor del depurador temporal compartido por el bucle
    /// de sesión (banner ámbar, teclas `Alt+←/→`, `Ctrl+T`, `Esc` seguro y
    /// bloqueo de envío al LLM bajo `TimeFreeze`).
    time_debug: Option<TimeDebugState>,
    seen_compact_count: usize,
    input: InputBuffer,
    history: InputHistory,
    popup: PopupState,
    file_index: WorkspaceFileIndex,
    pending_prompts: QueuedPrompts,
    root_busy: bool,
    queued: Vec<OnyxEvent>,
    active_approval: Option<(ConceptId, u64, String, String)>,
    approval_diff: Option<String>,
    approval_impact: Option<ImpactAssessment>,
    seen_approvals: BTreeSet<(u64, u64)>,
    focused_agent: Option<ConceptId>,
    last_known_root: Option<ConceptId>,
    pending_focus_switch: Option<FocusSwitch>,
    agent_picker_open: bool,
    agent_picker_idx: usize,
    last_agent_ids: Vec<ConceptId>,
    spinner_angle: f64,
    last_spinner_tick: Instant,
    scroll_from_bottom: usize,
    last_viewport_height: usize,
    quit: Arc<AtomicBool>,
    /// `SDD-03` Fase 3 (`REQ-03.6`): vista efímera activa (`F1..F5`).
    view: TuiView,
    /// Paleta `Ctrl+K` (views + LLM + agents) con filtro fuzzy en vivo.
    palette: PaletteState,
    /// Handle LLM vivo: la cabecera muestra `provider/model (effort:...)`
    /// actual y se refresca en `pump_idle` vía `take_dirty()`.
    llm_profile: Option<LlmProfileHandle>,
    /// Selección de fila en `F2 Agents` (agentes + jobs `[bg]`).
    agents_selected: usize,
    /// Selección de servicio en `F3 Sprout`.
    sprout_selected: usize,
    /// `F3 Sprout`: logs anclados al final del servicio seleccionado.
    sprout_follow: bool,
    /// Selección de hito en `F5 Timeline`.
    timeline_selected: usize,
    /// Scroll de `F4 Plan & Diff` (líneas).
    plan_scroll: usize,
    /// Últimos datos de agentes/jobs calculados en `render` (para `Ctrl+K`
    /// y `F2 Agents` fuera del render).
    last_agent_rows: Vec<AgentRow>,
    last_bg_jobs: Vec<SubagentJobSummary>,
    /// Aviso transitorio (gate F3, `/view` inválido) con su instante de emisión.
    status_hint: Option<String>,
    status_hint_at: Option<Instant>,
}

#[derive(Clone)]
pub struct TuiHandle {
    inner: Rc<RefCell<TuiInner>>,
}

impl TuiHandle {
    pub fn has_queued_events(&self) -> bool {
        !self.inner.borrow().queued.is_empty()
    }

    pub fn render(&self, graph: &dyn GraphView) {
        self.inner.borrow_mut().render(graph);
    }

    /// Reads keyboard/resize input while the brain is settled (`is_settled()`),
    /// dispatches any queued follow-up prompt once the root agent settles,
    /// or refreshes in-flight SSE deltas, telemetry, and throttled emblem rotation without advancing `host.ticks()`.
    pub fn pump_idle(&self, graph: &dyn GraphView) {
        let mut inner = self.inner.borrow_mut();
        let ui_event = inner.poll_crossterm();
        let stream_dirty = inner.stream_preview.take_dirty();
        let telemetry_dirty = inner.telemetry.take_dirty();
        // `SDD-03` Fase 3: un cambio LLM en caliente marca dirty → refresca la
        // cabecera viva sin tocar el bucle determinista.
        let llm_dirty = inner
            .llm_profile
            .as_ref()
            .is_some_and(LlmProfileHandle::take_dirty);
        let mut hint_expired = false;
        if let Some(at) = inner.status_hint_at {
            if at.elapsed() >= STATUS_HINT_TTL {
                inner.status_hint = None;
                inner.status_hint_at = None;
                hint_expired = true;
            }
        }
        let busy = inner.is_any_agent_busy(graph);
        let root_busy = inner.is_root_agent_busy(graph);
        inner.root_busy = root_busy;
        if !root_busy
            && inner.active_approval.is_none()
            && inner.queued.is_empty()
            && !inner.pending_prompts.is_empty()
        {
            if let Some(next_prompt) = inner.pending_prompts.pop_front() {
                inner.scroll_from_bottom = 0;
                inner
                    .queued
                    .push(OnyxEvent::Ingress(IngressEvent::Command(next_prompt)));
            }
        }
        let spinner_dirty = inner.advance_spinner_if_due(busy);
        if (ui_event
            || stream_dirty
            || telemetry_dirty
            || llm_dirty
            || hint_expired
            || spinner_dirty)
            && inner.queued.is_empty()
            && !inner.quit.load(Ordering::Relaxed)
        {
            inner.render(graph);
        }
    }
}

impl crate::session::UiIdlePump for TuiHandle {
    fn render(&self, graph: &dyn GraphView) {
        TuiHandle::render(self, graph);
    }

    fn has_queued_events(&self) -> bool {
        TuiHandle::has_queued_events(self)
    }

    fn pump_idle(&self, graph: &dyn GraphView) {
        TuiHandle::pump_idle(self, graph);
    }

    /// `SDD-07` Fase 2: encola el comando sellado; el próximo
    /// `TuiDriver::poll` lo drena vía `bridge.push` (queda en `ONYXTR2`).
    fn queue_ingress(&self, event: OnyxEvent) {
        self.inner.borrow_mut().queued.push(event);
    }
}

pub struct TuiDriver {
    inner: Rc<RefCell<TuiInner>>,
}

impl TuiDriver {
    pub fn new(vocab: Vocab) -> io::Result<Self> {
        #[cfg(not(test))]
        let terminal = {
            enable_raw_mode()?;
            let mut stdout = io::stdout();
            execute!(stdout, EnterAlternateScreen)?;
            let backend = ratatui::backend::CrosstermBackend::new(stdout);
            Terminal::new(backend)?
        };
        #[cfg(test)]
        let terminal = {
            // 160x40: ancho suficiente para el banner ámbar completo de
            // `SDD-07` (`⏸ TIME FROZEN … · revert on branch: N file(s)`).
            let backend = ratatui::backend::TestBackend::new(160, 40);
            Terminal::new(backend)?
        };
        Ok(Self {
            inner: Rc::new(RefCell::new(TuiInner {
                terminal,
                vocab,
                ws_vocab: None,
                workspace_root: None,
                policy: None,
                tools: None,
                sprout: None,
                subagents: None,
                mode_handle: WorkspaceModeHandle::default(),
                stream_preview: StreamPreviewHandle::default(),
                telemetry: SessionTelemetryHandle::default(),
                context_config: TieredContextConfig::default(),
                model_name: String::new(),
                reasoning_effort: None,
                session_label: None,
                snapshot_store: None,
                time_debug: None,
                seen_compact_count: 0,
                input: InputBuffer::default(),
                history: InputHistory::default(),
                popup: PopupState::default(),
                file_index: WorkspaceFileIndex::default(),
                pending_prompts: QueuedPrompts::default(),
                root_busy: false,
                queued: Vec::new(),
                active_approval: None,
                approval_diff: None,
                approval_impact: None,
                seen_approvals: BTreeSet::new(),
                focused_agent: None,
                last_known_root: None,
                pending_focus_switch: None,
                agent_picker_open: false,
                agent_picker_idx: 0,
                last_agent_ids: Vec::new(),
                spinner_angle: 0.0,
                last_spinner_tick: Instant::now(),
                scroll_from_bottom: 0,
                last_viewport_height: 20,
                quit: Arc::new(AtomicBool::new(false)),
                view: TuiView::Chat,
                palette: PaletteState::closed(),
                llm_profile: None,
                agents_selected: 0,
                sprout_selected: 0,
                sprout_follow: true,
                timeline_selected: 0,
                plan_scroll: 0,
                last_agent_rows: Vec::new(),
                last_bg_jobs: Vec::new(),
                status_hint: None,
                status_hint_at: None,
            })),
        })
    }

    pub fn with_workspace(self, root: PathBuf, mode_handle: WorkspaceModeHandle) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.workspace_root = Some(root);
            inner.mode_handle = mode_handle;
        }
        self
    }

    pub fn with_policy(self, policy: WorkspacePolicy) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.policy = Some(policy);
        }
        self
    }

    pub fn with_tools(self, tools: WorkspaceTools) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.tools = Some(tools);
        }
        self
    }

    pub fn with_sprout(self, sprout: SproutSupervisor) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.sprout = Some(sprout);
        }
        self
    }

    pub fn with_subagents(self, subagents: SubagentHandle) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.subagents = Some(subagents);
        }
        self
    }

    pub fn with_workspace_vocab(self, ws_vocab: WorkspaceVocab) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.ws_vocab = Some(ws_vocab);
        }
        self
    }

    /// `SDD-07` Fase 3: conecta el motor del depurador temporal al TUI
    /// (banner ámbar, scrubbing `Alt+←/→`, `Ctrl+T`, `Esc` seguro y bloqueo
    /// de envío al LLM bajo `TimeFreeze`). Sin este builder la TUI funciona
    /// en modo vivo clásico (0 cambios).
    pub fn with_time_debug(self, state: TimeDebugState) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.time_debug = Some(state);
        }
        self
    }

    pub fn with_stream_preview(self, stream_preview: StreamPreviewHandle) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.stream_preview = stream_preview;
        }
        self
    }

    pub fn with_telemetry(self, telemetry: SessionTelemetryHandle) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.telemetry = telemetry;
        }
        self
    }

    pub fn with_context_config(self, cfg: TieredContextConfig) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.context_config = cfg;
        }
        self
    }

    pub fn with_model_info(
        self,
        model: impl Into<String>,
        effort: Option<ReasoningEffort>,
    ) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.model_name = model.into();
            inner.reasoning_effort = effort;
        }
        self
    }

    /// `SDD-03` Fase 3 (`REQ-03.6`): handle LLM vivo — la cabecera de `F1 Chat`
    /// muestra el objetivo actual (`provider/model (effort:...)`) y se
    /// refresca en `pump_idle` vía `take_dirty()`; la paleta `Ctrl+K` expone el
    /// catálogo de perfiles/providers/esfuerzos.
    pub fn with_llm_profile(self, profile: LlmProfileHandle) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.llm_profile = Some(profile);
        }
        self
    }

    pub fn with_session_storage(
        self,
        session_label: Option<String>,
        snapshot_store: Option<SharedPackStore>,
    ) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.session_label = session_label;
            inner.snapshot_store = snapshot_store;
        }
        self
    }

    pub fn handle(&self) -> TuiHandle {
        TuiHandle {
            inner: Rc::clone(&self.inner),
        }
    }

    pub fn quit_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.inner.borrow().quit)
    }
}

impl TuiInner {
    fn active_session_label(&self) -> Option<String> {
        self.snapshot_store
            .as_ref()
            .and_then(|st| {
                let dir = st.current_dir();
                self.workspace_root
                    .as_deref()
                    .and_then(|ws| session_display_label(ws, &dir))
                    .or_else(|| dir.file_name().and_then(|n| n.to_str()).map(str::to_string))
            })
            .or_else(|| self.session_label.clone())
    }

    fn shutdown_inner(&mut self) -> io::Result<()> {
        #[cfg(not(test))]
        {
            disable_raw_mode()?;
            execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        }
        self.terminal.show_cursor()?;
        Ok(())
    }

    fn is_any_agent_busy(&self, graph: &dyn GraphView) -> bool {
        if self.active_approval.is_some() {
            return false;
        }
        graph.query(&[self.vocab.state]).into_iter().any(|id| {
            matches!(
                prop_text(graph, id, self.vocab.state).as_deref(),
                Some("thinking" | "acting" | "waiting_children")
            )
        })
    }

    fn is_root_agent_busy(&self, graph: &dyn GraphView) -> bool {
        if self.active_approval.is_some() {
            return true;
        }
        let latest_root = graph
            .query(&[self.vocab.state])
            .into_iter()
            .filter(|id| graph.related(*id, self.vocab.rel_parent).is_empty())
            .last();
        latest_root.is_some_and(|id| {
            matches!(
                prop_text(graph, id, self.vocab.state).as_deref(),
                Some("thinking" | "acting" | "waiting_children" | "waiting_approval")
            )
        })
    }

    fn sync_popup(&mut self) {
        self.popup.sync_from_buffer(
            &self.input,
            &mut self.file_index,
            self.workspace_root.as_deref(),
            self.policy.as_ref(),
        );
    }

    fn advance_spinner_if_due(&mut self, busy: bool) -> bool {
        if !busy {
            if self.spinner_angle != 0.0 {
                self.spinner_angle = 0.0;
                return true;
            }
            return false;
        }
        if self.last_spinner_tick.elapsed() >= SPINNER_FRAME_INTERVAL {
            self.last_spinner_tick = Instant::now();
            self.spinner_angle = (self.spinner_angle + SPINNER_STEP_RAD) % TAU;
            true
        } else {
            false
        }
    }

    fn open_agent_picker(&mut self) {
        self.agent_picker_open = true;
        if let Some(cur) = self.focused_agent {
            if let Some(idx) = self.last_agent_ids.iter().position(|id| *id == cur) {
                self.agent_picker_idx = idx;
                return;
            }
        }
        self.agent_picker_idx = 0;
    }

    /// `SDD-03` Fase 3: aviso transitorio (gate `F3`, `/view` inválido, etc.).
    fn set_status_hint(&mut self, hint: String) {
        self.status_hint = Some(hint);
        self.status_hint_at = Some(Instant::now());
    }

    /// `SDD-07` Fase 3: ¿hay un estado "tiempo" no-presente que deba mostrarse
    /// en el banner ámbar y proteger con `Esc` seguro? Congelado, o con el
    /// cursor anclado en el pasado (mirando una rama histórica).
    fn time_frozen_active(&self) -> bool {
        self.time_debug
            .as_ref()
            .is_some_and(|td| td.is_frozen() || td.cursor() < td.present())
    }

    /// `SDD-07` Fase 3: teclas universales del depurador temporal —
    /// `Alt+←`/`Alt+→` (scrub −1/+1 sobre `available_ticks()`, aviso breve en
    /// `status_hint` si el extremo/error) y `Ctrl+T` (toggle efímero
    /// freeze/return_to_present). **Nunca** tocan `InputBuffer`: sin colisión
    /// con la escritura. Devuelve `true` solo si la tecla la consumió.
    fn handle_time_debug_key(&mut self, code: CtKeyCode, modifiers: KeyModifiers) -> bool {
        let alt_only =
            modifiers.contains(KeyModifiers::ALT) && !modifiers.contains(KeyModifiers::CONTROL);
        if alt_only {
            // El borrow de `time_debug` se resuelve en el `and_then` antes de
            // tocar `self` mutable (`set_status_hint`).
            let err = match code {
                CtKeyCode::Left => self
                    .time_debug
                    .as_ref()
                    .and_then(|td| td.scrub_delta(-1).err()),
                CtKeyCode::Right => self
                    .time_debug
                    .as_ref()
                    .and_then(|td| td.scrub_delta(1).err()),
                _ => return false,
            };
            if err.is_none() && self.time_debug.is_none() {
                return false;
            }
            if let Some(e) = err {
                self.set_status_hint(e.to_string());
            }
            return true;
        }
        if modifiers.contains(KeyModifiers::CONTROL)
            && !modifiers.contains(KeyModifiers::ALT)
            && matches!(code, CtKeyCode::Char('t' | 'T'))
        {
            let frozen = self.time_debug.as_ref().is_some_and(|td| {
                if td.is_frozen() {
                    td.return_to_present();
                } else {
                    td.freeze();
                }
                td.is_frozen()
            });
            if self.time_debug.is_none() {
                return false;
            }
            self.set_status_hint(if frozen {
                "TIME FROZEN — Alt+←/→ scrub, /go resume, Esc present.".to_string()
            } else {
                "Back at present (live) — Esc again to quit.".to_string()
            });
            return true;
        }
        false
    }

    /// `SDD-03` Fase 3: cambia la vista efímera (`F1..F5` / `/view` / paleta).
    /// 100 % efímera: no toca el grafo ni el bucle determinista.
    fn switch_view(&mut self, view: TuiView) {
        self.view = view;
        self.popup.close();
        self.status_hint = None;
        self.status_hint_at = None;
    }

    /// Abre la paleta `Ctrl+K` con un catálogo fresco (views + LLM + agentes).
    fn open_palette(&mut self) {
        self.popup.close();
        let profiles = self
            .llm_profile
            .as_ref()
            .map(LlmProfileHandle::public_snapshot);
        let items = build_palette_items(
            profiles.as_ref(),
            &self.last_agent_rows,
            self.time_debug.as_ref(),
        );
        self.palette.open_with(items);
    }

    /// Paleta abierta: traga todas las teclas (Esc/Ctrl+K cierra, `F1..F5`
    /// cierra y cambia de vista, Up/Down o Ctrl+P/Ctrl+N navegan, Enter
    /// ejecuta). **Todos** los caracteres imprimibles filtran (`insert_char`)
    /// — `j`/`k` incluidos, nunca navegan.
    fn handle_palette_key(&mut self, code: CtKeyCode, modifiers: KeyModifiers) {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        match code {
            CtKeyCode::Esc => {
                self.palette.close();
            }
            CtKeyCode::Char('k' | 'K') if ctrl => {
                self.palette.close();
            }
            CtKeyCode::F(n) => {
                if let Some(view) = TuiView::from_key(n) {
                    self.palette.close();
                    self.switch_view(view);
                }
            }
            CtKeyCode::Up => {
                self.palette.move_up();
            }
            CtKeyCode::Down => {
                self.palette.move_down();
            }
            CtKeyCode::Char('p' | 'P') if ctrl => {
                self.palette.move_up();
            }
            CtKeyCode::Char('n' | 'N') if ctrl => {
                self.palette.move_down();
            }
            CtKeyCode::Enter => {
                if let Some(item) = self.palette.selected_item() {
                    self.apply_palette_action(item.action().clone());
                }
                self.palette.close();
            }
            CtKeyCode::Char(c) if !ctrl && !modifiers.contains(KeyModifiers::ALT) => {
                self.palette.insert_char(c);
            }
            CtKeyCode::Backspace => {
                self.palette.backspace();
            }
            _ => {}
        }
    }

    /// Ejecuta la selección de la paleta `Ctrl+K`: views/agentes son
    /// navegación local; los comandos LLM pasan por el bus determinista.
    fn apply_palette_action(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::View(view) => {
                self.switch_view(view);
            }
            PaletteAction::Command(cmd) => {
                // Slash LLM (Fase 2): inmediato en la frontera de tick, sellado en `ONYXTR2`.
                self.queued
                    .push(OnyxEvent::Ingress(IngressEvent::Command(cmd)));
            }
            PaletteAction::Agent(id) => {
                self.focused_agent = Some(id);
                self.switch_view(TuiView::Chat);
                self.scroll_from_bottom = 0;
            }
        }
    }

    /// `F3 Sprout`: filas de servicio parseadas del panel de texto.
    fn sprout_service_rows(&self) -> Vec<SproutServiceRow> {
        let Some(text) = self.sprout.as_ref().and_then(|s| s.render_panel_text()) else {
            return Vec::new();
        };
        parse_sprout_panel_lines(&text)
    }

    fn sprout_untrusted(&self) -> bool {
        self.policy
            .as_ref()
            .is_some_and(|p| !p.trust.is_fully_trusted())
    }

    /// `F3 Sprout`: aplica una tecla mutante (`s`/`x`/`r`/`a`) encolando el
    /// comando `/service ...` correspondiente por el bus (el turno de root lo
    /// re-valida contra el modo). Bloqueado por el gate ⇒ aviso, sin mutar.
    fn sprout_mutate(&mut self, key: char) {
        // `SDD-03` Fase 4: el gate antes que la selección — en `[UNTRUSTED]`
        // el panel no lista filas y el aviso real es el de trust, no
        // "no service selected".
        let gate = sprout_mutation_gate(self.mode_handle.get(), self.sprout_untrusted());
        if !matches!(gate, SproutGate::Allowed) {
            self.set_status_hint(gate.notice());
            return;
        }
        let rows = self.sprout_service_rows();
        let Some(row) = rows.get(self.sprout_selected) else {
            self.set_status_hint("F3 Sprout: no service selected.".to_string());
            return;
        };
        let cmd = match key {
            's' | 'S' => format!("/service start {}", row.id),
            'x' | 'X' => format!("/service stop {}", row.id),
            'r' | 'R' => {
                // Reinicio = stop + start, en orden, por el mismo bus.
                self.queued
                    .push(OnyxEvent::Ingress(IngressEvent::Command(format!(
                        "/service stop {}",
                        row.id
                    ))));
                format!("/service start {}", row.id)
            }
            'a' | 'A' => "/service start all".to_string(),
            _ => return,
        };
        self.queued
            .push(OnyxEvent::Ingress(IngressEvent::Command(cmd)));
    }

    /// `F2 Agents`: Up/Down/k/j sobre agentes + jobs `[bg]`; Enter enfoca y
    /// vuelve a `F1 Chat`.
    fn handle_agents_view_key(&mut self, code: CtKeyCode) -> bool {
        let total = self.last_agent_rows.len() + self.last_bg_jobs.len();
        if total == 0 {
            return false;
        }
        match code {
            CtKeyCode::Up | CtKeyCode::Char('k') => {
                self.agents_selected = if self.agents_selected == 0 {
                    total - 1
                } else {
                    self.agents_selected - 1
                };
                true
            }
            CtKeyCode::Down | CtKeyCode::Char('j') => {
                self.agents_selected = (self.agents_selected + 1) % total;
                true
            }
            CtKeyCode::Enter => {
                if let Some(row) = self.last_agent_rows.get(self.agents_selected) {
                    self.focused_agent = Some(row.id);
                }
                self.view = TuiView::Chat;
                self.scroll_from_bottom = 0;
                true
            }
            _ => false,
        }
    }

    /// `F3 Sprout`: Up/Down/PgUp/PgDn seleccionan; `s`/`x`/`r`/`a` mutan (con
    /// gate); `f` alterna `follow` de logs.
    fn handle_sprout_view_key(&mut self, code: CtKeyCode) -> bool {
        let count = self.sprout_service_rows().len();
        match code {
            CtKeyCode::Up => {
                if count > 0 {
                    self.sprout_selected = if self.sprout_selected == 0 {
                        count - 1
                    } else {
                        self.sprout_selected - 1
                    };
                }
                true
            }
            CtKeyCode::Down => {
                if count > 0 {
                    self.sprout_selected = (self.sprout_selected + 1) % count;
                }
                true
            }
            CtKeyCode::PageUp => {
                self.sprout_selected = self.sprout_selected.saturating_sub(10);
                if count > 0 {
                    self.sprout_selected = self.sprout_selected.min(count - 1);
                }
                true
            }
            CtKeyCode::PageDown => {
                if count > 0 {
                    self.sprout_selected = (self.sprout_selected + 10) % count;
                }
                true
            }
            CtKeyCode::Char(c @ ('s' | 'S' | 'x' | 'X' | 'r' | 'R' | 'a' | 'A')) => {
                self.sprout_mutate(c);
                true
            }
            CtKeyCode::Char('f' | 'F') => {
                self.sprout_follow = !self.sprout_follow;
                true
            }
            _ => false,
        }
    }

    /// `F4 Plan & Diff`: Up/Down ±1, PgUp/PgDn ±10 (scroll del ledger).
    fn handle_plan_diff_key(&mut self, code: CtKeyCode) -> bool {
        match code {
            CtKeyCode::Up => {
                self.plan_scroll = self.plan_scroll.saturating_sub(1);
                true
            }
            CtKeyCode::Down => {
                self.plan_scroll += 1;
                true
            }
            CtKeyCode::PageUp => {
                self.plan_scroll = self.plan_scroll.saturating_sub(10);
                true
            }
            CtKeyCode::PageDown => {
                self.plan_scroll += 10;
                true
            }
            _ => false,
        }
    }

    /// `F5 Timeline`: Up/Down selecciona hito; Enter encola `/rewind <tick>`;
    /// `f` encola `/fork` (todo por el bus determinista).
    /// `SDD-07` Fase 3: filas de `F5 Timeline` sobre los ticks scrubeables
    /// **reales** (`TimeDebugState::available_ticks()` = anillo RAM ∪ hitos
    /// en disco ∪ presente), con su etiqueta: `[present]`, `[pack:<tag>]` o
    /// `[ram]`. Sin depurador temporal, fallback a los hitos de disco
    /// (comportamiento `SDD-03`).
    fn timeline_rows(&self) -> Vec<(u64, String)> {
        let Some(td) = self.time_debug.as_ref() else {
            return self
                .snapshot_store
                .as_ref()
                .map(|s| {
                    s.entries()
                        .iter()
                        .map(|m| (m.tick, format!("pack:{}", m.tag)))
                        .collect()
                })
                .unwrap_or_default();
        };
        let mut pack_tags = std::collections::BTreeMap::new();
        if let Some(s) = &self.snapshot_store {
            for m in s.entries() {
                pack_tags.insert(m.tick, m.tag.clone());
            }
        }
        let present = td.present();
        td.available_ticks()
            .into_iter()
            .map(|t| {
                let tag = if t == present {
                    "present".to_string()
                } else if let Some(tag) = pack_tags.remove(&t) {
                    format!("pack:{tag}")
                } else {
                    "ram".to_string()
                };
                (t, tag)
            })
            .collect()
    }

    fn handle_timeline_view_key(&mut self, code: CtKeyCode) -> bool {
        let count = self.timeline_rows().len();
        match code {
            CtKeyCode::Up => {
                if count > 0 {
                    self.timeline_selected = if self.timeline_selected == 0 {
                        count - 1
                    } else {
                        self.timeline_selected - 1
                    };
                }
                true
            }
            CtKeyCode::Down => {
                if count > 0 {
                    self.timeline_selected = (self.timeline_selected + 1) % count;
                }
                true
            }
            CtKeyCode::Enter => {
                // `SDD-07` Fase 3: Enter → `scrub_to` del depurador: a
                // `< present` auto-congela y vuelve a `F1 Chat` con **preview
                // congelado** (0 disco hasta que una acción mutante confirme
                // la bifurcación). Sin depurador, fallback SDD-03: `/rewind`.
                if let Some((tick, _)) = self.timeline_rows().get(self.timeline_selected) {
                    // El borrow de `time_debug` muere al copiar `(cursor,
                    // present)` antes de mutar `self` (switch_view/hint).
                    let outcome: Option<Result<(u64, u64), String>> =
                        self.time_debug.as_ref().map(|td| {
                            td.scrub_to(*tick)
                                .map(|()| (td.cursor(), td.present()))
                                .map_err(|e| e.to_string())
                        });
                    if let Some(outcome) = outcome {
                        match outcome {
                            Ok((cursor, present)) => {
                                self.switch_view(TuiView::Chat);
                                if cursor < present {
                                    self.set_status_hint(format!(
                                        "Scrubbed to tick {tick} (frozen preview) — /go resume, Esc present, mutation branches here."
                                    ));
                                }
                            }
                            Err(msg) => self.set_status_hint(msg),
                        }
                    } else {
                        self.queued
                            .push(OnyxEvent::Ingress(IngressEvent::Command(format!(
                                "/rewind {tick}"
                            ))));
                    }
                } else {
                    self.set_status_hint("F5 Timeline: no tick selected.".to_string());
                }
                true
            }
            CtKeyCode::Char('r' | 'R') => {
                // `/rewind <tick>`: solo tiene sentido sobre hitos en disco
                // (`[pack:<tag>]` / `[present]`); un tick `[ram]` no tiene
                // snapshot para restaurar.
                if let Some((tick, tag)) = self.timeline_rows().get(self.timeline_selected) {
                    if tag.starts_with("pack:") || tag == "present" {
                        self.queued
                            .push(OnyxEvent::Ingress(IngressEvent::Command(format!(
                                "/rewind {tick}"
                            ))));
                    } else {
                        self.set_status_hint(
                            "F5 Timeline: 'r' /rewind solo sobre hitos en disco ([pack:*])."
                                .to_string(),
                        );
                    }
                } else {
                    self.set_status_hint("F5 Timeline: no tick selected.".to_string());
                }
                true
            }
            CtKeyCode::Char('f' | 'F') => {
                self.queued.push(OnyxEvent::Ingress(IngressEvent::Command(
                    "/fork".to_string(),
                )));
                true
            }
            _ => false,
        }
    }

    /// `F1 Chat`: input actual (buffer, popup, history, Tab de modo) con
    /// interceptación TUI de `/view` (nunca llega al bus).
    fn handle_chat_view_key(&mut self, code: CtKeyCode, modifiers: KeyModifiers) {
        match code {
            CtKeyCode::Tab if modifiers.contains(KeyModifiers::CONTROL) => {
                self.pending_focus_switch = Some(FocusSwitch::Next);
                self.scroll_from_bottom = 0;
            }
            CtKeyCode::Tab => {
                let next_cmd = match self.mode_handle.get() {
                    WorkspaceMode::Build => "/mode plan",
                    WorkspaceMode::Plan | WorkspaceMode::Explore => "/mode build",
                };
                self.queued.push(OnyxEvent::Ingress(IngressEvent::Command(
                    next_cmd.to_string(),
                )));
            }
            CtKeyCode::Left => {
                self.input.move_left();
                self.sync_popup();
            }
            CtKeyCode::Right => {
                self.input.move_right();
                self.sync_popup();
            }
            CtKeyCode::Char(c)
                if !modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.insert_char(c);
                self.history.reset_navigation();
                self.popup.reset_dismiss();
                self.sync_popup();
            }
            CtKeyCode::Backspace => {
                self.input.backspace();
                self.history.reset_navigation();
                self.popup.reset_dismiss();
                self.sync_popup();
            }
            CtKeyCode::Delete => {
                self.input.delete();
                self.history.reset_navigation();
                self.popup.reset_dismiss();
                self.sync_popup();
            }
            CtKeyCode::Enter => {
                let cmd = self.input.as_str().trim().to_string();
                self.input.clear();
                self.popup.close();
                // `SDD-07` Fase 3 (crítico UX): bajo TimeFreeze (o con el
                // cursor anclado en el pasado) un prompt normal **no se
                // envía** al LLM — un turno desordenado rompería la
                // bit-a-bit del replay. `/…` y `!…` sí se encolan: son las
                // vías de escape del depurador (`/go`, `/inject`, `/graph`).
                if self.time_frozen_active()
                    && !cmd.is_empty()
                    && !cmd.starts_with('/')
                    && !cmd.starts_with('!')
                {
                    self.input.set_text(cmd);
                    self.sync_popup();
                    self.set_status_hint(
                        "Paused in TIME FROZEN: use /go to resume or /inject to stage memory (Esc to return to present)"
                            .to_string(),
                    );
                    return;
                }
                // `SDD-03` Fase 3: `/view` es un comando de nivel TUI — cambia
                // de vista sin tocar el bus ni la historia.
                if let Some(outcome) = parse_view_command(&cmd) {
                    let view_names = TuiView::ALL
                        .iter()
                        .map(|v| v.as_slash())
                        .collect::<Vec<_>>()
                        .join("|");
                    match outcome {
                        ViewCommandOutcome::Usage => {
                            self.set_status_hint(format!("Usage: /view <{view_names}>"));
                        }
                        ViewCommandOutcome::Switch(view) => {
                            self.switch_view(view);
                        }
                        ViewCommandOutcome::Unknown(arg) => {
                            self.set_status_hint(format!(
                                "Unknown view `{arg}` — use {view_names}"
                            ));
                        }
                    }
                    return;
                }
                if !cmd.is_empty()
                    && !cmd.starts_with(SUBAGENT_PROPOSAL_CMD_PREFIX.trim_end())
                    && !cmd.starts_with("/__onyx_")
                {
                    self.history.push(cmd.clone());
                    self.scroll_from_bottom = 0;
                    if cmd == "/agent" || cmd == "/agents" {
                        if self.last_agent_ids.len() > 1 {
                            self.open_agent_picker();
                        } else {
                            self.pending_focus_switch = Some(FocusSwitch::Next);
                        }
                    } else if let Some(rest) = cmd
                        .strip_prefix("/agent ")
                        .or_else(|| cmd.strip_prefix("/agents "))
                    {
                        let target = rest.trim();
                        if target.is_empty() {
                            self.open_agent_picker();
                        } else {
                            self.pending_focus_switch =
                                Some(FocusSwitch::Target(target.to_string()));
                        }
                    } else if self.root_busy && !is_immediate_slash_cmd(&cmd) {
                        self.pending_prompts.push(cmd);
                    } else {
                        let lower = cmd.trim().to_ascii_lowercase();
                        if lower == "/cancel"
                            || lower.starts_with("/cancel ")
                            || lower == "/stop"
                            || lower.starts_with("/stop ")
                        {
                            self.pending_prompts.clear();
                            self.active_approval = None;
                        }
                        self.queued
                            .push(OnyxEvent::Ingress(IngressEvent::Command(cmd)));
                    }
                }
            }
            _ => {}
        }
    }

    /// Returns `true` if a UI or brain event occurred.
    fn poll_crossterm(&mut self) -> bool {
        if !event::poll(Duration::from_millis(16)).unwrap_or(false) {
            return false;
        }
        match event::read() {
            Ok(Event::Resize(w, h)) => {
                self.queued.push(OnyxEvent::Ingress(IngressEvent::Resize {
                    w: u32::from(w),
                    h: u32::from(h),
                }));
                true
            }
            Ok(Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            })) => {
                if !matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    return false;
                }
                self.dispatch_key(code, modifiers)
            }
            _ => false,
        }
    }

    /// `SDD-03` Fase 3 + `SDD-07` Fase 3: dispatch de teclado (extraído de
    /// `poll_crossterm` para testearlo sin terminal real).
    fn dispatch_key(&mut self, code: CtKeyCode, modifiers: KeyModifiers) -> bool {
        let is_ctrl_c = modifiers.contains(KeyModifiers::CONTROL)
            && matches!(code, CtKeyCode::Char('c') | CtKeyCode::Char('C'));
        if is_ctrl_c {
            self.quit.store(true, Ordering::Relaxed);
            return true;
        }
        // `SDD-03` Fase 3: paleta `Ctrl+K` — prioridad máxima (salvo
        // Ctrl+C) y traga todas las teclas mientras está abierta.
        if self.palette.is_open() {
            self.handle_palette_key(code, modifiers);
            return true;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, CtKeyCode::Char('k' | 'K')) {
            self.open_palette();
            return true;
        }
        if self.agent_picker_open {
            match code {
                CtKeyCode::Esc | CtKeyCode::F(2) => {
                    self.agent_picker_open = false;
                }
                CtKeyCode::Up | CtKeyCode::Char('k') => {
                    let len = self.last_agent_ids.len();
                    if len > 0 {
                        self.agent_picker_idx = if self.agent_picker_idx == 0 {
                            len - 1
                        } else {
                            self.agent_picker_idx - 1
                        };
                    }
                }
                CtKeyCode::Down | CtKeyCode::Char('j') => {
                    let len = self.last_agent_ids.len();
                    if len > 0 {
                        self.agent_picker_idx = (self.agent_picker_idx + 1) % len;
                    }
                }
                CtKeyCode::Enter => {
                    if let Some(&chosen) = self.last_agent_ids.get(self.agent_picker_idx) {
                        self.focused_agent = Some(chosen);
                        self.scroll_from_bottom = 0;
                    }
                    self.agent_picker_open = false;
                }
                _ => {}
            }
            return true;
        }
        // `SDD-07` Fase 3: teclas del depurador temporal universales
        // (`Alt+←/→`, `Ctrl+T`): funcionan en cualquier vista y **nunca**
        // tocan `InputBuffer` (sin colisiones con la escritura).
        if self.handle_time_debug_key(code, modifiers) {
            return true;
        }
        // `SDD-03` Fase 3: `F1..F5` — cambio de vista efímera
        // (cierra popup y aviso; `F6+` cae al resto de la cadena).
        if let CtKeyCode::F(n) = code {
            if let Some(view) = TuiView::from_key(n) {
                self.switch_view(view);
                return true;
            }
        }
        if self.popup.is_open() && self.active_approval.is_none() {
            match code {
                CtKeyCode::Esc => {
                    self.popup.close();
                    return true;
                }
                CtKeyCode::Up => {
                    self.popup.move_up();
                    return true;
                }
                CtKeyCode::Down => {
                    self.popup.move_down();
                    return true;
                }
                CtKeyCode::Tab | CtKeyCode::Enter => {
                    // `SDD-07` Fase 3: con `Enter` y el input ya igual al
                    // completado (slash sin argumento: `/go`, `/time`,
                    // `/cancel`…), la tecla es un **envío**, no un aceptado:
                    // se cierra el popup y la tecla cae al brazo de envío de
                    // `F1 Chat`. `Tab` sigue siendo aceptado puro.
                    if code == CtKeyCode::Enter && self.popup.selected_matches_input(&self.input) {
                        self.popup.close();
                    } else {
                        if self.popup.accept_into(&mut self.input) {
                            self.history.reset_navigation();
                            self.sync_popup();
                        }
                        return true;
                    }
                }
                _ => {}
            }
        }
        if matches!(code, CtKeyCode::Esc) {
            // `SDD-07` Fase 3 (seguridad UX crítica): con TimeFreeze activo,
            // Esc **vuelve al presente** (no cierra la aplicación, 0
            // mutación en disco); un segundo Esc ya en vivo sí cierra.
            if self.view == TuiView::Chat && self.time_frozen_active() {
                if let Some(td) = &self.time_debug {
                    td.return_to_present();
                }
                self.set_status_hint("Back at present (live) — Esc again to quit.".to_string());
                return true;
            }
            // `SDD-03` Fase 3: Esc sale solo desde `F1 Chat`; en el
            // resto de vistas vuelve a `F1 Chat`.
            if self.view == TuiView::Chat {
                self.quit.store(true, Ordering::Relaxed);
            } else {
                self.switch_view(TuiView::Chat);
            }
        } else if self.view == TuiView::Chat
            && self.active_approval.is_none()
            && ((modifiers.contains(KeyModifiers::ALT) && matches!(code, CtKeyCode::Up))
                || (modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(code, CtKeyCode::Char('p' | 'P'))))
        {
            if let Some(prev) = self.history.prev(self.input.as_str()) {
                self.input.set_text(prev);
                self.popup.close();
            }
        } else if self.view == TuiView::Chat
            && self.active_approval.is_none()
            && ((modifiers.contains(KeyModifiers::ALT) && matches!(code, CtKeyCode::Down))
                || (modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(code, CtKeyCode::Char('n' | 'N'))))
        {
            if let Some(next) = self.history.next() {
                self.input.set_text(next);
                self.popup.close();
            }
        } else if self.active_approval.is_none()
            && modifiers.contains(KeyModifiers::CONTROL)
            && matches!(code, CtKeyCode::Char('x' | 'X'))
        {
            self.pending_prompts.clear();
        } else if self.view == TuiView::Chat
            && self.active_approval.is_none()
            && !self.input.is_empty()
            && matches!(code, CtKeyCode::Home | CtKeyCode::End)
        {
            if matches!(code, CtKeyCode::Home) {
                self.input.move_home();
            } else {
                self.input.move_end();
            }
            self.sync_popup();
        } else if self.view == TuiView::Chat
            && apply_scroll_key(
                &mut self.scroll_from_bottom,
                self.last_viewport_height,
                code,
                modifiers,
            )
        {
            return true;
        } else if self.view == TuiView::Chat && self.active_approval.is_some() {
            // `REQ-03.6` (OBLIGATORIO): `y`/`n` solo en `F1 Chat` con
            // aprobación pendiente. En Chat cualquier otra tecla se
            // traga (arm de swallow).
            if let (Some(action), Some((agent, req, _, _))) = (
                approval_key_action(self.view, code),
                self.active_approval.clone(),
            ) {
                match action {
                    ApprovalKeyAction::Approve => {
                        self.queued.push(Reply::approve(agent, req).to_event());
                    }
                    ApprovalKeyAction::Deny => {
                        self.queued
                            .push(Reply::deny(agent, req, "denied by user in TUI").to_event());
                    }
                }
                self.active_approval = None;
                self.approval_diff = None;
                self.approval_impact = None;
            }
        } else if self.active_approval.is_some()
            && matches!(code, CtKeyCode::Char('y' | 'Y' | 'n' | 'N'))
        {
            // `SDD-03` Fase 4: fuera de Chat, `y`/`n` a ciegas **nunca**
            // emiten Reply — aviso accionable; el resto de teclas
            // (Up/Down/PgUp/PgDn/f) sigue navegando F2..F5 libremente.
            self.set_status_hint(
                "Approval pending — press F1 to review diff in Chat before deciding".to_string(),
            );
        } else {
            match self.view {
                TuiView::Chat => {
                    self.handle_chat_view_key(code, modifiers);
                }
                TuiView::Agents => {
                    self.handle_agents_view_key(code);
                }
                TuiView::Sprout => {
                    self.handle_sprout_view_key(code);
                }
                TuiView::PlanDiff => {
                    self.handle_plan_diff_key(code);
                }
                TuiView::Timeline => {
                    self.handle_timeline_view_key(code);
                }
            }
        }
        true
    }
}

impl Drop for TuiInner {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

impl PlatformDriver for TuiDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        let mut inner = self.inner.borrow_mut();
        if inner.queued.is_empty() {
            inner.poll_crossterm();
        }
        for ev in inner.queued.drain(..) {
            bus.push(ev);
        }
    }

    fn present(&mut self, graph: &dyn GraphView, _surfaces: &[SurfaceBinding]) {
        let mut inner = self.inner.borrow_mut();
        let busy = inner.is_any_agent_busy(graph);
        inner.advance_spinner_if_due(busy);
        inner.render(graph);
    }
}

impl TuiInner {
    fn render(&mut self, graph: &dyn GraphView) {
        let approvals = pending_approvals(graph, &self.vocab);
        if self.active_approval.is_none() {
            let current_mode = self.mode_handle.get();
            for a in &approvals {
                let Some(p) = &a.pending_approval else {
                    continue;
                };
                let mut merge_preview_diff: Option<String> = None;
                let impact_assessment = if p.tool == "subagent_merge" {
                    let static_dec = self.policy.as_ref().map_or(PermissionDecision::Ask, |pol| {
                        pol.evaluate_permission_in_mode(current_mode, &p.tool, &p.args)
                    });
                    if static_dec == PermissionDecision::Deny {
                        continue;
                    }
                    let job_id = serde_json::from_str::<serde_json::Value>(&p.args)
                        .ok()
                        .and_then(|v| v.get("job_id").and_then(|j| j.as_str()).map(str::to_string));
                    let preview = job_id.and_then(|jid| {
                        self.subagents.as_ref().and_then(|h| {
                            h.preview_merge_for_request(&jid, a.agent.0, a.request).ok()
                        })
                    });
                    match preview {
                        // `SDD-10` Fase 4: misma decisión efectiva que
                        // `SubagentManagerDriver` (gobernanza → `Ask` obligatorio); si no es
                        // `Ask`, el driver auto-aprueba y el prompt no debe mostrarse.
                        Some(MergePreviewState::Clean(ticket)) => {
                            if effective_merge_decision(static_dec, &ticket)
                                != PermissionDecision::Ask
                            {
                                continue;
                            }
                            merge_preview_diff =
                                Some(truncate_diff_lines(&ticket.unified_diff, 14));
                            ticket.impact
                        }
                        Some(MergePreviewState::NoOp) => {
                            if static_dec != PermissionDecision::Ask {
                                continue;
                            }
                            None
                        }
                        Some(MergePreviewState::Conflict { .. }) => continue,
                        // `SDD-10` Fase 3: Control Plane → sin banner; `SubagentManagerDriver`
                        // niega la solicitud de plano (disco de `root` intacto).
                        Some(MergePreviewState::ControlPlaneDenied { .. }) => continue,
                        // `SDD-10` Fase 4: alineado con `CliApprovalDriver`: sin preview no hay
                        // `y/n`; el driver auto-aprueba para que `subagent_merge` devuelva el
                        // error exacto a `root` (evita un prompt que el driver no espera).
                        None => continue,
                    }
                } else if let Some(tools) = &self.tools {
                    let (decision, impact) =
                        tools.evaluate_permission_with_impact(current_mode, &p.tool, &p.args);
                    if decision != PermissionDecision::Ask {
                        // Auto-approved or auto-denied by `WorkspaceToolBridge` per policy + impact.
                        continue;
                    }
                    impact
                } else {
                    if let Some(policy) = &self.policy {
                        if policy.evaluate_permission_in_mode(current_mode, &p.tool, &p.args)
                            != PermissionDecision::Ask
                        {
                            continue;
                        }
                    }
                    None
                };
                if self.seen_approvals.insert((a.agent.0, a.request)) {
                    self.approval_diff = merge_preview_diff.or_else(|| {
                        self.workspace_root
                            .as_deref()
                            .and_then(|root| preview_tool_diff(root, &p.tool, &p.args, 14))
                    });
                    self.approval_impact = impact_assessment;
                    self.active_approval =
                        Some((a.agent, a.request, p.tool.clone(), p.args.clone()));
                    break;
                }
            }
        }

        let bg_jobs: Vec<SubagentJobSummary> = self
            .subagents
            .as_ref()
            .map(SubagentHandle::list_summaries)
            .unwrap_or_default();
        let active_bg_jobs = bg_jobs.iter().filter(|j| j.status == "running").count();

        let agents = graph.query(&[self.vocab.state]);
        let agent_rows: Vec<AgentRow> = agents
            .iter()
            .map(|id| AgentRow {
                id: *id,
                name: prop_text(graph, *id, self.vocab.name).unwrap_or_default(),
                is_root: graph.related(*id, self.vocab.rel_parent).is_empty(),
                state: prop_text(graph, *id, self.vocab.state).unwrap_or_default(),
                goal: prop_text(graph, *id, self.vocab.goal).unwrap_or_default(),
            })
            .collect();
        self.last_agent_ids = agents.clone();
        if self.agent_picker_idx >= agent_rows.len() {
            self.agent_picker_idx = agent_rows.len().saturating_sub(1);
        }
        // `SDD-03` Fase 3: caches para `Ctrl+K` / `F2 Agents` fuera del render
        // (`poll_crossterm` no recibe `graph`).
        self.last_agent_rows = agent_rows.clone();
        self.last_bg_jobs = bg_jobs.clone();

        let explicit_switch = self.pending_focus_switch.take();
        if let Some(switch) = explicit_switch {
            if !agent_rows.is_empty() {
                match switch {
                    FocusSwitch::Next => {
                        let next_idx = self
                            .focused_agent
                            .and_then(|cur| agent_rows.iter().position(|r| r.id == cur))
                            .map(|idx| (idx + 1) % agent_rows.len())
                            .unwrap_or(0);
                        self.focused_agent = Some(agent_rows[next_idx].id);
                    }
                    FocusSwitch::Target(query) => {
                        let q_lower = query.to_ascii_lowercase();
                        let matched = if let Ok(target_num) = query.parse::<u64>() {
                            agent_rows
                                .iter()
                                .find(|r| r.id.0 == target_num)
                                .map(|r| r.id)
                        } else {
                            agent_rows
                                .iter()
                                .find(|r| r.name.eq_ignore_ascii_case(&query))
                                .or_else(|| {
                                    agent_rows
                                        .iter()
                                        .find(|r| r.name.to_ascii_lowercase().contains(&q_lower))
                                })
                                .map(|r| r.id)
                        };
                        if let Some(id) = matched {
                            self.focused_agent = Some(id);
                        }
                    }
                }
            }
        }

        let (active_agent, root_changed) =
            resolve_focused_agent(&agent_rows, self.focused_agent, &mut self.last_known_root);
        self.focused_agent = active_agent;
        if root_changed {
            self.scroll_from_bottom = 0;
        }

        let any_agent_active = agent_rows
            .iter()
            .any(|r| matches!(r.state.as_str(), "thinking" | "acting" | "waiting_children"));
        let latest_root_busy = agent_rows.iter().rfind(|r| r.is_root).is_some_and(|r| {
            matches!(
                r.state.as_str(),
                "thinking" | "acting" | "waiting_children" | "waiting_approval"
            )
        });
        self.root_busy = self.active_approval.is_some() || latest_root_busy;
        let active_subagents = agent_rows
            .iter()
            .filter(|r| {
                !r.is_root
                    && matches!(
                        r.state.as_str(),
                        "thinking" | "acting" | "waiting_children" | "waiting_approval"
                    )
            })
            .count();

        let is_busy = self.active_approval.is_none() && any_agent_active;
        let spinner_angle = if is_busy { self.spinner_angle } else { 0.0 };

        let mut header_lines: Vec<Line<'static>> = Vec::with_capacity(3);
        let mut log_lines: Vec<Line<'static>> = Vec::new();
        let mut latest_agent_memory: Option<(String, Vec<(String, String)>)> = None;
        let mut compact_count = 0usize;
        // `SDD-03` Fase 3: cabecera LLM viva (`F1`) — si hay handle, el
        // objetivo actual (`/profile`/`/provider`/`/model`/`/effort` en
        // caliente); si no, el arranque/telemetría.
        let fallback_model = if self.model_name.is_empty() {
            self.telemetry.snapshot().model
        } else {
            self.model_name.clone()
        };
        let (model_display, live_effort) = resolve_live_llm_header(
            self.llm_profile.as_ref(),
            &fallback_model,
            self.reasoning_effort,
        );
        let effort_display = live_effort.map(ReasoningEffort::as_str).unwrap_or("auto");

        if let Some(id) = active_agent {
            let active_row = agent_rows.iter().find(|r| r.id == id);
            let name = active_row.map(|r| r.name.as_str()).unwrap_or("");
            let state = active_row.map(|r| r.state.as_str()).unwrap_or("");
            let goal = active_row.map(|r| r.goal.clone()).unwrap_or_default();
            let role_tag = if active_row.map(|r| r.is_root).unwrap_or(true) {
                "root"
            } else {
                "sub"
            };

            // Row 1: goal (single line, no wrap so phase is never pushed out).
            header_lines.push(Line::from(vec![
                Span::styled("goal:  ", Style::default().fg(Color::DarkGray)),
                Span::raw(sanitize_display_line(&goal)),
            ]));

            // Row 2: phase + focused agent identity + compact multi-agent indicator.
            let phase_label = if self.active_approval.is_some() {
                "awaiting human approval ([y] / [n])"
            } else {
                match state {
                    "thinking" => "thinking / streaming...",
                    "acting" => "executing workspace tool...",
                    "waiting_children" => "waiting for subagent(s)...",
                    "done" => "settled (ready for follow-up)",
                    "fail" => "failed (enter follow-up or /new)",
                    _ => "idle",
                }
            };
            let phase_color = if is_busy {
                Color::Rgb(255, 248, 231)
            } else if self.active_approval.is_some() {
                Color::Yellow
            } else {
                Color::Gray
            };
            let mut phase_spans = vec![
                Span::styled("phase: ", Style::default().fg(Color::DarkGray)),
                Span::styled(phase_label, Style::default().fg(phase_color)),
                Span::styled(
                    format!("  · #{id_num} {name} [{role_tag}]", id_num = id.0),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if active_subagents > 0 || active_bg_jobs > 0 {
                let total_sub = active_subagents + active_bg_jobs;
                let tag = if active_bg_jobs > 0 {
                    format!("  [subagents:{total_sub} · bg:{active_bg_jobs} · F2]")
                } else {
                    format!("  [subagents:{active_subagents} · F2]")
                };
                phase_spans.push(Span::styled(
                    tag,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            } else if agent_rows.len() > 1 || !bg_jobs.is_empty() {
                let active_count = agent_rows
                    .iter()
                    .filter(|r| {
                        matches!(r.state.as_str(), "thinking" | "acting" | "waiting_children")
                    })
                    .count();
                phase_spans.push(Span::styled(
                    format!("  [{}/{} agents · F2]", active_count, agent_rows.len()),
                    Style::default()
                        .fg(if active_count > 0 {
                            Color::Cyan
                        } else {
                            Color::DarkGray
                        })
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if !self.pending_prompts.is_empty() {
                phase_spans.push(Span::styled(
                    format!("  [queued:{}]", self.pending_prompts.len()),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if let Some(vlabel) = self.active_session_label() {
                phase_spans.push(Span::styled(
                    format!("  · vault:{vlabel}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            header_lines.push(Line::from(phase_spans));

            // Row 3: compact single-line agent strip only when >1 agent exists; otherwise quiet hint.
            if agent_rows.len() > 1 {
                let mut strip_spans = vec![Span::styled(
                    "focus: ",
                    Style::default().fg(Color::DarkGray),
                )];
                for (i, row) in agent_rows.iter().enumerate() {
                    if i > 0 {
                        strip_spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
                    }
                    let is_cur = row.id == id;
                    let short_state = match row.state.as_str() {
                        "waiting_children" => "wait",
                        "waiting_approval" => "appr",
                        "thinking" => "think",
                        "acting" => "act",
                        other => other,
                    };
                    let label = if is_cur {
                        format!("*{}:{} ({short_state})", row.id.0, row.name)
                    } else {
                        format!("{}:{} ({short_state})", row.id.0, row.name)
                    };
                    let style = if is_cur {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else if matches!(
                        row.state.as_str(),
                        "thinking" | "acting" | "waiting_children"
                    ) {
                        Style::default().fg(Color::Cyan)
                    } else if row.state == "waiting_approval" {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    strip_spans.push(Span::styled(label, style));
                }
                header_lines.push(Line::from(strip_spans));
            } else {
                header_lines.push(Line::from(Span::styled(
                    "Tab: Plan/Build · F2: Agents · /rewind · /fork · /services · Esc: quit",
                    Style::default().fg(Color::DarkGray),
                )));
            }

            if !goal.is_empty() {
                push_multiline_entry(&mut log_lines, "user", Color::LightCyan, &goal);
            }

            let mut mem_pairs: Vec<(String, String)> = Vec::new();
            for mem_id in graph.related(id, self.vocab.rel_memory) {
                let kind = prop_text(graph, mem_id, self.vocab.mem_kind).unwrap_or_default();
                let text = prop_text(graph, mem_id, self.vocab.mem_text).unwrap_or_default();
                if kind == "system" && text.starts_with(COMPACT_MARKER) {
                    compact_count += 1;
                }
                let color = match kind.as_str() {
                    "call" => Color::Yellow,
                    "observe" => Color::Green,
                    "tool_error" | "denied" => Color::Red,
                    "user" => Color::LightCyan,
                    "system" => Color::Magenta,
                    _ => Color::Cyan,
                };
                push_multiline_entry(&mut log_lines, &kind, color, &text);
                mem_pairs.push((kind, text));
            }
            latest_agent_memory = Some((goal, mem_pairs));

            if let Some(res) = prop_text(graph, id, self.vocab.result) {
                if !res.is_empty() {
                    push_multiline_entry(&mut log_lines, "result", Color::LightGreen, &res);
                }
            }

            if state == "thinking" {
                if let Some(preview) = self.stream_preview.get() {
                    if preview.agent == id.0 && !preview.text.is_empty() {
                        push_multiline_entry(
                            &mut log_lines,
                            "stream",
                            Color::LightCyan,
                            &format!("{}▍", preview.text),
                        );
                    }
                }
            }
        } else {
            header_lines.push(Line::from(Span::styled(
                "idle — enter a goal below",
                Style::default().fg(Color::Gray),
            )));
            let active_vault_label = self.active_session_label();
            let mut idle_meta = vec![
                Span::styled("model: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    if model_display.is_empty() {
                        "(default)".to_string()
                    } else {
                        model_display.clone()
                    },
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("  ·  effort: {effort_display}"),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if let Some(ref vlabel) = active_vault_label {
                idle_meta.push(Span::styled(
                    format!("  ·  vault: {vlabel}"),
                    Style::default().fg(Color::Cyan),
                ));
            }
            header_lines.push(Line::from(idle_meta));
            header_lines.push(Line::from(Span::styled(
                "Tab: Plan/Build · F2: Agents · /rewind · /fork · /services · Esc: quit",
                Style::default().fg(Color::DarkGray),
            )));
        }

        let mut telemetry_bar = self.telemetry.snapshot().format_status_bar();
        if let Some((goal, mem_pairs)) = latest_agent_memory {
            let current_snap = self.telemetry.snapshot();
            let projected = project_memory(&goal, &mem_pairs, &self.context_config);
            if compact_count != self.seen_compact_count
                || (current_snap.cumulative.input_tokens == 0 && !mem_pairs.is_empty())
            {
                self.telemetry
                    .sync_estimated_context_bytes(projected.total_bytes);
                self.seen_compact_count = compact_count;
                telemetry_bar = self.telemetry.snapshot().format_status_bar();
            }
            if projected.highest_tier == ToolOutputTier::Tier2Compacted {
                telemetry_bar.push_str(" [compacted]");
            }
        }
        let input_snapshot = self.input.clone();
        let popup_snapshot = self.popup.clone();
        let queued_count = self.pending_prompts.len();
        let queued_preview = self.pending_prompts.peek_front().map(str::to_string);
        let root_busy_snapshot = self.root_busy;
        let approval_snapshot = self.active_approval.clone();
        let approval_agent_label = approval_snapshot.as_ref().map(|(aid, _, _, _)| {
            let name = agent_rows
                .iter()
                .find(|r| r.id == *aid)
                .map(|r| r.name.as_str())
                .unwrap_or("");
            if name.is_empty() {
                format!("#{}", aid.0)
            } else {
                format!("#{} {}", aid.0, name)
            }
        });
        let diff_snapshot = self.approval_diff.clone();
        let impact_snapshot = self.approval_impact.clone();
        let plan_snapshot = self
            .ws_vocab
            .as_ref()
            .and_then(|wv| graph_workspace_plan(graph, wv));
        let services_snapshot = self
            .ws_vocab
            .as_ref()
            .and_then(|wv| graph_workspace_services(graph, wv))
            .or_else(|| self.sprout.as_ref().and_then(|s| s.render_panel_text()));
        let mode_label = self.mode_handle.get().as_label();
        let native_driver = cached_sandbox_driver();
        let sandbox_badge = self
            .policy
            .as_ref()
            .map(|p| p.sandbox.mode.effective_badge(native_driver))
            .unwrap_or("workspace");
        let untrusted = self
            .policy
            .as_ref()
            .is_some_and(|p| !p.trust.is_fully_trusted());
        let header_title = format_header_title_with_trust(
            mode_label,
            sandbox_badge,
            untrusted,
            &model_display,
            live_effort,
        );
        let picker_open = self.agent_picker_open;
        let picker_idx = self.agent_picker_idx;
        // `SDD-03` Fase 3: datos de las vistas efímeras (`F2..F5`) y de la
        // paleta `Ctrl+K` — se calculan aquí para que el closure de `draw`
        // no capture `self`.
        let view = self.view;
        let agents_total = agent_rows.len() + bg_jobs.len();
        let agents_selected = if agents_total == 0 {
            0
        } else {
            self.agents_selected.min(agents_total - 1)
        };
        let sprout_view_data = (view == TuiView::Sprout).then(|| {
            let rows = self.sprout_service_rows();
            if rows.is_empty() {
                self.sprout_selected = 0;
            } else {
                self.sprout_selected = self.sprout_selected.min(rows.len() - 1);
            }
            let gate = sprout_mutation_gate(self.mode_handle.get(), self.sprout_untrusted());
            let logs = rows
                .get(self.sprout_selected)
                .and_then(|row| {
                    self.sprout
                        .as_ref()
                        .and_then(|s| s.logs(&row.id, Some(200), None, false).ok())
                })
                .unwrap_or_default();
            (rows, self.sprout_selected, self.sprout_follow, logs, gate)
        });
        let plan_view_effects = (view == TuiView::PlanDiff).then(|| {
            self.ws_vocab
                .as_ref()
                .map(|wv| {
                    all_recorded_effects(graph, wv)
                        .into_iter()
                        .map(|e| EffectRow {
                            tick: e.tick,
                            tool: e.tool,
                            target: e.target,
                            reversibility: e.reversibility,
                            undone: is_effect_undone(graph, wv, e.id),
                            symbols: e.symbols,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        });
        let plan_scroll = if view == TuiView::PlanDiff {
            let max = plan_view_effects.as_ref().map_or(0, |e| e.len());
            self.plan_scroll.min(max)
        } else {
            0
        };
        let timeline_view_data = (view == TuiView::Timeline).then(|| {
            let rows = self.timeline_rows();
            if rows.is_empty() {
                self.timeline_selected = 0;
            } else {
                self.timeline_selected = self.timeline_selected.min(rows.len() - 1);
            }
            let (cursor, present) = self
                .time_debug
                .as_ref()
                .map_or((0, 0), |td| (td.cursor(), td.present()));
            (rows, cursor, present, self.timeline_selected)
        });
        let status_hint_snapshot = self.status_hint.clone();
        // `SDD-07` Fase 3: snapshot del depurador temporal para el banner
        // ámbar de `F1 Chat` (el closure `draw` no puede capturar `self`).
        // Incluye `· revert on branch: N file(s)` cuando el cursor mira el
        // pasado: **solo RAM** (`effects_after_tick`), 0 I/O.
        let time_banner: Option<String> = self
            .time_debug
            .as_ref()
            .filter(|_| view == TuiView::Chat)
            .filter(|td| td.is_frozen() || td.cursor() < td.present())
            .map(|td| {
                let mut banner = format!(
                    "⏸ TIME FROZEN · tick {} / present {} · Alt+←/→ scrub · \
                     !cmd /graph /step /step-llm · /go resume · Esc present",
                    td.cursor(),
                    td.present()
                );
                if td.cursor() < td.present() {
                    let files = self
                        .ws_vocab
                        .as_ref()
                        .map(|wv| td.pending_revert_files(graph, wv).len())
                        .unwrap_or(0);
                    banner.push_str(&format!(" · revert on branch: {files} file(s)"));
                }
                banner
            });
        let time_frozen_render = time_banner.is_some();
        let palette_open = self.palette.is_open();
        let palette_visible: Vec<PaletteItem> = if palette_open {
            self.palette.visible_items().into_iter().cloned().collect()
        } else {
            Vec::new()
        };
        let palette_selected = self.palette.selected;
        let palette_query = self.palette.query.clone();
        let terminal = &mut self.terminal;
        let scroll_from_bottom = &mut self.scroll_from_bottom;
        let last_viewport_height = &mut self.last_viewport_height;
        let _ = terminal.draw(|f| {
            let is_approval = approval_snapshot.is_some();
            let (bottom_title, bottom_lines, border_color) = if view != TuiView::Chat {
                // `SDD-03` Fase 3: en vistas efímeras la franja inferior muestra
                // la ayuda de teclas de la vista (+ aviso transitorio).
                let hint = match view {
                    TuiView::Agents => " F2 Agents — Up/Down select · Enter focus → F1 Chat · Esc back ",
                    TuiView::Sprout => " F3 Sprout — Up/Down · s start · x stop · r restart · a all · f follow · Esc back ",
                    TuiView::PlanDiff => " F4 Plan & Diff — Up/Down scroll · Esc back ",
                    TuiView::Timeline => " F5 Timeline — Up/Down select · Enter scrub (frozen preview) · r /rewind <tick> (disk) · f /fork · Esc back ",
                    TuiView::Chat => unreachable!(),
                };
                let mut lines = Vec::new();
                if let Some(h) = &status_hint_snapshot {
                    lines.push(Line::from(Span::styled(
                        h.clone(),
                        Style::default().fg(Color::Yellow),
                    )));
                }
                (hint.to_string(), lines, Color::White)
            } else {
                match &approval_snapshot {
                Some((_, _, tool, args)) => {
                    let lines = build_approval_lines(
                        tool,
                        args,
                        diff_snapshot.as_deref(),
                        impact_snapshot.as_ref(),
                    );
                    let agent_tag = approval_agent_label
                        .as_deref()
                        .map(|lbl| format!(" [agent {lbl}]"))
                        .unwrap_or_default();
                    let box_color = if impact_snapshot
                        .as_ref()
                        .is_some_and(|imp| imp.hard_threshold_hit)
                    {
                        Color::Red
                    } else {
                        Color::Yellow
                    };
                    (
                        format!(" Approval Required{agent_tag} ([y] Approve / [n] Deny) "),
                        lines,
                        box_color,
                    )
                }
                None => {
                    let queue_tag = if queued_count > 0 {
                        format!(" · queued:{queued_count} (Ctrl+X clear)")
                    } else if root_busy_snapshot {
                        " · typing queues follow-up".to_string()
                    } else {
                        String::new()
                    };
                    // `SDD-03` Fase 4: el aviso transitorio (`/view` inválido,
                    // gate F3, …) también se ve en Chat — línea amarilla sobre
                    // el input, TTL en `pump_idle`.
                    let mut lines = Vec::new();
                    if let Some(h) = &status_hint_snapshot {
                        lines.push(Line::from(Span::styled(
                            h.clone(),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                    lines.push(input_snapshot.render_line("> "));
                    if input_snapshot.is_empty() {
                        if let Some(next_q) = &queued_preview {
                            lines.push(Line::from(Span::styled(
                                format!("  [queued #1/{queued_count}] {}", sanitize_display_line(next_q)),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                    (
                        format!(
                            " [{mode_label}{queue_tag}] Goal / Command (/ cmds · @ files · Alt+↑/↓ history · Tab Plan/Build · F2 Agents) "
                        ),
                        lines,
                        // `SDD-07` Fase 3: input caja en ámbar bajo TimeFreeze.
                        if time_frozen_render {
                            Color::Yellow
                        } else if queued_count > 0 {
                            Color::Cyan
                        } else {
                            Color::White
                        },
                    )
                }
                }
            };

            let bottom_inner_width = f.area().width.saturating_sub(2);
            let bottom_rows = visual_row_count(&bottom_lines, bottom_inner_width).max(1);
            let max_bottom = f.area().height.saturating_sub(8).max(4);
            let bottom_height = if is_approval {
                u16::try_from(bottom_rows.saturating_add(2))
                    .unwrap_or(16)
                    .clamp(5, 16)
                    .min(max_bottom)
            } else {
                u16::try_from(bottom_rows.saturating_add(2))
                    .unwrap_or(4)
                    .clamp(4, 8)
                    .min(max_bottom)
            };
            let bottom_inner_height = usize::from(bottom_height.saturating_sub(2));
            let bottom_scroll = if is_approval {
                0
            } else {
                bottom_rows.saturating_sub(bottom_inner_height)
            };

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Min(5),
                    Constraint::Length(bottom_height),
                ])
                .split(f.area());

            let header_block = Block::default()
                .title(header_title)
                // `SDD-03` Fase 3: tira de pestañas `F1..F5` (+ badge
                // `[F1 Chat !APPROVAL]` fuera de Chat con aprobación pendiente).
                .title_top(view_tabs_line(view, is_approval).right_aligned())
                .title_bottom(
                    Line::from(Span::styled(
                        format!(" {telemetry_bar} "),
                        Style::default().fg(Color::Cyan),
                    ))
                    .right_aligned(),
                )
                .borders(Borders::ALL);
            let header_inner = header_block.inner(chunks[0]);
            f.render_widget(header_block, chunks[0]);

            let header_cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(6),
                    Constraint::Length(1),
                    Constraint::Min(10),
                ])
                .split(header_inner);

            render_onyx_emblem(f, header_cols[0], spinner_angle, is_busy);

            // Render header without wrapping so `goal`, `phase`, and `focus` stay pinned to their rows.
            let header_text = Paragraph::new(header_lines);
            f.render_widget(header_text, header_cols[2]);

            if view == TuiView::Chat {
                // `SDD-07` Fase 3: banner ámbar negrita **arriba** de la traza
                // (línea sola sin bloque: `Length(1)` no deja sitio para borde).
                let (chat_area, banner_area) =
                    if let Some(banner) = &time_banner {
                        let vsplit = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([Constraint::Length(1), Constraint::Min(4)])
                            .split(chunks[1]);
                        (vsplit[1], Some((vsplit[0], banner.clone())))
                    } else {
                        (chunks[1], None)
                    };
                if let Some((area, text)) = banner_area {
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            text,
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ))),
                        area,
                    );
                }
                let has_side_panel = plan_snapshot.is_some() || services_snapshot.is_some();
                let (trace_area, side_area) = if has_side_panel && f.area().width >= 100 {
                let hsplit = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
                    .split(chat_area);
                (hsplit[0], Some(hsplit[1]))
            } else {
                (chat_area, None)
            };

            let viewport_height = trace_area.height.saturating_sub(2) as usize;
            *last_viewport_height = viewport_height.max(1);
            let inner_width = trace_area.width.saturating_sub(2);
            let total_rows = visual_row_count(&log_lines, inner_width);
            let max_scroll = total_rows.saturating_sub(viewport_height);
            *scroll_from_bottom = (*scroll_from_bottom).min(max_scroll);
            let scroll_top = max_scroll.saturating_sub(*scroll_from_bottom);

            let trace_title = if *scroll_from_bottom > 0 {
                format!(
                    " Memory & Trace [SCROLLED -{} lines | End to follow] ",
                    *scroll_from_bottom
                )
            } else {
                " Memory & Trace (Up/Down/PgUp/PgDn/Home/End) ".to_string()
            };

            let body = Paragraph::new(log_lines)
                .scroll((u16::try_from(scroll_top).unwrap_or(u16::MAX), 0))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(trace_title)
                        .borders(Borders::ALL)
                        // `SDD-07` Fase 3: borde ámbar mientras hay TimeFreeze.
                        .border_style(if time_frozen_render {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default()
                        }),
                );
            f.render_widget(body, trace_area);

            if let Some(right_area) = side_area {
                let (plan_area, services_area) =
                    match (plan_snapshot.as_deref(), services_snapshot.as_deref()) {
                        (Some(_), Some(_)) => {
                            let vsplit = Layout::default()
                                .direction(Direction::Vertical)
                                .constraints([
                                    Constraint::Percentage(50),
                                    Constraint::Percentage(50),
                                ])
                                .split(right_area);
                            (Some(vsplit[0]), Some(vsplit[1]))
                        }
                        (Some(_), None) => (Some(right_area), None),
                        (None, Some(_)) => (None, Some(right_area)),
                        (None, None) => (None, None),
                    };

                if let (Some(p_area), Some(plan_text)) = (plan_area, plan_snapshot.as_deref()) {
                    let mut plan_lines: Vec<Line<'static>> = Vec::new();
                    for pline in plan_text.lines() {
                        let trimmed = pline.trim_start();
                        let style = if trimmed.starts_with("[x]") {
                            Style::default().fg(Color::Green)
                        } else if trimmed.starts_with("[>]") {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else if trimmed.starts_with("[ ]") {
                            Style::default().fg(Color::Gray)
                        } else {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        };
                        plan_lines.push(Line::from(Span::styled(pline.to_string(), style)));
                    }
                    let plan_widget = Paragraph::new(plan_lines)
                        .wrap(Wrap { trim: false })
                        .block(Block::default().title(" Live Plan ").borders(Borders::ALL));
                    f.render_widget(plan_widget, p_area);
                }

                if let (Some(s_area), Some(svc_text)) = (services_area, services_snapshot.as_deref())
                {
                    let svc_lines = render_services_lines(svc_text);
                    let svc_widget = Paragraph::new(svc_lines)
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .title(" Services (Sprout) ")
                                .borders(Borders::ALL),
                        );
                    f.render_widget(svc_widget, s_area);
                }
            }
            } else {
                // `SDD-03` Fase 3: vista efímera a pantalla completa (sin traza).
                match view {
                    TuiView::Agents => {
                        render_agents_view(
                            f,
                            chunks[1],
                            &agent_rows,
                            &bg_jobs,
                            active_agent,
                            agents_selected,
                        );
                    }
                    TuiView::Sprout => {
                        if let Some((rows, sel, follow, logs, gate)) = &sprout_view_data {
                            render_sprout_view(f, chunks[1], rows, *sel, *follow, logs, *gate);
                        }
                    }
                    TuiView::PlanDiff => {
                        let effects = plan_view_effects.as_deref().unwrap_or(&[]);
                        render_plan_diff_view(
                            f,
                            chunks[1],
                            plan_snapshot.as_deref(),
                            effects,
                            plan_scroll,
                        );
                    }
                    TuiView::Timeline => {
                        if let Some((rows, cursor, present, sel)) = &timeline_view_data {
                            render_timeline_view(f, chunks[1], rows, *cursor, *present, *sel);
                        }
                    }
                    TuiView::Chat => unreachable!(),
                }
            }

            let mut bottom = Paragraph::new(bottom_lines)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(bottom_title)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border_color)),
                );
            if bottom_scroll > 0 {
                bottom = bottom.scroll((u16::try_from(bottom_scroll).unwrap_or(u16::MAX), 0));
            }
            f.render_widget(bottom, chunks[2]);

            if !palette_open {
                if picker_open {
                    render_agent_picker_with_subagents(
                        f,
                        f.area(),
                        &agent_rows,
                        &bg_jobs,
                        active_agent,
                        picker_idx,
                    );
                } else if !is_approval && popup_snapshot.is_open() {
                    render_input_popup(f, f.area(), chunks[2], &popup_snapshot);
                }
            }
            // `SDD-03` Fase 3: la paleta `Ctrl+K` se dibuja por encima de todo.
            if palette_open {
                render_palette(f, f.area(), &palette_visible, palette_selected, &palette_query);
            }
        });
    }
}

/// Builds the approval banner lines, prepending any Scent-Heated `# impact: ...` summary above
/// the tool summary and colorized unified diff (`REQ-MIX1.3`, `TEST-MIX1.6`).
pub(crate) fn build_approval_lines(
    tool: &str,
    args: &str,
    diff: Option<&str>,
    impact: Option<&ImpactAssessment>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(imp) = impact {
        let style = if imp.hard_threshold_hit {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(Span::styled(imp.summary_line.clone(), style)));
    }
    let args_preview = if diff.is_some() && args.len() > 100 {
        let mut cut = 100;
        while !args.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &args[..cut])
    } else {
        args.to_string()
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("{tool}: "),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(args_preview),
    ]));
    if let Some(diff_text) = diff {
        for dline in diff_text.lines() {
            let style = if dline.starts_with("+++") || dline.starts_with("---") {
                Style::default().fg(Color::DarkGray)
            } else if dline.starts_with("@@") {
                Style::default().fg(Color::Cyan)
            } else if dline.starts_with('+') {
                Style::default().fg(Color::Green)
            } else if dline.starts_with('-') {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(dline.to_string(), style)));
        }
    }
    lines
}

/// Returns `true` for slash commands that should execute immediately instead of waiting in the
/// follow-up prompt queue while the root agent is busy.
fn is_immediate_slash_cmd(cmd: &str) -> bool {
    let lower = cmd.trim().to_ascii_lowercase();
    lower == "/mode"
        || lower.starts_with("/mode ")
        || lower == "/profile"
        || lower.starts_with("/profile ")
        || lower == "/provider"
        || lower.starts_with("/provider ")
        || lower == "/model"
        || lower.starts_with("/model ")
        || lower == "/effort"
        || lower.starts_with("/effort ")
        || lower == "/services"
        || lower.starts_with("/services ")
        || lower == "/service"
        || lower.starts_with("/service ")
        || lower == "/subagents"
        || lower == "/subagent"
        || lower == "/spawn"
        || lower.starts_with("/spawn ")
        || lower.starts_with("/merge ")
        || lower.starts_with("/discard ")
        || lower == "/trust"
        || lower.starts_with("/trust ")
        || lower == "/undo"
        || lower == "/cancel"
        || lower.starts_with("/cancel ")
        || lower == "/stop"
        || lower.starts_with("/stop ")
}

fn cached_sandbox_driver() -> onyx_workspace::SandboxDriverKind {
    use std::sync::OnceLock;
    static DRIVER: OnceLock<onyx_workspace::SandboxDriverKind> = OnceLock::new();
    *DRIVER.get_or_init(|| onyx_workspace::detect_sandbox_capabilities().native_driver)
}

/// Formats the non-invasive top header title showing mode, sandbox, model, and reasoning effort.
#[allow(dead_code)]
pub(crate) fn format_header_title(
    mode_label: &str,
    sandbox_badge: &str,
    model_name: &str,
    effort: Option<ReasoningEffort>,
) -> String {
    format_header_title_with_trust(mode_label, sandbox_badge, false, model_name, effort)
}

/// Formats the top header title including `[UNTRUSTED]` when Workspace Trust restricts local config.
pub(crate) fn format_header_title_with_trust(
    mode_label: &str,
    sandbox_badge: &str,
    untrusted: bool,
    model_name: &str,
    effort: Option<ReasoningEffort>,
) -> String {
    let trust_tag = if untrusted { " [UNTRUSTED]" } else { "" };
    let model = model_name.trim();
    if model.is_empty() {
        format!(" openOnyx [{mode_label}] [SANDBOX:{sandbox_badge}]{trust_tag} ")
    } else {
        let effort_label = effort.map(ReasoningEffort::as_str).unwrap_or("auto");
        format!(
            " openOnyx [{mode_label}] [SANDBOX:{sandbox_badge}]{trust_tag} · {model} (effort:{effort_label}) "
        )
    }
}

/// `SDD-03` Fase 3: cabecera LLM **viva**. Con handle presente, el objetivo
/// actual (`/profile`/`/provider`/`/model`/`/effort` aplicados en caliente en
/// la frontera de tick) opaca al campo estático de arranque; sin handle, se
/// degrada al arranque/telemetría (`fallback_model`/`fallback_effort`).
pub(crate) fn resolve_live_llm_header(
    handle: Option<&LlmProfileHandle>,
    fallback_model: &str,
    fallback_effort: Option<ReasoningEffort>,
) -> (String, Option<ReasoningEffort>) {
    match handle {
        Some(h) => {
            let snap = h.public_snapshot();
            let effort = if snap.effort == "auto" {
                None
            } else {
                ReasoningEffort::parse(&snap.effort).ok()
            };
            // `SDD-03` Fase 4: `provider/model` — el cambio de proveedor al
            // vuelo se ve en la cabecera sin abrir la paleta.
            (format!("{}/{}", snap.provider, snap.model), effort)
        }
        None => (fallback_model.to_string(), fallback_effort),
    }
}

/// Resolves which agent the TUI should display:
/// - Automatically follows a newly created root agent (e.g. after `/new`).
/// - Never lets a spawned subagent hijack focus away from the currently viewed agent.
/// - Preserves an explicit user focus choice as long as that agent still exists.
pub(crate) fn resolve_focused_agent(
    rows: &[AgentRow],
    current_focus: Option<ConceptId>,
    last_known_root: &mut Option<ConceptId>,
) -> (Option<ConceptId>, bool) {
    let latest_root = rows.iter().rfind(|r| r.is_root).map(|r| r.id);
    let root_changed = latest_root.is_some() && latest_root != *last_known_root;
    *last_known_root = latest_root;
    if root_changed {
        return (latest_root, true);
    }
    let valid_current = current_focus.filter(|id| rows.iter().any(|r| r.id == *id));
    let resolved = valid_current
        .or(latest_root)
        .or_else(|| rows.first().map(|r| r.id));
    (resolved, false)
}

/// Renders the centered ephemeral Agent Selector popup (`F2` / `/agent`), keeping backward compatibility.
#[allow(dead_code)]
pub(crate) fn render_agent_picker(
    f: &mut Frame<'_>,
    area: Rect,
    rows: &[AgentRow],
    focused: Option<ConceptId>,
    selected_idx: usize,
) {
    render_agent_picker_with_subagents(f, area, rows, &[], focused, selected_idx);
}

/// Renders the centered ephemeral Agent Selector popup (`F2` / `/agent`) including background worktree jobs.
pub(crate) fn render_agent_picker_with_subagents(
    f: &mut Frame<'_>,
    area: Rect,
    rows: &[AgentRow],
    bg_jobs: &[SubagentJobSummary],
    focused: Option<ConceptId>,
    selected_idx: usize,
) {
    if area.width < 20 || area.height < 6 {
        return;
    }
    let popup_w = area.width.saturating_sub(6).clamp(32, 82).min(area.width);
    let desired_h =
        u16::try_from(rows.len().saturating_add(bg_jobs.len()).saturating_add(5)).unwrap_or(18);
    let popup_h = desired_h
        .clamp(6, 20)
        .min(area.height.saturating_sub(2).max(5));
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup_area = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup_area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() && bg_jobs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no agents yet — enter a goal to start)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let max_goal_chars = usize::from(popup_w.saturating_sub(34)).max(12);
        for (idx, row) in rows.iter().enumerate() {
            let is_sel = idx == selected_idx;
            let is_cur = Some(row.id) == focused;
            let marker = if is_cur { "*" } else { " " };
            let role = if row.is_root { "root" } else { "sub " };
            let state_color = match row.state.as_str() {
                "thinking" | "acting" | "waiting_children" => Color::Cyan,
                "waiting_approval" => Color::Yellow,
                "done" => Color::Green,
                "fail" => Color::Red,
                _ => Color::Gray,
            };
            let clean_goal = sanitize_display_line(&row.goal);
            let goal_preview = if clean_goal.chars().count() > max_goal_chars {
                let head: String = clean_goal
                    .chars()
                    .take(max_goal_chars.saturating_sub(1))
                    .collect();
                format!("{head}…")
            } else {
                clean_goal
            };
            let base_style = if is_sel {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let state_style = if is_sel {
                base_style
            } else {
                Style::default().fg(state_color)
            };
            let dim_style = if is_sel {
                base_style
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} #{:<3} ", row.id.0), base_style),
                Span::styled(format!("{} ", row.name), base_style),
                Span::styled(format!("[{role}] "), dim_style),
                Span::styled(format!("({}) ", row.state), state_style),
                Span::styled(goal_preview, dim_style),
            ]));
        }
        for job in bg_jobs {
            let state_color = match job.status {
                "running" => Color::Cyan,
                "done" | "merged" => Color::Green,
                "failed" => Color::Red,
                _ => Color::Gray,
            };
            let clean_goal = sanitize_display_line(&job.goal);
            let goal_preview = if clean_goal.chars().count() > max_goal_chars {
                let head: String = clean_goal
                    .chars()
                    .take(max_goal_chars.saturating_sub(1))
                    .collect();
                format!("{head}…")
            } else {
                clean_goal
            };
            let mut spans = vec![
                Span::styled(
                    "  [bg] ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("#{} ", job.job_id),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("({}) ", job.status),
                    Style::default().fg(state_color),
                ),
            ];
            // `SDD-10` Fase 4: badge del driver (`→ opencode`); el driver nativo es el
            // default y no se muestra (ni se empuja Span vacío) para no ensuciar la línea.
            if job.driver != "native" {
                spans.push(Span::styled(
                    format!("→ {} ", job.driver),
                    Style::default().fg(Color::Magenta),
                ));
            }
            spans.push(Span::styled(
                format!("{} — ", job.branch_name),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(
                goal_preview,
                Style::default().fg(Color::DarkGray),
            ));
            lines.push(Line::from(spans));
        }
    }

    let block = Block::default()
        .title(" Select Agent (Up/Down · Enter focus · /merge /discard · Esc/F2 close) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let widget = Paragraph::new(lines).block(block);
    f.render_widget(widget, popup_area);
}

fn truncate_diff_lines(diff: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= max_lines {
        diff.to_string()
    } else {
        let mut head = lines[..max_lines].join("\n");
        head.push_str(&format!("\n... (+{} more lines)", lines.len() - max_lines));
        head
    }
}

/// Renders the parametric openOnyx emblem in Braille sub-pixels (`6x3` cells = `12x12` dots).
/// Rotates smoothly while `is_busy` is `true` and rests at `0.0` rad when idle or awaiting approval.
pub(crate) fn render_onyx_emblem(f: &mut Frame<'_>, area: Rect, angle_rad: f64, is_busy: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rot = |x: f64, y: f64| -> (f64, f64) {
        let (s, c) = angle_rad.sin_cos();
        (x * c - y * s, x * s + y * c)
    };

    let core_color = if is_busy {
        Color::Rgb(255, 248, 231)
    } else {
        Color::Rgb(180, 155, 118)
    };
    let arc_color = if is_busy {
        Color::Rgb(212, 180, 131)
    } else {
        Color::DarkGray
    };

    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([-1.15, 1.15])
        .y_bounds([-1.15, 1.15])
        .paint(|ctx| {
            // Opposing orbital C-arcs
            let arc_steps = 16;
            for base in [0.0_f64, PI] {
                for i in 0..arc_steps {
                    let t0 = base - 0.85 + (i as f64 / arc_steps as f64) * 1.7;
                    let t1 = base - 0.85 + ((i + 1) as f64 / arc_steps as f64) * 1.7;
                    let (x1, y1) = rot(0.92 * t0.cos(), 0.92 * t0.sin());
                    let (x2, y2) = rot(0.92 * t1.cos(), 0.92 * t1.sin());
                    ctx.draw(&CanvasLine {
                        x1,
                        y1,
                        x2,
                        y2,
                        color: arc_color,
                    });
                }
            }

            // Central 4-point concave astroid star
            let star_steps = 16;
            for i in 0..star_steps {
                let t0 = (i as f64 / star_steps as f64) * TAU;
                let t1 = ((i + 1) as f64 / star_steps as f64) * TAU;
                let (x1, y1) = rot(0.62 * t0.cos().powi(3), 0.78 * t0.sin().powi(3));
                let (x2, y2) = rot(0.62 * t1.cos().powi(3), 0.78 * t1.sin().powi(3));
                ctx.draw(&CanvasLine {
                    x1,
                    y1,
                    x2,
                    y2,
                    color: core_color,
                });
            }

            // Two bright orbital nodes
            ctx.draw(&Points {
                coords: &[rot(-0.62, 0.0), rot(0.62, 0.0)],
                color: Color::White,
            });
        });

    f.render_widget(canvas, area);
}

/// Styles each line of the `Services (Sprout)` panel by status (`REQ-08.5`, `TEST-08.5`).
pub(crate) fn render_services_lines(svc_text: &str) -> Vec<Line<'static>> {
    svc_text
        .lines()
        .map(|sline| {
            let trimmed = sline.trim_start();
            let style = if trimmed.starts_with("[READY]") {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if trimmed.starts_with("[STARTING]") || trimmed.starts_with("[PENDING]") {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if trimmed.starts_with("[FAILED]") {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else if trimmed.starts_with("[OFF]") {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Cyan)
            };
            Line::from(Span::styled(sline.to_string(), style))
        })
        .collect()
}

fn sanitize_display_line(raw: &str) -> String {
    raw.trim_end_matches('\r').replace('\t', "    ")
}

fn push_multiline_entry(out: &mut Vec<Line<'static>>, label: &str, color: Color, text: &str) {
    let prefix_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let mut lines = text.split('\n');
    let first = sanitize_display_line(lines.next().unwrap_or(""));
    out.push(Line::from(vec![
        Span::styled(format!("[{label}] "), prefix_style),
        Span::raw(first),
    ]));
    for line in lines {
        let clean = sanitize_display_line(line);
        if clean.trim().is_empty() {
            out.push(Line::from(""));
        } else {
            out.push(Line::from(Span::raw(format!("  {clean}"))));
        }
    }
}

fn prop_text(g: &dyn GraphView, id: ConceptId, attr: onyx_soul_agents::Attr) -> Option<String> {
    match g.get(id, attr) {
        Some(onyx_soul_agents::Value::Text(s)) => Some(s),
        _ => None,
    }
}

pub(crate) fn apply_scroll_key(
    scroll_from_bottom: &mut usize,
    viewport_height: usize,
    code: CtKeyCode,
    modifiers: KeyModifiers,
) -> bool {
    let page = viewport_height.max(5);
    match code {
        CtKeyCode::Up => {
            *scroll_from_bottom = scroll_from_bottom.saturating_add(3);
            true
        }
        CtKeyCode::Down => {
            *scroll_from_bottom = scroll_from_bottom.saturating_sub(3);
            true
        }
        CtKeyCode::PageUp => {
            *scroll_from_bottom = scroll_from_bottom.saturating_add(page);
            true
        }
        CtKeyCode::PageDown => {
            *scroll_from_bottom = scroll_from_bottom.saturating_sub(page);
            true
        }
        CtKeyCode::Home => {
            *scroll_from_bottom = usize::MAX / 2;
            true
        }
        CtKeyCode::End => {
            *scroll_from_bottom = 0;
            true
        }
        CtKeyCode::Char('u' | 'U') if modifiers.contains(KeyModifiers::CONTROL) => {
            *scroll_from_bottom = scroll_from_bottom.saturating_add(page);
            true
        }
        CtKeyCode::Char('d' | 'D') if modifiers.contains(KeyModifiers::CONTROL) => {
            *scroll_from_bottom = scroll_from_bottom.saturating_sub(page);
            true
        }
        _ => false,
    }
}

/// Computes the total visual row count occupied by `lines` when wrapped to `inner_width` columns,
/// matching `ratatui::widgets::reflow::WordWrapper` with `Wrap { trim: false }`.
pub(crate) fn visual_row_count(lines: &[Line<'_>], inner_width: u16) -> usize {
    if inner_width == 0 {
        return lines.len();
    }
    lines
        .iter()
        .map(|line| wrapped_line_rows(line, inner_width))
        .sum()
}

fn wrapped_line_rows(line: &Line<'_>, max_line_width: u16) -> usize {
    if max_line_width == 0 {
        return 0;
    }
    let mut wrapped_count = 0usize;
    let mut pending_line_empty = true;
    let mut line_width: u16 = 0;
    let mut word_width: u16 = 0;
    let mut whitespace_width: u16 = 0;
    let mut non_whitespace_previous = false;
    let mut pending_word_empty = true;
    let mut pending_whitespace: VecDeque<u16> = VecDeque::new();

    for grapheme in line.styled_graphemes(Style::default()) {
        let symbol = grapheme.symbol;
        let is_whitespace = symbol == "\u{200b}"
            || (symbol.chars().all(char::is_whitespace) && symbol != "\u{00a0}");
        let symbol_width = u16::try_from(Span::raw(symbol).width()).unwrap_or(u16::MAX);
        if symbol_width > max_line_width {
            continue;
        }

        let word_found = non_whitespace_previous && is_whitespace;
        let untrimmed_overflow = pending_line_empty
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(symbol_width)
                > max_line_width;

        if word_found || untrimmed_overflow {
            if !pending_whitespace.is_empty() || !pending_word_empty {
                pending_line_empty = false;
            }
            line_width = line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width);
            pending_whitespace.clear();
            whitespace_width = 0;
            pending_word_empty = true;
            word_width = 0;
        }

        let line_full = line_width >= max_line_width;
        let pending_word_overflow = symbol_width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= max_line_width;

        if line_full || pending_word_overflow {
            let mut remaining_width = max_line_width.saturating_sub(line_width);
            wrapped_count += 1;
            pending_line_empty = true;
            line_width = 0;

            while let Some(&w) = pending_whitespace.front() {
                if w > remaining_width {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(w);
                remaining_width = remaining_width.saturating_sub(w);
                pending_whitespace.pop_front();
            }

            if is_whitespace && pending_whitespace.is_empty() {
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(symbol_width);
            pending_whitespace.push_back(symbol_width);
        } else {
            word_width = word_width.saturating_add(symbol_width);
            pending_word_empty = false;
        }

        non_whitespace_previous = !is_whitespace;
    }

    if pending_line_empty && pending_word_empty && !pending_whitespace.is_empty() {
        wrapped_count += 1;
    }
    if !pending_whitespace.is_empty() || !pending_word_empty {
        pending_line_empty = false;
    }
    if !pending_line_empty {
        wrapped_count += 1;
    }
    if wrapped_count == 0 {
        wrapped_count = 1;
    }
    wrapped_count
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn header_title_displays_model_and_effort_compactly() {
        assert_eq!(
            format_header_title("BUILD", "workspace", "qwen38-27b", None),
            " openOnyx [BUILD] [SANDBOX:workspace] · qwen38-27b (effort:auto) "
        );
        assert_eq!(
            format_header_title(
                "PLAN",
                "strict",
                "gemini-4-hydrogen",
                Some(ReasoningEffort::High)
            ),
            " openOnyx [PLAN] [SANDBOX:strict] · gemini-4-hydrogen (effort:high) "
        );
        assert_eq!(
            format_header_title("BUILD", "off", "", None),
            " openOnyx [BUILD] [SANDBOX:off] "
        );
        assert_eq!(
            format_header_title_with_trust("BUILD", "env+job", true, "qwen38-27b", None),
            " openOnyx [BUILD] [SANDBOX:env+job] [UNTRUSTED] · qwen38-27b (effort:auto) "
        );
    }

    /// `SDD-03` Fase 3: la cabecera viva de la TUI sigue el objetivo LLM
    /// hot-swap (el handle opaca al campo estático de arranque, que solo es
    /// fallback sin handle). Regresión: el header "mentía" hasta reiniciar.
    #[test]
    fn live_llm_header_follows_hot_swapped_target() {
        use crate::llm_profile::{LlmConfig, LlmProfileHandle, LlmProvider};

        let startup = LlmConfig {
            provider: LlmProvider::Gemini,
            api_key: "startup-key".to_string(),
            model: "gemini-4-hydrogen".to_string(),
            effort: None,
            thinking_budget: None,
            api_base: None,
            project_instructions: String::new(),
        };
        let handle = LlmProfileHandle::new(&startup, &[], false);

        // Con handle presente opaca el campo estático (aunque esté pueblado).
        let (model, effort) = resolve_live_llm_header(
            Some(&handle),
            "stale-static-model",
            Some(ReasoningEffort::High),
        );
        assert_eq!(model, "gemini/gemini-4-hydrogen");
        assert_eq!(effort, None);

        // Hot-swap (lo que hace `apply_llm_target` en la frontera de tick):
        // la cabecera refleja el objetivo nuevo en el siguiente render.
        handle
            .set_model("gemini-4-deuterium")
            .expect("set_model sobre el perfil por defecto");
        handle.set_effort(Some(ReasoningEffort::High));
        let (model, effort) = resolve_live_llm_header(Some(&handle), "stale-static-model", None);
        assert_eq!(model, "gemini/gemini-4-deuterium");
        assert_eq!(effort, Some(ReasoningEffort::High));

        // Sin handle: se degrada al arranque estático (fallback).
        let (model, effort) =
            resolve_live_llm_header(None, "static-model", Some(ReasoningEffort::Low));
        assert_eq!(model, "static-model");
        assert_eq!(effort, Some(ReasoningEffort::Low));
    }

    #[test]
    fn focus_resolution_follows_new_root_and_ignores_subagents() {
        let mut last_root = None;
        let root1 = AgentRow {
            id: ConceptId(1),
            name: "openonyx-1".to_string(),
            is_root: true,
            state: "waiting_children".to_string(),
            goal: "audit repo".to_string(),
        };
        let (focused, changed) = resolve_focused_agent(&[root1.clone()], None, &mut last_root);
        assert_eq!(focused, Some(ConceptId(1)));
        assert!(changed);

        // Spawning a subagent must NOT hijack focus away from root1.
        let sub = AgentRow {
            id: ConceptId(2),
            name: "auditor".to_string(),
            is_root: false,
            state: "acting".to_string(),
            goal: "audit open-onyx".to_string(),
        };
        let (focused, changed) =
            resolve_focused_agent(&[root1.clone(), sub.clone()], focused, &mut last_root);
        assert_eq!(focused, Some(ConceptId(1)));
        assert!(!changed);

        // Explicit user switch to subagent stays sticky while no new root appears.
        let (focused, changed) = resolve_focused_agent(
            &[root1.clone(), sub.clone()],
            Some(ConceptId(2)),
            &mut last_root,
        );
        assert_eq!(focused, Some(ConceptId(2)));
        assert!(!changed);

        // Starting a fresh root agent via `/new` automatically moves focus to the new root.
        let root2 = AgentRow {
            id: ConceptId(3),
            name: "openonyx-2".to_string(),
            is_root: true,
            state: "thinking".to_string(),
            goal: "new task".to_string(),
        };
        let (focused, changed) =
            resolve_focused_agent(&[root1, sub, root2], focused, &mut last_root);
        assert_eq!(focused, Some(ConceptId(3)));
        assert!(changed);
    }

    #[test]
    fn agent_picker_popup_renders_agents_with_role_and_state() {
        let rows = vec![
            AgentRow {
                id: ConceptId(1),
                name: "openonyx-1".to_string(),
                is_root: true,
                state: "waiting_children".to_string(),
                goal: "audit repository".to_string(),
            },
            AgentRow {
                id: ConceptId(113),
                name: "auditor".to_string(),
                is_root: false,
                state: "acting".to_string(),
                goal: "inspect open-onyx".to_string(),
            },
        ];
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_agent_picker(f, f.area(), &rows, Some(ConceptId(1)), 1);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rendered: String = (0..20)
            .map(|y| (0..90).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Select Agent"), "{rendered}");
        assert!(rendered.contains("openonyx-1"), "{rendered}");
        assert!(rendered.contains("auditor"), "{rendered}");
        assert!(rendered.contains("[root]"), "{rendered}");
        assert!(rendered.contains("[sub ]"), "{rendered}");
    }

    #[test]
    fn agent_picker_popup_renders_background_worktree_subagents() {
        let rows = vec![AgentRow {
            id: ConceptId(1),
            name: "openonyx-1".to_string(),
            is_root: true,
            state: "thinking".to_string(),
            goal: "coordinate background tasks".to_string(),
        }];
        let bg_jobs = vec![crate::subagent::SubagentJobSummary {
            job_id: "bg-1".to_string(),
            driver: "native".to_string(),
            name: "worker".to_string(),
            goal: "refactor parser".to_string(),
            spawn_tick: 7,
            branch_name: "onyx/sub/bg-1".to_string(),
            worktree_path: std::path::PathBuf::from("wt/bg-1"),
            status: "running",
            ok: None,
            touched_files: vec!["src/lib.rs".to_string()],
            summary: String::new(),
        }];
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_agent_picker_with_subagents(
                    f,
                    f.area(),
                    &rows,
                    &bg_jobs,
                    Some(ConceptId(1)),
                    0,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rendered: String = (0..20)
            .map(|y| (0..90).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("[bg]"), "{rendered}");
        assert!(rendered.contains("#bg-1"), "{rendered}");
        assert!(rendered.contains("(running)"), "{rendered}");
        assert!(rendered.contains("onyx/sub/bg-1"), "{rendered}");
    }

    #[test]
    fn onyx_emblem_renders_braille_cells_in_idle_and_rotating_states() {
        let backend = TestBackend::new(6, 3);
        let mut terminal = Terminal::new(backend).unwrap();

        for (angle, busy) in [(0.0_f64, false), (0.66_f64, true)] {
            terminal
                .draw(|f| {
                    render_onyx_emblem(f, f.area(), angle, busy);
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let non_empty = (0..3)
                .flat_map(|y| (0..6).map(move |x| (x, y)))
                .filter(|&(x, y)| buf[(x, y)].symbol().trim() != "")
                .count();
            assert!(
                non_empty > 0,
                "expected Braille glyph cells for angle={angle}, busy={busy}"
            );
        }
    }

    #[test]
    fn visual_row_count_accounts_for_word_wrapping_and_scroll_keys() {
        let lines = vec![
            Line::from(""),
            Line::from("short"),
            Line::from("hello world"),
            Line::from("    hello"),
            Line::from("abcdefghijklmno"),
        ];
        // width = 5:
        // "" -> 1
        // "short" (5) -> 1
        // "hello world" -> "hello" (5) + wrap + "world" (5) -> 2
        // "    hello" (4 spaces + 5 chars) -> 2
        // "abcdefghijklmno" (15) -> 3
        assert_eq!(visual_row_count(&lines, 5), 9);

        let mut scroll = 0usize;
        assert!(apply_scroll_key(
            &mut scroll,
            10,
            CtKeyCode::Up,
            KeyModifiers::NONE
        ));
        assert_eq!(scroll, 3);
        assert!(apply_scroll_key(
            &mut scroll,
            10,
            CtKeyCode::PageUp,
            KeyModifiers::NONE
        ));
        assert_eq!(scroll, 13);
        assert!(apply_scroll_key(
            &mut scroll,
            10,
            CtKeyCode::Char('d'),
            KeyModifiers::CONTROL
        ));
        assert_eq!(scroll, 3);
        assert!(apply_scroll_key(
            &mut scroll,
            10,
            CtKeyCode::Down,
            KeyModifiers::NONE
        ));
        assert_eq!(scroll, 0);
        assert!(apply_scroll_key(
            &mut scroll,
            10,
            CtKeyCode::Home,
            KeyModifiers::NONE
        ));
        assert!(scroll > 100);
        assert!(apply_scroll_key(
            &mut scroll,
            10,
            CtKeyCode::End,
            KeyModifiers::NONE
        ));
        assert_eq!(scroll, 0);
    }

    #[test]
    fn visual_row_count_matches_ratatui_paragraph_rendering_to_the_last_line() {
        let mut log_lines: Vec<Line<'static>> = Vec::new();
        push_multiline_entry(
            &mut log_lines,
            "user",
            Color::LightCyan,
            "Inspect the workspace\r\n\r\nand explain everything step by step.",
        );
        push_multiline_entry(
            &mut log_lines,
            "call",
            Color::Yellow,
            r#"fs_read {"path":"very/long/nested/directory/structure/without/spaces/file_name_extra_long.rs","start_line":1,"end_line":120}"#,
        );
        push_multiline_entry(
            &mut log_lines,
            "observe",
            Color::Green,
            "fn alpha() {\n\tlet x = 1;\n\n\tlet beta_long_identifier_name_here = 2;\n    \n}",
        );
        push_multiline_entry(
            &mut log_lines,
            "result",
            Color::LightGreen,
            "Summary paragraph with multiple lines.\n\nSecond paragraph.\n\nExactBoundaryWord1234567890Tail\nPrevTail",
        );
        push_multiline_entry(
            &mut log_lines,
            "stream",
            Color::LightCyan,
            "Streaming line 1\n\nStreaming line 2\nTAIL_END_OK▍",
        );

        let extract_rows =
            |buf: &ratatui::buffer::Buffer, width: u16, height: u16| -> Vec<String> {
                (0..height)
                    .map(|y| {
                        let mut row = String::new();
                        for x in 0..width {
                            row.push_str(buf[(x, y)].symbol());
                        }
                        row.trim_end().to_string()
                    })
                    .collect()
            };

        for width in [26u16, 33u16, 42u16, 80u16] {
            let height = 10u16;
            let total_rows = visual_row_count(&log_lines, width);
            let viewport_height = usize::from(height);
            assert!(
                total_rows > viewport_height,
                "expected total_rows ({total_rows}) > viewport ({viewport_height}) for width {width}"
            );
            let max_scroll = total_rows.saturating_sub(viewport_height);

            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let render_lines = log_lines.clone();
            terminal
                .draw(|f| {
                    let p = Paragraph::new(render_lines)
                        .wrap(Wrap { trim: false })
                        .scroll((u16::try_from(max_scroll).unwrap_or(u16::MAX), 0));
                    f.render_widget(p, f.area());
                })
                .unwrap();

            let rows_text = extract_rows(terminal.backend().buffer(), width, height);
            let last_row = rows_text.last().expect("last row exists");
            assert_eq!(
                last_row, "  TAIL_END_OK▍",
                "width={width}: expected exact bottom row match, got rows: {rows_text:#?}"
            );

            // Prove there is zero undercounting or overcounting by scrolling 1 row further.
            let over_scroll = max_scroll + 1;
            let over_lines = log_lines.clone();
            terminal
                .draw(|f| {
                    let p = Paragraph::new(over_lines)
                        .wrap(Wrap { trim: false })
                        .scroll((u16::try_from(over_scroll).unwrap_or(u16::MAX), 0));
                    f.render_widget(p, f.area());
                })
                .unwrap();
            let over_rows = extract_rows(terminal.backend().buffer(), width, height);
            assert!(
                over_rows.last().expect("last row").is_empty(),
                "width={width}: scrolling by max_scroll + 1 should clear last row, got: {over_rows:#?}"
            );
        }
    }

    #[test]
    fn approval_banner_renders_scent_impact_line_with_diff_in_test_backend() {
        let impact = ImpactAssessment {
            max_heat_bp: 6_750,
            callers: 4,
            hops: 2,
            touches_public_src: true,
            hard_threshold_hit: false,
            escalates_allow_to_ask: true,
            epicenters: vec!["src/engine.rs::Engine::tick".to_string()],
            summary_line:
                "# impact: 4 callers, 2 hops, scope:src(pub) [heat 6750bp] (epicenters: Engine::tick)"
                    .to_string(),
        };
        let diff = "--- a/src/engine.rs\n+++ b/src/engine.rs\n@@ -1 +1 @@\n-old\n+new";
        let lines = build_approval_lines(
            "fs_edit",
            r#"{"path":"src/engine.rs"}"#,
            Some(diff),
            Some(&impact),
        );

        let backend = ratatui::backend::TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let block = Block::default()
                    .title(" Approval Required [agent #1 root] ([y] Approve / [n] Deny) ")
                    .borders(Borders::ALL);
                let widget = Paragraph::new(lines).block(block);
                f.render_widget(widget, f.area());
            })
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("[agent #1 root]"), "{rendered}");
        assert!(
            rendered.contains("# impact: 4 callers, 2 hops, scope:src(pub) [heat 6750bp]"),
            "{rendered}"
        );
        assert!(rendered.contains("+new"), "{rendered}");
    }

    // ============================================================
    // `SDD-07` Fase 3 — TEST-07.6: TimeFreeze en el TUI, determinista
    // sobre `ratatui::backend::TestBackend` (sin terminal real).
    // ============================================================

    fn tf_temp_dir(prefix: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("openonyx-tui-time-{prefix}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Contexto TEST-07.6: grafo con 1 efecto registrado (tick 4), ring con
    /// ticks `0..=5` (`present = 5`) y `TimeDebugState` opcionalmente scrubeado
    /// a `scrub_to` (si `Some`, queda congelado mirando el pasado).
    fn tf_ctx(
        scrub_to: Option<u64>,
    ) -> (
        TuiDriver,
        onyx_soul_agents::AgentGraph,
        crate::time_debug::TimeDebugState,
    ) {
        use onyx_soul_agents::{AttrRegistry, Graph as _};
        let mut graph = onyx_soul_agents::AgentGraph::new();
        let vocab = Vocab::resolve(&mut graph);
        let ws_vocab = onyx_workspace::WorkspaceVocab::resolve_with(|name| graph.attr(name));
        // Un efecto registrado tras el cursor (tick 4 > 3): es lo que se
        // revertiría al confirmar la bifurcación (`revert on branch: N file(s)`).
        let eff = graph.create();
        graph.set(eff, ws_vocab.effect_tick, onyx_soul_agents::Value::Int(4));
        graph.set(
            eff,
            ws_vocab.effect_tool,
            onyx_soul_agents::Value::Text("fs_write".into()),
        );
        graph.set(
            eff,
            ws_vocab.effect_target,
            onyx_soul_agents::Value::Text("out.txt".into()),
        );
        let td = crate::time_debug::TimeDebugState::new(None);
        for t in 0..6u64 {
            td.record_tick(t, &graph.export());
        }
        if let Some(tick) = scrub_to {
            td.scrub_to(tick).unwrap();
        }
        let driver = TuiDriver::new(vocab)
            .unwrap()
            .with_workspace_vocab(ws_vocab)
            .with_time_debug(td.clone());
        (driver, graph, td)
    }

    /// TEST-07.6: escribe un comando en el input y lo envía con `Enter`;
    /// si el popup de autocompletado slash quedó abierto, el primer `Enter`
    /// acepta el completado y el segundo envía (comportamiento estándar).
    fn tf_type_command(inner: &mut TuiInner, cmd: &str) {
        inner.input.clear();
        for c in cmd.chars() {
            inner.dispatch_key(CtKeyCode::Char(c), KeyModifiers::NONE);
        }
        if inner.popup.is_open() {
            inner.dispatch_key(CtKeyCode::Enter, KeyModifiers::NONE);
        }
        inner.dispatch_key(CtKeyCode::Enter, KeyModifiers::NONE);
    }

    fn tf_rendered(inner: &TuiInner) -> String {
        inner
            .terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// TEST-07.6: escribe `prefix` en el input, sincroniza el popup y
    /// devuelve las labels visibles (helper aparte: el `RefMut` del test no
    /// permite dividir borrows de campos de `TuiInner`).
    fn tf_slash_labels(inner: &mut TuiInner, prefix: &str) -> Vec<String> {
        inner.input.set_text(prefix);
        let input_snapshot = inner.input.clone();
        inner
            .popup
            .sync_from_buffer(&input_snapshot, &mut inner.file_index, None, None);
        let labels: Vec<String> = inner.popup.items.iter().map(|i| i.label.clone()).collect();
        inner.popup.close();
        labels
    }

    #[test]
    fn time_freeze_banner_and_input_lock_in_test_backend() {
        let (driver, graph, _td) = tf_ctx(Some(3));
        let mut inner = driver.inner.borrow_mut();

        // (1a) Banner ámbar + `tick 3 / present 5` + aviso de ficheros a
        // revertir (solo RAM, `effects_after_tick`).
        inner.render(&graph);
        let rendered = tf_rendered(&inner);
        assert!(rendered.contains("TIME FROZEN"), "{rendered}");
        assert!(rendered.contains("tick 3 / present 5"), "{rendered}");
        assert!(
            rendered.contains("revert on branch: 1 file(s)"),
            "{rendered}"
        );

        // (1b) Prompt normal bloqueado: no entra a `queued`, vuelve al input
        // y avisa en `status_hint`.
        for c in "hola".chars() {
            inner.dispatch_key(CtKeyCode::Char(c), KeyModifiers::NONE);
        }
        inner.dispatch_key(CtKeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(inner.input.as_str(), "hola");
        assert!(
            inner.queued.is_empty(),
            "frozen: el prompt normal no entra a queued"
        );
        assert!(
            inner
                .status_hint
                .as_deref()
                .is_some_and(|h| h.starts_with("Paused in TIME FROZEN")),
            "status_hint: {:?}",
            inner.status_hint
        );

        // (1c) `/…` y `!…` sí se encolan bajo freeze (vías de escape del
        // depurador).
        tf_type_command(&mut inner, "/go");
        tf_type_command(&mut inner, "/step 1");
        tf_type_command(&mut inner, "/graph inspect mem");
        tf_type_command(&mut inner, "!echo hi");
        let cmds: Vec<String> = inner
            .queued
            .iter()
            .filter_map(|ev| match ev {
                OnyxEvent::Ingress(IngressEvent::Command(c)) => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            cmds,
            vec![
                "/go".to_string(),
                "/step 1".to_string(),
                "/graph inspect mem".to_string(),
                "!echo hi".to_string(),
            ]
        );
    }

    #[test]
    fn time_freeze_keys_scrub_ctrl_t_and_safe_esc() {
        let (driver, _graph, td) = tf_ctx(None);
        let mut inner = driver.inner.borrow_mut();
        // En vivo: cursor == present == 5, sin congelación.
        assert!(!td.is_frozen());
        assert_eq!(td.cursor(), 5);
        assert_eq!(td.present(), 5);

        // Escribir "hola" y `Alt+Left`: texto intacto, el cursor retrocedió
        // y el scrub a pasado auto-congeló.
        for c in "hola".chars() {
            inner.dispatch_key(CtKeyCode::Char(c), KeyModifiers::NONE);
        }
        inner.dispatch_key(CtKeyCode::Left, KeyModifiers::ALT);
        assert_eq!(inner.input.as_str(), "hola", "Alt+← no toca el InputBuffer");
        assert!(td.is_frozen(), "scrub a pasado auto-congela");
        assert_eq!(td.cursor(), 4);
        assert_eq!(td.present(), 5);

        // `Ctrl+T` alterna: descongela (vuelve al presente) y congela de nuevo.
        inner.dispatch_key(CtKeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(!td.is_frozen());
        assert_eq!(td.cursor(), 5);
        inner.dispatch_key(CtKeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(td.is_frozen());

        // `Esc` congelado → `return_to_present` **sin cerrar** la app.
        inner.dispatch_key(CtKeyCode::Esc, KeyModifiers::NONE);
        assert!(!td.is_frozen());
        assert_eq!(td.cursor(), 5);
        assert!(
            !inner.quit.load(Ordering::Relaxed),
            "Esc congelado no cierra la aplicación"
        );

        // Segundo `Esc` ya en vivo → cierra.
        inner.dispatch_key(CtKeyCode::Esc, KeyModifiers::NONE);
        assert!(
            inner.quit.load(Ordering::Relaxed),
            "Esc en vivo cierra la aplicación"
        );
    }

    #[test]
    fn timeline_f5_lists_ram_pack_and_present_with_cursor() {
        use onyx_soul_agents::Graph as _;
        let dir = tf_temp_dir("timeline");
        let store = SharedPackStore::new(&dir, None);
        let mut graph = onyx_soul_agents::AgentGraph::new();
        let vocab = Vocab::resolve(&mut graph);
        let td = crate::time_debug::TimeDebugState::new(Some(store.clone()));
        for t in 0..6u64 {
            td.record_tick(t, &graph.export());
        }
        // Hito en disco en tick 2 (también en el anillo → etiqueta [pack:*]).
        store
            .save_milestone(2, "cmd:/step", &graph.export())
            .unwrap();
        td.scrub_to(3).unwrap();

        let driver = TuiDriver::new(vocab)
            .unwrap()
            .with_session_storage(None, Some(store.clone()))
            .with_time_debug(td.clone());
        let mut inner = driver.inner.borrow_mut();
        inner.switch_view(TuiView::Timeline);
        inner.render(&graph);
        let rendered = tf_rendered(&inner);
        assert!(rendered.contains("[pack:cmd:/step]"), "{rendered}");
        assert!(rendered.contains("[ram]"), "{rendered}");
        assert!(rendered.contains("(present)"), "{rendered}");
        assert!(rendered.contains("▶ cursor"), "{rendered}");

        // Down → tick 1; Enter → `scrub_to` + `F1 Chat` con preview congelado
        // (0 disco: el pack sigue con su único hito).
        inner.dispatch_key(CtKeyCode::Down, KeyModifiers::NONE);
        inner.dispatch_key(CtKeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(td.cursor(), 1);
        assert!(td.is_frozen());
        assert_eq!(inner.view, TuiView::Chat);
        assert!(
            inner
                .status_hint
                .as_deref()
                .is_some_and(|h| h.starts_with("Scrubbed to tick 1")),
            "status_hint: {:?}",
            inner.status_hint
        );
        assert_eq!(store.entries().len(), 1, "0 disco hasta confirmar");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn palette_time_group_and_slash_autocomplete_sdd07() {
        let (driver, _graph, _td) = tf_ctx(None);
        let mut inner = driver.inner.borrow_mut();

        // (4a) Grupo "Time" en la paleta `Ctrl+K` con los comandos exactos.
        inner.open_palette();
        let time_items: Vec<&crate::tui_palette::PaletteItem> = inner
            .palette
            .visible_items()
            .iter()
            .filter(|i| i.group == "Time")
            .copied()
            .collect();
        assert_eq!(time_items.len(), 5, "grupo Time con 5 ítems");
        let cmds: std::collections::BTreeMap<String, String> = time_items
            .iter()
            .map(|i| match i.action() {
                PaletteAction::Command(c) => (i.label.clone(), c.clone()),
                other => panic!("ítem Time no es Command: {:?} {other:?}", i.label),
            })
            .collect();
        assert_eq!(
            cmds.get("time toggle freeze (pause here)"),
            Some(&"/time".to_string())
        );
        assert_eq!(cmds.get("time step 1"), Some(&"/step 1".to_string()));
        assert_eq!(cmds.get("time step-llm"), Some(&"/step-llm".to_string()));
        assert_eq!(cmds.get("time go (resume)"), Some(&"/go".to_string()));
        assert_eq!(
            cmds.get("graph inspect mem"),
            Some(&"/graph inspect mem".to_string())
        );
        inner.palette.close();

        // (4b) Autocompletado `/` de los 7 slash del depurador.
        for (prefix, expected) in [
            ("/go", "/go"),
            ("/graph", "/graph"),
            ("/in", "/inject"),
            ("/rea", "/read"),
            ("/step", "/step"),
            ("/step-", "/step-llm"),
            ("/time", "/time"),
        ] {
            let labels = tf_slash_labels(&mut inner, prefix);
            assert!(
                labels.contains(&expected.to_string()),
                "{prefix} → esperaba {expected} en {labels:?}"
            );
        }
    }
}
