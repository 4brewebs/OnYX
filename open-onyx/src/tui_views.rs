//! `SDD-03` Fase 3 (`REQ-03.6`): pantallas TUI modulares (`F1..F5`).
//!
//! Lógica **pura** (testable sin terminal) y renderizadores de las cinco vistas
//! efímeras — `Chat`, `Agents`, `Sprout`, `Plan & Diff` y `Timeline` — navegables
//! con `F1..F5`, `/view <nombre>` o la paleta `Ctrl+K`. La navegación es 100 %
//! efímera: no toca el grafo, el bucle determinista ni el formato `ONYXTR2`.

use crossterm::event::KeyCode;
use onyx_soul_agents::ConceptId;
use onyx_workspace::WorkspaceMode;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::pack::SnapshotMeta;
use crate::subagent::SubagentJobSummary;
use crate::tui::AgentRow;

/// Vista efímera activa de la TUI (default `Chat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiView {
    /// Vista principal: traza, paneles laterales, input y aprobaciones.
    Chat,
    /// Lista de agentes del grafo + subagentes asíncronos `[bg]`.
    Agents,
    /// Servicios `sprout.toml` (lista/DAG) + logs en vivo.
    Sprout,
    /// Plan activo (`ws.plan.text`) + ledger de efectos Git.
    PlanDiff,
    /// Hitos de `SharedPackStore` (`/rewind` / `/fork`).
    Timeline,
}

impl TuiView {
    /// Orden estable `F1..F5` (tira de pestañas y `Ctrl+K`).
    pub const ALL: [TuiView; 5] = [
        TuiView::Chat,
        TuiView::Agents,
        TuiView::Sprout,
        TuiView::PlanDiff,
        TuiView::Timeline,
    ];

    /// Mapeo de tecla funcional `F1..F5` → vista (`None` para el resto).
    pub fn from_key(n: u8) -> Option<TuiView> {
        Some(match n {
            1 => TuiView::Chat,
            2 => TuiView::Agents,
            3 => TuiView::Sprout,
            4 => TuiView::PlanDiff,
            5 => TuiView::Timeline,
            _ => return None,
        })
    }

    /// Mapeo del argumento de `/view <chat|agents|sprout|plan|timeline>`
    /// (case-insensitive).
    pub fn from_slash(arg: &str) -> Option<TuiView> {
        Some(match arg.trim().to_ascii_lowercase().as_str() {
            "chat" => TuiView::Chat,
            "agents" => TuiView::Agents,
            "sprout" => TuiView::Sprout,
            "plan" => TuiView::PlanDiff,
            "timeline" => TuiView::Timeline,
            _ => return None,
        })
    }

    /// Nombre visible en la tira de pestañas.
    pub fn as_str(self) -> &'static str {
        match self {
            TuiView::Chat => "Chat",
            TuiView::Agents => "Agents",
            TuiView::Sprout => "Sprout",
            TuiView::PlanDiff => "Plan & Diff",
            TuiView::Timeline => "Timeline",
        }
    }

    /// Nombre del argumento de `/view <...>` correspondiente.
    pub fn as_slash(self) -> &'static str {
        match self {
            TuiView::Chat => "chat",
            TuiView::Agents => "agents",
            TuiView::Sprout => "sprout",
            TuiView::PlanDiff => "plan",
            TuiView::Timeline => "timeline",
        }
    }

    /// Atajo de teclado (`F1`..`F5`).
    pub fn shortcut(self) -> &'static str {
        match self {
            TuiView::Chat => "F1",
            TuiView::Agents => "F2",
            TuiView::Sprout => "F3",
            TuiView::PlanDiff => "F4",
            TuiView::Timeline => "F5",
        }
    }
}

/// Resultado puro de interpretar un comando `/view` tecleado en `Enter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewCommandOutcome {
    /// `/view` sin argumento: mostrar usage.
    Usage,
    /// `/view <nombre>` válido: cambiar de vista.
    Switch(TuiView),
    /// `/view <nombre>` desconocido: aviso transitorio.
    Unknown(String),
}

