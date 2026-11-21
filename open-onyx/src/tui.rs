//! Interactive TUI adapter (`ratatui` + `crossterm`) for `openOnyx`.
//! Lives outside `onyx-core` and `onyx-soul-agents` to keep souls dependency-free.

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::f64::consts::{PI, TAU};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode as CtKeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use onyx_soul_agents::{
    ConceptId, EventBus, GraphView, IngressEvent, OnyxEvent, PlatformDriver, Reply, SurfaceBinding,
    Vocab, pending_approvals,
};
use onyx_workspace::{
    PermissionDecision, SproutSupervisor, WorkspaceMode, WorkspaceModeHandle, WorkspacePolicy,
    WorkspaceVocab, graph_workspace_plan, graph_workspace_services, preview_tool_diff,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
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
use crate::pack::SharedPackStore;
use crate::vault::session_display_label;

const SPINNER_FRAME_INTERVAL: Duration = Duration::from_millis(90);
const SPINNER_STEP_RAD: f64 = 0.35;

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
    terminal: Terminal<CrosstermBackend<Stdout>>,
    vocab: Vocab,
    ws_vocab: Option<WorkspaceVocab>,
    workspace_root: Option<PathBuf>,
    policy: Option<WorkspacePolicy>,
    sprout: Option<SproutSupervisor>,
    mode_handle: WorkspaceModeHandle,
    stream_preview: StreamPreviewHandle,
    telemetry: SessionTelemetryHandle,
    context_config: TieredContextConfig,
    model_name: String,
    reasoning_effort: Option<ReasoningEffort>,
    session_label: Option<String>,
    snapshot_store: Option<SharedPackStore>,
    seen_compact_count: usize,
    input: String,
    queued: Vec<OnyxEvent>,
    active_approval: Option<(ConceptId, u64, String, String)>,
    approval_diff: Option<String>,
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
    /// or refreshes in-flight SSE deltas, telemetry, and throttled emblem rotation without advancing `host.ticks()`.
    pub fn pump_idle(&self, graph: &dyn GraphView) {
        let mut inner = self.inner.borrow_mut();
        let ui_event = inner.poll_crossterm();
        let stream_dirty = inner.stream_preview.take_dirty();
        let telemetry_dirty = inner.telemetry.take_dirty();
        let busy = inner.is_any_agent_busy(graph);
        let spinner_dirty = inner.advance_spinner_if_due(busy);
        if (ui_event || stream_dirty || telemetry_dirty || spinner_dirty)
            && inner.queued.is_empty()
            && !inner.quit.load(Ordering::Relaxed)
        {
            inner.render(graph);
        }
    }
}

pub struct TuiDriver {
    inner: Rc<RefCell<TuiInner>>,
}

