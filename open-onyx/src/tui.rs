//! Adaptador TUI interactivo (`ratatui` + `crossterm`) para `openOnyx`.
//! Vive fuera de `onyx-core` y `onyx-soul-agents` para mantener los souls con 0 dependencias.

use std::cell::RefCell;
use std::collections::BTreeSet;
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
    WorkspaceMode, WorkspaceModeHandle, WorkspaceVocab, graph_workspace_plan, preview_tool_diff,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::gemini::StreamPreviewHandle;

struct TuiInner {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    vocab: Vocab,
    ws_vocab: Option<WorkspaceVocab>,
    workspace_root: Option<PathBuf>,
    mode_handle: WorkspaceModeHandle,
    stream_preview: StreamPreviewHandle,
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
    /// o refresca deltas SSE en vuelo sin avanzar `host.ticks()`.
    pub fn pump_idle(&self, graph: &dyn GraphView) {
        let mut inner = self.inner.borrow_mut();
        let ui_event = inner.poll_crossterm();
        let stream_dirty = inner.stream_preview.take_dirty();
        if (ui_event || stream_dirty)
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
                mode_handle: WorkspaceModeHandle::default(),
                stream_preview: StreamPreviewHandle::default(),
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
            for a in &approvals {
                if self.seen_approvals.insert((a.agent.0, a.request)) {
                    if let Some(p) = &a.pending_approval {
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
        }

        let agents = graph.query(&[self.vocab.state]);
        let mut status_line = String::from("idle (enter a goal below)");
        let mut log_lines: Vec<Line<'static>> = Vec::new();

        for id in &agents {
            let name = prop_text(graph, *id, self.vocab.name).unwrap_or_default();
            let state = prop_text(graph, *id, self.vocab.state).unwrap_or_default();
            let goal = prop_text(graph, *id, self.vocab.goal).unwrap_or_default();
            status_line = format!("agent={} state={} goal={}", name, state, goal);

            for mem_id in graph.related(*id, self.vocab.rel_memory) {
                let kind = prop_text(graph, mem_id, self.vocab.mem_kind).unwrap_or_default();
                let text = prop_text(graph, mem_id, self.vocab.mem_text).unwrap_or_default();
                let color = match kind.as_str() {
                    "call" => Color::Yellow,
                    "observe" => Color::Green,
                    "tool_error" | "denied" => Color::Red,
                    "user" => Color::LightCyan,
                    "system" => Color::Magenta,
                    _ => Color::Cyan,
                };
                push_multiline_entry(&mut log_lines, &kind, color, &text);
            }

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
        let bottom_height = if approval_snapshot.is_some() && diff_snapshot.is_some() {
            16
        } else {
            4
        };

        let terminal = &mut self.terminal;
        let scroll_from_bottom = &mut self.scroll_from_bottom;
        let last_viewport_height = &mut self.last_viewport_height;
        let _ = terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(bottom_height),
                ])
                .split(f.area());

            let header = Paragraph::new(status_line)
                .block(Block::default().title(header_title).borders(Borders::ALL));
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
                        " [{mode_label}] Goal / Command (Enter to send, Tab Plan/Build, /undo, /new) "
                    ),
                    vec![Line::from(format!("> {input_snapshot}"))],
                    Color::White,
                ),
            };
            let bottom = Paragraph::new(bottom_lines)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(bottom_title)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border_color)),
                );
            f.render_widget(bottom, chunks[2]);
        });
    }
}

fn push_multiline_entry(out: &mut Vec<Line<'static>>, label: &str, color: Color, text: &str) {
    let prefix_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or("");
    out.push(Line::from(vec![
        Span::styled(format!("[{label}] "), prefix_style),
        Span::raw(first.to_string()),
    ]));
    for line in lines {
        out.push(Line::from(Span::raw(format!("  {line}"))));
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

/// Calcula el número total de filas visuales que ocupan `lines` al envolver texto a `inner_width` columnas.
pub(crate) fn visual_row_count(lines: &[Line<'_>], inner_width: u16) -> usize {
    let w = usize::from(inner_width);
    if w == 0 {
        return lines.len();
    }
    let mut rows = 0usize;
    for line in lines {
        let line_width = line.width();
        if line_width == 0 || line_width <= w {
            rows += 1;
            continue;
        }
        let mut flat = String::new();
        for span in &line.spans {
            flat.push_str(span.content.as_ref());
        }
        let mut current_rows = 1usize;
        let mut col = 0usize;
        let mut pending_space = 0usize;
        for word in flat.split(' ') {
            let word_w = Span::raw(word).width();
            if word_w == 0 {
                pending_space += 1;
                continue;
            }
            if col > 0 && col + pending_space + word_w > w {
                current_rows += 1;
                col = 0;
                pending_space = 0;
            }
            let start_col = col + pending_space;
            if start_col + word_w > w {
                let first_fit = w.saturating_sub(start_col);
                let remaining = word_w - first_fit;
                let extra = remaining.div_ceil(w);
                current_rows += extra;
                let mod_rem = remaining % w;
                col = if mod_rem == 0 { w } else { mod_rem };
            } else {
                col = start_col + word_w;
            }
            pending_space = 1;
        }
        rows += current_rows;
    }
    rows
}

#[cfg(test)]
mod tests {
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
}