/// Interpreta `/view <chat|agents|sprout|plan|timeline>` (case-insensitive,
/// mismo patrón ASCII-safe de `/spawn`). Devuelve `None` si `cmd` no es un
/// comando `/view` (se procesa como input normal).
pub fn parse_view_command(cmd: &str) -> Option<ViewCommandOutcome> {
    let lower = cmd.trim().to_ascii_lowercase();
    if lower == "/view" {
        return Some(ViewCommandOutcome::Usage);
    }
    let rest = lower.strip_prefix("/view ")?;
    let arg = rest.trim();
    if arg.is_empty() {
        return Some(ViewCommandOutcome::Usage);
    }
    match TuiView::from_slash(arg) {
        Some(view) => Some(ViewCommandOutcome::Switch(view)),
        None => Some(ViewCommandOutcome::Unknown(arg.to_string())),
    }
}

/// Tira compacta de pestañas de la cabecera:
/// `F1 Chat · F2 Agents · F3 Sprout · F4 Plan · F5 Timeline`.
///
/// Con una aprobación pendiente **fuera** de `Chat` añade el badge
/// `[F1 Chat !APPROVAL]` (rojo/ámbar): la aprobación a ciegas está prohibida.
pub fn view_tabs_line(active: TuiView, approval_pending: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, view) in TuiView::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" · "));
        }
        let label = format!("{} {}", view.shortcut(), view.as_str());
        let style = if *view == active {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));
    }
    if approval_pending && active != TuiView::Chat {
        spans.push(Span::styled(
            "  [F1 Chat !APPROVAL]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    } else {
        // `SDD-03` Fase 4: la etiqueta de Esc refleja lo que hace — salir de
        // la app solo en `Chat`; en las vistas efímeras vuelve a `Chat`.
        let esc_label = if active == TuiView::Chat {
            "Esc:Quit"
        } else {
            "Esc:Back"
        };
        spans.push(Span::styled(
            format!("  ·  {esc_label}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKeyAction {
    Approve,
    Deny,
}

/// Seguridad cross-pantalla (`REQ-03.6`, OBLIGATORIO): `y`/`n` solo se mapean
/// a aprobar/denegar estando en `TuiView::Chat`. En cualquier otra vista **no**
/// se emite `Reply::approve`/`Reply::deny` (prohibido aprobar a ciegas sin ver
/// el diff ni el `# impact:`).
pub fn approval_key_action(view: TuiView, code: KeyCode) -> Option<ApprovalKeyAction> {
    if view != TuiView::Chat {
        return None;
    }
    Some(match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => ApprovalKeyAction::Approve,
        KeyCode::Char('n') | KeyCode::Char('N') => ApprovalKeyAction::Deny,
        _ => return None,
    })
}

/// Gate obligatorio para mutaciones Sprout desde `F3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SproutGate {
    Allowed,
    /// Modo `PLAN`/`EXPLORE` (solo lectura): sin mutaciones.
    BlockedMode(&'static str),
    /// Workspace Trust no totalmente confiado (`[UNTRUSTED]`).
    BlockedUntrusted,
}

/// Decide si `F3 Sprout` puede lanzar acciones mutantes (`s`/`x`/`r`/`a`).
/// Sin `Allowed` la TUI muestra un aviso y **nunca** muta.
pub fn sprout_mutation_gate(mode: WorkspaceMode, untrusted: bool) -> SproutGate {
    if mode != WorkspaceMode::Build {
        SproutGate::BlockedMode(mode.as_label())
    } else if untrusted {
        SproutGate::BlockedUntrusted
    } else {
        SproutGate::Allowed
    }
}

impl SproutGate {
    /// Aviso de bloqueo transitorio (vacio si `Allowed`).
    pub fn notice(self) -> String {
        match self {
            Self::Allowed => String::new(),
            Self::BlockedMode(label) => {
                format!("Sprout mutation blocked in `{label}` mode — switch to BUILD (Tab) first.")
            }
            Self::BlockedUntrusted => {
                "Sprout mutation blocked: workspace [UNTRUSTED] — run `/trust` first.".to_string()
            }
        }
    }
}

/// Fila de servicio parseada de la salida de `format_services_panel_from_toon`
/// (líneas `[BADGE] id (kind :port)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SproutServiceRow {
    pub badge: String,
    pub id: String,
    pub detail: String,
}

/// Parsea las líneas del panel de servicios Sprout en filas accionables.
/// Las líneas que no encajan (p. ej. `[UNTRUSTED — run /trust]`) se omiten.
pub fn parse_sprout_panel_lines(text: &str) -> Vec<SproutServiceRow> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((badge, rest)) = rest.split_once("] ") else {
            continue;
        };
        let Some((id, detail)) = rest.rsplit_once(" (") else {
            continue;
        };
        let Some(detail) = detail.strip_suffix(')') else {
            continue;
        };
        if badge.is_empty() || id.is_empty() || detail.is_empty() {
            continue;
        }
        out.push(SproutServiceRow {
            badge: badge.to_string(),
            id: id.to_string(),
            detail: detail.to_string(),
        });
    }
    out
}

/// Color del badge de estado de un servicio.
pub fn sprout_badge_color(badge: &str) -> Color {
    let upper = badge.to_ascii_uppercase();
    if upper.contains("READY") {
        Color::Green
    } else if upper.contains("STOP") || upper.is_empty() {
        Color::Gray
    } else if upper.contains("WAIT") || upper.contains("START") {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Fila de efecto Git del ledger para la vista `F4 Plan & Diff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRow {
    pub tick: u64,
    pub tool: String,
    pub target: String,
    pub reversibility: String,
    pub undone: bool,
    pub symbols: Vec<String>,
}

fn render_agents_lines(
    rows: &[AgentRow],
    bg_jobs: &[SubagentJobSummary],
    focused: Option<ConceptId>,
    selected: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() && bg_jobs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no agents yet — enter a goal in F1 Chat to start)",
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }
    let max_goal_chars = width.saturating_sub(40).max(12);
    for (idx, row) in rows.iter().enumerate() {
        let is_sel = idx == selected;
        let is_cur = Some(row.id) == focused;
        let marker = if is_cur { "*" } else { " " };
        let role = if row.is_root { "root" } else { "sub " };
        let clean_goal = sanitize_for_cell(&row.goal);
        let goal_preview = truncate_cells(&clean_goal, max_goal_chars);
        let style = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let state_color = match row.state.as_str() {
            "thinking" | "acting" | "waiting_children" => Color::Cyan,
            "waiting_approval" => Color::Yellow,
            "done" => Color::Green,
            "fail" => Color::Red,
            _ => Color::Gray,
        };
        let state_style = if is_sel {
            style
        } else {
            Style::default().fg(state_color)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} #{:<3} ", row.id.0), style),
            Span::styled(format!("{:<20} ", row.name), style),
            Span::styled(format!("[{role}] "), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("({}) ", row.state), state_style),
            Span::styled(goal_preview, Style::default().fg(Color::DarkGray)),
        ]));
    }
    for (job_off, job) in bg_jobs.iter().enumerate() {
        let idx = rows.len() + job_off;
        let is_sel = idx == selected;
        let style = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let state_color = match job.status {
            "running" => Color::Cyan,
            "done" | "merged" => Color::Green,
            "failed" => Color::Red,
            _ => Color::Gray,
        };
        let state_style = if is_sel {
            style
        } else {
            Style::default().fg(state_color)
        };
        let clean_goal = sanitize_for_cell(&job.goal);
        let goal_preview = truncate_cells(&clean_goal, max_goal_chars);
        let mut spans = vec![
            Span::styled(
                "  [bg] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("#{} ", job.job_id), style),
            Span::styled(format!("({}) ", job.status), state_style),
        ];
        // Badge del driver (mismo patrón que el agent picker).
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
    lines
}

fn sanitize_for_cell(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| *c >= ' ' || *c == '\t')
        .collect()
}

fn truncate_cells(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Renderiza `F2 Agents` a pantalla completa: agentes del grafo + jobs `[bg]`
/// (con badge `→ <driver>`). `Up`/`Down` seleccionan, `Enter` enfoca y vuelve a
/// `F1 Chat`, `Esc` vuelve a `F1 Chat`.
pub fn render_agents_view(
    f: &mut Frame<'_>,
    area: Rect,
    rows: &[AgentRow],
    bg_jobs: &[SubagentJobSummary],
    focused: Option<ConceptId>,
    selected: usize,
) {
    if area.width < 30 || area.height < 4 {
        return;
    }
    let inner_width = usize::from(area.width.saturating_sub(2)).saturating_sub(2);
    let lines = render_agents_lines(rows, bg_jobs, focused, selected, inner_width);
    let widget = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(" Agents (F2) — Up/Down select · Enter focus → F1 Chat · Esc back ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(widget, area);
}

/// Renderiza `F3 Sprout`: lista/DAG de servicios a la izquierda (selección con
/// `Up`/`Down`/`PgUp`/`PgDn`) y logs en vivo del servicio seleccionado a la
/// derecha (`follow` = anclado al final). El gate bloquea las mutantes `s`/`x`/
/// `r`/`a` con aviso.
pub fn render_sprout_view(
    f: &mut Frame<'_>,
    area: Rect,
    rows: &[SproutServiceRow],
    selected: usize,
    follow: bool,
    logs: &str,
    gate: SproutGate,
) {
    if area.width < 46 || area.height < 4 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " F3 Sprout — terminal too small ",
                Style::default().fg(Color::DarkGray),
            )))
            .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    // Izquierda: lista de servicios.
    let mut svc_lines: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() {
        svc_lines.push(Line::from(Span::styled(
            "  (no services — create a `sprout.toml` in the workspace)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (idx, row) in rows.iter().enumerate() {
        let is_sel = idx == selected;
        let marker = if is_sel { "▸" } else { " " };
        let badge_style = if is_sel {
            Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(sprout_badge_color(&row.badge))
        };
        let text_style = if is_sel {
            Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        svc_lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), text_style),
            Span::styled(format!("[{}]", row.badge), badge_style),
            Span::styled(format!(" {} ", row.id), text_style),
            Span::styled(row.detail.clone(), Style::default().fg(Color::DarkGray)),
        ]));
    }
    let gate_tag = if matches!(gate, SproutGate::Allowed) {
        " s start · x stop · r restart · a all · f follow "
    } else {
        " MUTATIONS BLOCKED — see notice "
    };
    let left = Paragraph::new(svc_lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(format!(" Services (F3){gate_tag} "))
            .borders(Borders::ALL)
            .border_style(if matches!(gate, SproutGate::Allowed) {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Yellow)
            }),
    );
    f.render_widget(left, chunks[0]);

    // Derecha: logs del servicio seleccionado.
    let log_lines: Vec<Line<'static>> = if logs.is_empty() {
        vec![Line::from(Span::styled(
            "  (no logs)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        logs.lines()
            .map(|l| {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(Color::Gray),
                ))
            })
            .collect()
    };
    let viewport = usize::from(chunks[1].height).saturating_sub(4).max(1);
    let total = log_lines.len();
    let scroll_top = if follow {
        total.saturating_sub(viewport)
    } else {
        0
    };
    let right = Paragraph::new(log_lines)
        .scroll((u16::try_from(scroll_top).unwrap_or(u16::MAX), 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(if follow {
                    " Logs [follow] "
                } else {
                    " Logs (f = follow) "
                })
                .borders(Borders::ALL),
        );
    f.render_widget(right, chunks[1]);
}

/// Renderiza `F4 Plan & Diff`: plan activo arriba y ledger de efectos Git
/// (tick, tool, target, reversibility, undone, symbols) abajo.
pub fn render_plan_diff_view(
    f: &mut Frame<'_>,
    area: Rect,
    plan: Option<&str>,
    effects: &[EffectRow],
    scroll: usize,
) {
    if area.width < 40 || area.height < 4 {
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let mut plan_lines: Vec<Line<'static>> = Vec::new();
    match plan {
        Some(text) => {
            for pline in text.lines() {
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
        }
        None => plan_lines.push(Line::from(Span::styled(
            "  (no active plan — set one in F1 Chat)",
            Style::default().fg(Color::DarkGray),
        ))),
    }
    f.render_widget(
        Paragraph::new(plan_lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Plan (ws.plan.text) ")
                .borders(Borders::ALL),
        ),
        chunks[0],
    );

    let mut eff_lines: Vec<Line<'static>> = Vec::new();
    if effects.is_empty() {
        eff_lines.push(Line::from(Span::styled(
            "  (no recorded Git effects)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for eff in effects {
        let mut spans = vec![
            Span::styled(
                format!("#t{:<4} ", eff.tick),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{:<14} ", eff.tool),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("→ {} ", eff.target),
                Style::default().fg(Color::White),
            ),
        ];
        let rev_color = match eff.reversibility.as_str() {
            "auto_reversible" => Color::Green,
            "compensable" => Color::Yellow,
            _ => Color::Red,
        };
        spans.push(Span::styled(
            format!("[{}]", eff.reversibility),
            Style::default().fg(rev_color),
        ));
        if !eff.symbols.is_empty() {
            let symbols = eff.symbols.join(", ");
            spans.push(Span::styled(
                format!("  {symbols}"),
                Style::default().fg(Color::Magenta),
            ));
        }
        if eff.undone {
            spans.push(Span::styled(
                "  (undone)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        eff_lines.push(Line::from(spans));
    }
    let widget = Paragraph::new(eff_lines)
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
        .block(
            Block::default()
                .title(" Recorded Effects (Git ledger) — Up/Down scroll ")
                .borders(Borders::ALL),
        );
    f.render_widget(widget, chunks[1]);
}

/// Renderiza `F5 Timeline`: hitos de `SharedPackStore` (`/rewind` / `/fork`).
pub fn render_timeline_view(
    f: &mut Frame<'_>,
    area: Rect,
    entries: &[SnapshotMeta],
    selected: usize,
    latest_tick: Option<u64>,
) {
    if area.width < 40 || area.height < 4 {
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no snapshots yet — ticks save as the session progresses)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (idx, meta) in entries.iter().enumerate() {
        let is_sel = idx == selected;
        let is_latest = Some(meta.tick) == latest_tick;
        let marker = if is_sel { "▸" } else { " " };
        let star = if is_latest { " * current" } else { "" };
        let base = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), base),
            Span::styled(
                format!("#t{:<6} ", meta.tick),
                if is_sel {
                    base
                } else {
                    Style::default().fg(Color::Cyan)
                },
            ),
            Span::styled(format!("{:<16} ", meta.tag), base),
            Span::styled(
                format!("stored:{}B  ", meta.stored_len),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("cksum:{:016x}{}", meta.checksum, star),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    let widget = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(" Timeline (F5) — Up/Down select · Enter /rewind <tick> · f /fork · Esc back ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(widget, area);
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;

    #[test]
    fn view_maps_from_keys_f1_to_f5() {
        assert_eq!(TuiView::from_key(1), Some(TuiView::Chat));
        assert_eq!(TuiView::from_key(2), Some(TuiView::Agents));
        assert_eq!(TuiView::from_key(3), Some(TuiView::Sprout));
        assert_eq!(TuiView::from_key(4), Some(TuiView::PlanDiff));
        assert_eq!(TuiView::from_key(5), Some(TuiView::Timeline));
        assert_eq!(TuiView::from_key(0), None);
        assert_eq!(TuiView::from_key(6), None);
        assert_eq!(TuiView::from_key(255), None);
    }

    #[test]
    fn view_from_slash_is_case_insensitive() {
        assert_eq!(TuiView::from_slash("chat"), Some(TuiView::Chat));
        assert_eq!(TuiView::from_slash("AGENTS"), Some(TuiView::Agents));
        assert_eq!(TuiView::from_slash("  Sprout "), Some(TuiView::Sprout));
        assert_eq!(TuiView::from_slash("plan"), Some(TuiView::PlanDiff));
        assert_eq!(TuiView::from_slash("Timeline"), Some(TuiView::Timeline));
        assert_eq!(TuiView::from_slash("nope"), None);
        assert_eq!(TuiView::from_slash("plans"), None);
        assert_eq!(TuiView::from_slash(""), None);
    }

    #[test]
    fn parse_view_command_intercepts_only_view_cmds() {
        assert_eq!(parse_view_command("/view"), Some(ViewCommandOutcome::Usage));
        assert_eq!(
            parse_view_command("/VIEW   "),
            Some(ViewCommandOutcome::Usage)
        );
        assert_eq!(
            parse_view_command("/view agents"),
            Some(ViewCommandOutcome::Switch(TuiView::Agents))
        );
        assert_eq!(
            parse_view_command("/View  PLAN"),
            Some(ViewCommandOutcome::Switch(TuiView::PlanDiff))
        );
        assert_eq!(
            parse_view_command("/view nope"),
            Some(ViewCommandOutcome::Unknown("nope".to_string()))
        );
        // No es un `/view` → se procesa como input normal.
        assert_eq!(parse_view_command("/views"), None);
        assert_eq!(parse_view_command("/viewx chat"), None);
        assert_eq!(parse_view_command("hola"), None);
    }

    #[test]
    fn tabs_line_renders_approval_badge_outside_chat() {
        let line = view_tabs_line(TuiView::Sprout, true);
        let rendered: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(rendered.contains("!APPROVAL"), "{rendered}");
        assert!(rendered.contains("F1 Chat"), "{rendered}");
        assert!(rendered.contains("F3 Sprout"), "{rendered}");

        // En Chat la aprobación se ve en su panel: sin badge.
        let line = view_tabs_line(TuiView::Chat, true);
        let rendered: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(!rendered.contains("!APPROVAL"), "{rendered}");

        // Sin aprobación: la etiqueta de Esc depende de la vista —
        // `Esc:Quit` solo en Chat; `Esc:Back` en las efímeras.
        let line = view_tabs_line(TuiView::Timeline, false);
        let rendered: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(rendered.contains("Esc:Back"), "{rendered}");
        let line = view_tabs_line(TuiView::Chat, false);
        let rendered: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(rendered.contains("Esc:Quit"), "{rendered}");
    }

    #[test]
    fn approval_y_n_only_in_chat_view() {
        // En `Sprout`, `y`/`n` NUNCA aprueban/deniegan.
        assert_eq!(
            approval_key_action(TuiView::Sprout, KeyCode::Char('y')),
            None
        );
        assert_eq!(
            approval_key_action(TuiView::Agents, KeyCode::Char('n')),
            None
        );
        assert_eq!(
            approval_key_action(TuiView::Timeline, KeyCode::Char('Y')),
            None
        );
        // En `Chat`, `y`/`n` aprueban/deniegan; otra tecla no.
        assert_eq!(
            approval_key_action(TuiView::Chat, KeyCode::Char('y')),
            Some(ApprovalKeyAction::Approve)
        );
        assert_eq!(
            approval_key_action(TuiView::Chat, KeyCode::Char('Y')),
            Some(ApprovalKeyAction::Approve)
        );
        assert_eq!(
            approval_key_action(TuiView::Chat, KeyCode::Char('n')),
            Some(ApprovalKeyAction::Deny)
        );
        assert_eq!(approval_key_action(TuiView::Chat, KeyCode::Char('x')), None);
    }

    #[test]
    fn sprout_gate_blocks_non_build_and_untrusted() {
        assert_eq!(
            sprout_mutation_gate(WorkspaceMode::Build, false),
            SproutGate::Allowed
        );
        assert_eq!(
            sprout_mutation_gate(WorkspaceMode::Plan, false),
            SproutGate::BlockedMode("PLAN")
        );
        assert_eq!(
            sprout_mutation_gate(WorkspaceMode::Explore, false),
            SproutGate::BlockedMode("EXPLORE")
        );
        assert_eq!(
            sprout_mutation_gate(WorkspaceMode::Build, true),
            SproutGate::BlockedUntrusted
        );
        assert!(SproutGate::Allowed.notice().is_empty());
        assert!(SproutGate::BlockedMode("PLAN").notice().contains("PLAN"));
        assert!(
            SproutGate::BlockedUntrusted
                .notice()
                .contains("[UNTRUSTED]")
        );
    }

    #[test]
    fn parse_sprout_panel_lines_extracts_rows() {
        let text = "[UNTRUSTED — run /trust]\n[READY] api (process :8080)\n[STOPPED] db (container)\n[STARTING] web (process :3000)";
        let rows = parse_sprout_panel_lines(text);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows[0].badge, "READY");
        assert_eq!(rows[0].id, "api");
        assert_eq!(rows[0].detail, "process :8080");
        assert_eq!(rows[1].badge, "STOPPED");
        assert_eq!(rows[1].id, "db");
        assert_eq!(rows[1].detail, "container");
        assert_eq!(rows[2].badge, "STARTING");
        assert_eq!(rows[2].id, "web");
        assert_eq!(rows[2].detail, "process :3000");
    }

    #[test]
    fn agents_view_renders_rows_and_bg_jobs() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let rows = vec![AgentRow {
            id: ConceptId(1),
            name: "openonyx-1".to_string(),
            is_root: true,
            state: "thinking".to_string(),
            goal: "coordinate tasks".to_string(),
        }];
        let bg_jobs = vec![SubagentJobSummary {
            job_id: "bg-1".to_string(),
            driver: "opencode".to_string(),
            name: "worker".to_string(),
            goal: "audit module".to_string(),
            spawn_tick: 3,
            branch_name: "wt/bg-1".to_string(),
            worktree_path: std::path::PathBuf::from("wt/bg-1"),
            status: "running",
            ok: None,
            touched_files: Vec::new(),
            summary: String::new(),
        }];
        let backend = TestBackend::new(100, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_agents_view(f, f.area(), &rows, &bg_jobs, Some(ConceptId(1)), 1);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rendered: String = (0..16)
            .map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Agents (F2)"), "{rendered}");
        assert!(rendered.contains("openonyx-1"), "{rendered}");
        assert!(rendered.contains("[bg]"), "{rendered}");
        assert!(rendered.contains("→ opencode"), "{rendered}");
    }
}
