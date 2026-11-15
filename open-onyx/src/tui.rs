//! Adaptador TUI interactivo (`ratatui` + `crossterm`) para `openOnyx`.
//! Vive fuera de `onyx-core` y `onyx-soul-agents` para mantener los souls con 0 dependencias.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{self, Stdout};
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
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

struct TuiInner {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    vocab: Vocab,
    input: String,
    queued: Vec<OnyxEvent>,
    active_approval: Option<(ConceptId, u64, String, String)>,
    seen_approvals: BTreeSet<(u64, u64)>,
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
    /// redibujando la UI sin avanzar `host.ticks()`.
    pub fn pump_idle(&self, graph: &dyn GraphView) {
        let mut inner = self.inner.borrow_mut();
        if inner.poll_crossterm() && inner.queued.is_empty() && !inner.quit.load(Ordering::Relaxed)
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
                input: String::new(),
                queued: Vec::new(),
                active_approval: None,
                seen_approvals: BTreeSet::new(),
                quit: Arc::new(AtomicBool::new(false)),
            })),
        })
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
                } else if let Some((agent, req, _, _)) = self.active_approval.clone() {
                    match code {
                        CtKeyCode::Char('y') | CtKeyCode::Char('Y') => {
                            self.queued.push(Reply::approve(agent, req).to_event());
                            self.active_approval = None;
                        }
                        CtKeyCode::Char('n') | CtKeyCode::Char('N') => {
                            self.queued
                                .push(Reply::deny(agent, req, "denied by user in TUI").to_event());
                            self.active_approval = None;
                        }
                        _ => {}
                    }
                } else {
                    match code {
                        CtKeyCode::Char(c) => self.input.push(c),
                        CtKeyCode::Backspace => {
                            self.input.pop();
                        }
                        CtKeyCode::Enter => {
                            let cmd = self.input.trim().to_string();
                            self.input.clear();
                            if !cmd.is_empty() {
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
                    _ => Color::Cyan,
                };
                push_multiline_entry(&mut log_lines, &kind, color, &text);
            }

            if let Some(res) = prop_text(graph, *id, self.vocab.result) {
                if !res.is_empty() {
                    push_multiline_entry(&mut log_lines, "result", Color::LightGreen, &res);
                }
            }
        }

        let input_snapshot = self.input.clone();
        let approval_snapshot = self.active_approval.clone();

        let _ = self.terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(4),
                ])
                .split(f.area());

            let header = Paragraph::new(status_line).block(
                Block::default()
                    .title(" openOnyx — Deterministic Harness (Esc/Ctrl+C to quit) ")
                    .borders(Borders::ALL),
            );
            f.render_widget(header, chunks[0]);

            let scroll = log_lines.len().saturating_sub(chunks[1].height.saturating_sub(2) as usize);
            let body = Paragraph::new(log_lines)
                .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
                .wrap(Wrap { trim: false })
                .block(Block::default().title(" Memory & Trace ").borders(Borders::ALL));
            f.render_widget(body, chunks[1]);

            let (bottom_title, bottom_text, border_color) = match &approval_snapshot {
                Some((_, _, tool, args)) => (
                    " Approval Required ([y] Approve / [n] Deny) ",
                    format!("{tool}: {args}"),
                    Color::Yellow,
                ),
                None => (
                    " Goal / Command (Enter to spawn agent) ",
                    format!("> {input_snapshot}"),
                    Color::White,
                ),
            };
            let bottom = Paragraph::new(bottom_text)
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