impl TuiDriver {
    pub fn new(vocab: Vocab) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            inner: Rc::new(RefCell::new(TuiInner {
                terminal,
                vocab,
                ws_vocab: None,
                workspace_root: None,
                policy: None,
                sprout: None,
                mode_handle: WorkspaceModeHandle::default(),
                stream_preview: StreamPreviewHandle::default(),
                telemetry: SessionTelemetryHandle::default(),
                context_config: TieredContextConfig::default(),
                model_name: String::new(),
                reasoning_effort: None,
                session_label: None,
                snapshot_store: None,
                seen_compact_count: 0,
                input: String::new(),
                queued: Vec::new(),
                active_approval: None,
                approval_diff: None,
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

    pub fn with_sprout(self, sprout: SproutSupervisor) -> Self {
        {
            let mut inner = self.inner.borrow_mut();
            inner.sprout = Some(sprout);
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
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
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
                let is_ctrl_c = modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(code, CtKeyCode::Char('c') | CtKeyCode::Char('C'));
                if is_ctrl_c {
                    self.quit.store(true, Ordering::Relaxed);
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
                if matches!(code, CtKeyCode::Esc) {
                    self.quit.store(true, Ordering::Relaxed);
                } else if matches!(code, CtKeyCode::F(2)) {
                    self.open_agent_picker();
                } else if apply_scroll_key(
                    &mut self.scroll_from_bottom,
                    self.last_viewport_height,
                    code,
                    modifiers,
                ) {
                    return true;
                } else if let Some((agent, req, _, _)) = self.active_approval.clone() {
                    match code {
                        CtKeyCode::Char('y') | CtKeyCode::Char('Y') => {
                            self.queued.push(Reply::approve(agent, req).to_event());
                            self.active_approval = None;
                            self.approval_diff = None;
                        }
                        CtKeyCode::Char('n') | CtKeyCode::Char('N') => {
                            self.queued
                                .push(Reply::deny(agent, req, "denied by user in TUI").to_event());
                            self.active_approval = None;
                            self.approval_diff = None;
                        }
                        _ => {}
                    }
                } else {
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
                        CtKeyCode::Char(c) => self.input.push(c),
                        CtKeyCode::Backspace => {
                            self.input.pop();
                        }
                        CtKeyCode::Enter => {
                            let cmd = self.input.trim().to_string();
                            self.input.clear();
                            if !cmd.is_empty() {
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
                                } else {
                                    self.queued
                                        .push(OnyxEvent::Ingress(IngressEvent::Command(cmd)));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                true
            }
            _ => false,
        }
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
                if let Some(policy) = &self.policy {
                    if policy.evaluate_permission_in_mode(current_mode, &p.tool, &p.args)
                        != PermissionDecision::Ask
                    {
                        // Auto-approved or auto-denied by `WorkspaceToolBridge` per policy.
                        continue;
                    }
                }
                if self.seen_approvals.insert((a.agent.0, a.request)) {
                    self.approval_diff = self
                        .workspace_root
                        .as_deref()
                        .and_then(|root| preview_tool_diff(root, &p.tool, &p.args, 14));
                    self.active_approval =
                        Some((a.agent, a.request, p.tool.clone(), p.args.clone()));
                    break;
                }
            }
        }

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

        let is_busy = self.active_approval.is_none() && any_agent_active;
        let spinner_angle = if is_busy { self.spinner_angle } else { 0.0 };

        let mut header_lines: Vec<Line<'static>> = Vec::with_capacity(3);
        let mut log_lines: Vec<Line<'static>> = Vec::new();
        let mut latest_agent_memory: Option<(String, Vec<(String, String)>)> = None;
        let mut compact_count = 0usize;
        let model_display = if self.model_name.is_empty() {
            self.telemetry.snapshot().model
        } else {
            self.model_name.clone()
        };
        let effort_display = self
            .reasoning_effort
            .map(ReasoningEffort::as_str)
            .unwrap_or("auto");

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
            if agent_rows.len() > 1 {
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
        let sandbox_badge = self
            .policy
            .as_ref()
            .map(|p| p.sandbox.mode.as_badge())
            .unwrap_or("workspace");
        let header_title = format_header_title(
            mode_label,
            sandbox_badge,
            &model_display,
            self.reasoning_effort,
        );
        let picker_open = self.agent_picker_open;
        let picker_idx = self.agent_picker_idx;
        let terminal = &mut self.terminal;
        let scroll_from_bottom = &mut self.scroll_from_bottom;
        let last_viewport_height = &mut self.last_viewport_height;
        let _ = terminal.draw(|f| {
            let is_approval = approval_snapshot.is_some();
            let (bottom_title, bottom_lines, border_color) = match &approval_snapshot {
                Some((_, _, tool, args)) => {
                    let args_preview = if diff_snapshot.is_some() && args.len() > 100 {
                        let mut cut = 100;
                        while !args.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        format!("{}...", &args[..cut])
                    } else {
                        args.clone()
                    };
                    let mut lines = vec![Line::from(vec![
                        Span::styled(
                            format!("{tool}: "),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(args_preview),
                    ])];
                    if let Some(diff) = &diff_snapshot {
                        for dline in diff.lines() {
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
                    let agent_tag = approval_agent_label
                        .as_deref()
                        .map(|lbl| format!(" [agent {lbl}]"))
                        .unwrap_or_default();
                    (
                        format!(" Approval Required{agent_tag} ([y] Approve / [n] Deny) "),
                        lines,
                        Color::Yellow,
                    )
                }
                None => (
                    format!(
                        " [{mode_label}] Goal / Command (Enter send · Tab Plan/Build · F2 Agents · /rewind · /fork · /undo · /compact · /services · /new) "
                    ),
                    vec![Line::from(format!("> {input_snapshot}"))],
                    Color::White,
                ),
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
                .title_top(
                    Line::from(Span::styled(
                        " F2:Agents · Esc:Quit ",
                        Style::default().fg(Color::DarkGray),
                    ))
                    .right_aligned(),
                )
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

            let has_side_panel = plan_snapshot.is_some() || services_snapshot.is_some();
            let (trace_area, side_area) = if has_side_panel && f.area().width >= 100 {
                let hsplit = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
                    .split(chunks[1]);
                (hsplit[0], Some(hsplit[1]))
            } else {
                (chunks[1], None)
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
                .block(Block::default().title(trace_title).borders(Borders::ALL));
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

            if picker_open {
                render_agent_picker(f, f.area(), &agent_rows, active_agent, picker_idx);
            }
        });
    }
}

/// Formats the non-invasive top header title showing mode, sandbox, model, and reasoning effort.
pub(crate) fn format_header_title(
    mode_label: &str,
    sandbox_badge: &str,
    model_name: &str,
    effort: Option<ReasoningEffort>,
) -> String {
    let model = model_name.trim();
    if model.is_empty() {
        format!(" openOnyx [{mode_label}] [SANDBOX:{sandbox_badge}] ")
    } else {
        let effort_label = effort.map(ReasoningEffort::as_str).unwrap_or("auto");
        format!(
            " openOnyx [{mode_label}] [SANDBOX:{sandbox_badge}] · {model} (effort:{effort_label}) "
        )
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

/// Renders the centered ephemeral Agent Selector popup (`F2` / `/agent`).
pub(crate) fn render_agent_picker(
    f: &mut Frame<'_>,
    area: Rect,
    rows: &[AgentRow],
    focused: Option<ConceptId>,
    selected_idx: usize,
) {
    if area.width < 20 || area.height < 6 {
        return;
    }
    let popup_w = area.width.saturating_sub(6).clamp(32, 82).min(area.width);
    let desired_h = u16::try_from(rows.len().saturating_add(4)).unwrap_or(14);
    let popup_h = desired_h
        .clamp(6, 16)
        .min(area.height.saturating_sub(2).max(5));
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup_area = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup_area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() {
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
    }

    let block = Block::default()
        .title(" Select Agent (Up/Down · Enter focus · Esc/F2 close) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let widget = Paragraph::new(lines).block(block);
    f.render_widget(widget, popup_area);
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
}
