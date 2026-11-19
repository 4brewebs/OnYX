//! Adaptador TUI interactivo (`ratatui` + `crossterm`) para `openOnyx`.
//! Vive fuera de `onyx-core` y `onyx-soul-agents` para mantener los souls con 0 dependencias.

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
    PermissionDecision, WorkspaceMode, WorkspaceModeHandle, WorkspacePolicy, WorkspaceVocab,
    graph_workspace_plan, preview_tool_diff,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::context::{
    COMPACT_MARKER, SessionTelemetryHandle, TieredContextConfig, ToolOutputTier, project_memory,
};
use crate::gemini::StreamPreviewHandle;

struct TuiInner {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    vocab: Vocab,
    ws_vocab: Option<WorkspaceVocab>,
    workspace_root: Option<PathBuf>,
    policy: Option<WorkspacePolicy>,
    mode_handle: WorkspaceModeHandle,
    stream_preview: StreamPreviewHandle,
    telemetry: SessionTelemetryHandle,
    context_config: TieredContextConfig,
    seen_compact_count: usize,
    input: String,
    queued: Vec<OnyxEvent>,
    active_approval: Option<(ConceptId, u64, String, String)>,
    approval_diff: Option<String>,
    seen_approvals: BTreeSet<(u64, u64)>,
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

    /// Lee entrada de teclado/resize cuando el cerebro está en reposo (`is_settled()`),
    /// o refresca deltas SSE / telemetría en vuelo sin avanzar `host.ticks()`.
    pub fn pump_idle(&self, graph: &dyn GraphView) {
        let mut inner = self.inner.borrow_mut();
        let ui_event = inner.poll_crossterm();
        let stream_dirty = inner.stream_preview.take_dirty();
        let telemetry_dirty = inner.telemetry.take_dirty();
        if (ui_event || stream_dirty || telemetry_dirty)
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
                mode_handle: WorkspaceModeHandle::default(),
                stream_preview: StreamPreviewHandle::default(),
                telemetry: SessionTelemetryHandle::default(),
                context_config: TieredContextConfig::default(),
                seen_compact_count: 0,
                input: String::new(),
                queued: Vec::new(),
                active_approval: None,
                approval_diff: None,
                seen_approvals: BTreeSet::new(),
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
    fn shutdown_inner(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// Devuelve `true` si hubo algún evento de UI o cerebro.
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
                if is_ctrl_c || matches!(code, CtKeyCode::Esc) {
                    self.quit.store(true, Ordering::Relaxed);
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
                                self.queued
                                    .push(OnyxEvent::Ingress(IngressEvent::Command(cmd)));
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
        self.inner.borrow_mut().render(graph);
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
                        // Auto-aprobado o auto-denegado por `WorkspaceToolBridge` según política.
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
        let mut status_line = String::from("idle (enter a goal below)");
        let mut log_lines: Vec<Line<'static>> = Vec::new();
        let mut latest_agent_memory: Option<(String, Vec<(String, String)>)> = None;
        let mut compact_count = 0usize;

        for id in &agents {
            let name = prop_text(graph, *id, self.vocab.name).unwrap_or_default();
            let state = prop_text(graph, *id, self.vocab.state).unwrap_or_default();
            let goal = prop_text(graph, *id, self.vocab.goal).unwrap_or_default();
            status_line = format!("agent={} state={} goal={}", name, state, goal);
            if !goal.is_empty() {
                push_multiline_entry(&mut log_lines, "user", Color::LightCyan, &goal);
            }

            let mut mem_pairs: Vec<(String, String)> = Vec::new();
            for mem_id in graph.related(*id, self.vocab.rel_memory) {
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

            if let Some(res) = prop_text(graph, *id, self.vocab.result) {
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
        let diff_snapshot = self.approval_diff.clone();
        let plan_snapshot = self
            .ws_vocab
            .as_ref()
            .and_then(|wv| graph_workspace_plan(graph, wv));
        let mode_label = self.mode_handle.get().as_label();
        let header_title =
            format!(" openOnyx [{mode_label}] — Deterministic Harness (Esc/Ctrl+C to quit) ");
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
                    (
                        " Approval Required ([y] Approve / [n] Deny) ".to_string(),
                        lines,
                        Color::Yellow,
                    )
                }
                None => (
                    format!(
                        " [{mode_label}] Goal / Command (Enter send, Tab Plan/Build, /undo, /compact, /new) "
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
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(bottom_height),
                ])
                .split(f.area());

            let header = Paragraph::new(status_line).block(
                Block::default()
                    .title(header_title)
                    .title_bottom(
                        Line::from(Span::styled(
                            format!(" {telemetry_bar} "),
                            Style::default().fg(Color::Cyan),
                        ))
                        .right_aligned(),
                    )
                    .borders(Borders::ALL),
            );
            f.render_widget(header, chunks[0]);

            let (trace_area, plan_area) = match plan_snapshot.as_deref() {
                Some(_) if f.area().width >= 100 => {
                    let hsplit = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
                        .split(chunks[1]);
                    (hsplit[0], Some(hsplit[1]))
                }
                _ => (chunks[1], None),
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
        });
    }
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

/// Calcula el número total de filas visuales que ocupan `lines` al envolver texto a `inner_width` columnas,
/// replicando exactamente la máquina de estados de `ratatui::widgets::reflow::WordWrapper` con `Wrap { trim: false }`.
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
