//! `SDD-03` Fase 3 (`REQ-03.6`): paleta `Ctrl+K` — navegación y cambios LLM
//! en un popup centrado con filtrado **fuzzy en vivo**.
//!
//! Catálogo agrupado:
//! - **View**: las 5 vistas (`F1 Chat` … `F5 Timeline`).
//! - **LLM**: perfiles (`/profile <id>`), providers (`/provider <p>`) y
//!   esfuerzos (`/effort <e>`) — al pulsar `Enter` se encola el slash
//!   correspondiente en `self.queued` para que pase por el bucle determinista
//!   y quede sellado en `ONYXTR2`.
//! - **Agent**: saltar el foco a cualquier agente activo.
//!
//! Lógica 100 % pura (testable sin terminal): [`PaletteState`] es una máquina
//! de estados y [`fuzzy_matches`] el filtro (subsecuencia case-insensitive,
//! misma idea que el popup `@` de `tui_input`).

use onyx_soul_agents::ConceptId;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::llm_profile::LlmProfilesPublicState;
use crate::time_debug::TimeDebugState;
use crate::tui::AgentRow;
use crate::tui_views::TuiView;

/// Acción que produce seleccionar un ítem de la paleta con `Enter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    /// Cambia la vista efímera (navegación local, 0 impacto en el grafo).
    View(TuiView),
    /// Comando slash a encolar en el bus determinista (`/profile`,
    /// `/provider`, `/effort`).
    Command(String),
    /// Enfoque a un agente activo (navegación local).
    Agent(ConceptId),
}

/// Ítem del catálogo de la paleta.
#[derive(Debug, Clone)]
pub struct PaletteItem {
    pub label: String,
    pub group: &'static str,
    pub hint: String,
    pub enabled: bool,
    action: PaletteAction,
}

impl PaletteItem {
    fn new(
        group: &'static str,
        label: impl Into<String>,
        hint: impl Into<String>,
        action: PaletteAction,
    ) -> Self {
        Self {
            label: label.into(),
            group,
            hint: hint.into(),
            enabled: true,
            action,
        }
    }

    /// `F1 Chat` … `F5 Timeline`.
    pub fn view(view: TuiView) -> Self {
        Self::new(
            "View",
            format!("{} {}", view.shortcut(), view.as_str()),
            "switch view",
            PaletteAction::View(view),
        )
    }

    /// Perfil LLM registrado; los no disponibles se muestran pero **no** son
    /// seleccionables (el motivo va en el hint).
    pub fn llm_profile(
        id: &str,
        provider: &str,
        model: &str,
        effort: &str,
        available: bool,
        unavailable_reason: Option<&str>,
    ) -> Self {
        let mut hint = format!("{provider}/{model} (effort:{effort})");
        if !available {
            match unavailable_reason {
                Some(reason) => hint.push_str(&format!(" — unavailable: {reason}")),
                None => hint.push_str(" — unavailable"),
            };
        }
        let item = Self::new(
            "LLM",
            format!("profile {id}"),
            hint,
            PaletteAction::Command(format!("/profile {id}")),
        );
        Self {
            enabled: available,
            ..item
        }
    }

    /// Provider con credenciales disponibles.
    pub fn llm_provider(provider: &str) -> Self {
        Self::new(
            "LLM",
            format!("provider {provider}"),
            "switch provider",
            PaletteAction::Command(format!("/provider {provider}")),
        )
    }

    /// Esfuerzo de razonamiento (`auto|low|medium|high`).
    pub fn llm_effort(effort: &str) -> Self {
        Self::new(
            "LLM",
            format!("effort {effort}"),
            "reasoning effort",
            PaletteAction::Command(format!("/effort {effort}")),
        )
    }

    /// Agente activo (el foco vuelve a `F1 Chat`).
    pub fn agent(id: ConceptId, name: &str, is_root: bool) -> Self {
        let tag = if is_root { "root" } else { "sub" };
        Self::new(
            "Agent",
            format!("#{} {name}", id.0),
            format!("focus [{tag}]"),
            PaletteAction::Agent(id),
        )
    }

    /// `SDD-07` Fase 3: ítem del grupo **Time** (depurador temporal). El
    /// label de `toggle freeze` es **dinámico** según el estado actual
    /// (congelado → "resume", en vivo → "freeze").
    pub fn time(label: impl Into<String>, hint: impl Into<String>, cmd: &str) -> Self {
        Self::new("Time", label, hint, PaletteAction::Command(cmd.to_string()))
    }

    pub fn action(&self) -> &PaletteAction {
        &self.action
    }
}

/// Fuzzy en vivo: `query` debe aparecer como **subsecuencia** case-insensitive
/// en `text` (misma idea que el popup `@` de `tui_input`). Vacío → todo pasa.
pub fn fuzzy_matches(query: &str, text: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let needle = q.to_ascii_lowercase();
    let lower = text.to_ascii_lowercase();
    let mut hay = lower.chars();
    needle.chars().all(|nc| hay.any(|hc| hc == nc))
}

/// Estado de la paleta: query en vivo, selección y catálogo filtrado.
pub struct PaletteState {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    items: Vec<PaletteItem>,
    /// Índices (en `items`) que pasan el filtro fuzzy, en orden de catálogo.
    filtered: Vec<usize>,
}

impl PaletteState {
    /// Paleta cerrada y vacía (estado inicial de `TuiInner`).
    pub fn closed() -> Self {
        Self {
            open: false,
            query: String::new(),
            selected: 0,
            items: Vec::new(),
            filtered: Vec::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Abre la paleta con un catálogo recién construido.
    pub fn open_with(&mut self, items: Vec<PaletteItem>) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.items = items;
        self.recompute_filtered();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = 0;
        self.items.clear();
        self.filtered.clear();
    }

    fn recompute_filtered(&mut self) {
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.enabled && fuzzy_matches(&self.query, &item.label))
            .map(|(idx, _)| idx)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    pub fn insert_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.query.push(c);
        self.recompute_filtered();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.recompute_filtered();
    }

    pub fn move_up(&mut self) {
        let len = self.filtered.len();
        if len > 0 {
            self.selected = if self.selected == 0 {
                len - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn move_down(&mut self) {
        let len = self.filtered.len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    /// Ítem bajo el cursor (ya filtrado).
    pub fn selected_item(&self) -> Option<&PaletteItem> {
        self.filtered
            .get(self.selected)
            .and_then(|&idx| self.items.get(idx))
    }

    /// Ítems visibles (filtrados) en orden de catálogo.
    pub fn visible_items(&self) -> Vec<&PaletteItem> {
        self.filtered.iter().map(|&idx| &self.items[idx]).collect()
    }
}

/// Construye el catálogo de la paleta: 5 vistas + **Time** (depurador
/// temporal, solo si hay `TimeDebugState`) + LLM (perfiles/providers/esfuerzos,
/// solo si hay handle) + agentes del último render.
pub fn build_palette_items(
    profiles: Option<&LlmProfilesPublicState>,
    agent_rows: &[AgentRow],
    time_debug: Option<&TimeDebugState>,
) -> Vec<PaletteItem> {
    let mut items: Vec<PaletteItem> = TuiView::ALL.iter().map(|v| PaletteItem::view(*v)).collect();
    if let Some(td) = time_debug {
        // Label dinámico: congelado → "resume", en vivo → "freeze".
        let toggle_label = if td.is_frozen() {
            "time toggle freeze → resume (go to present)"
        } else {
            "time toggle freeze (pause here)"
        };
        items.push(PaletteItem::time(
            toggle_label,
            "toggle TimeFreeze",
            "/time",
        ));
        items.push(PaletteItem::time(
            "time step 1",
            "advance 1 tick (frozen)",
            "/step 1",
        ));
        items.push(PaletteItem::time(
            "time step-llm",
            "allow 1 LLM turn (frozen)",
            "/step-llm",
        ));
        items.push(PaletteItem::time(
            "time go (resume)",
            "return to present + live",
            "/go",
        ));
        items.push(PaletteItem::time(
            "graph inspect mem",
            "preview memory graph at cursor",
            "/graph inspect mem",
        ));
    }
    if let Some(snapshot) = profiles {
        for entry in &snapshot.profiles {
            items.push(PaletteItem::llm_profile(
                &entry.id,
                &entry.provider,
                &entry.model,
                &entry.effort,
                entry.available,
                entry.unavailable_reason.as_deref(),
            ));
        }
        for provider in &snapshot.available_providers {
            items.push(PaletteItem::llm_provider(provider));
        }
        for effort in ["auto", "low", "medium", "high"] {
            items.push(PaletteItem::llm_effort(effort));
        }
    }
    for row in agent_rows {
        items.push(PaletteItem::agent(row.id, &row.name, row.is_root));
    }
    items
}

/// Renderiza el popup centrado de la paleta (cuadro de búsqueda + ítems
/// agrupados con el cursor resaltado). Recibe los ítems visibles ya filtrados
/// (snapshot de [`PaletteState`]) para que el closure de `draw` no capture el
/// estado completo de la TUI.
pub fn render_palette(
    f: &mut Frame<'_>,
    area: Rect,
    visible: &[PaletteItem],
    selected: usize,
    query: &str,
) {
    if area.width < 44 || area.height < 10 {
        return;
    }
    let popup_w = area.width.saturating_sub(8).clamp(46, 90).min(area.width);
    let desired_h = visible.len().saturating_add(5).clamp(8, 26);
    let popup_h = u16::try_from(desired_h)
        .unwrap_or(12)
        .clamp(8, 24)
        .min(area.height.saturating_sub(2).max(8));
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup_area = Rect::new(x, y, popup_w, popup_h);
    f.render_widget(Clear, popup_area);

    // Área interna: borders (2) + title + title_bottom (2).
    let inner_rows = usize::from(popup_h).saturating_sub(4).max(1);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut prev_group: Option<&'static str> = None;
    let mut selected_line: Option<usize> = None;
    for (i, item) in visible.iter().enumerate() {
        if prev_group != Some(item.group) {
            lines.push(Line::from(Span::styled(
                item.group.to_uppercase(),
                Style::default().fg(Color::DarkGray),
            )));
            prev_group = Some(item.group);
        }
        let row_idx = lines.len();
        let is_sel = i == selected;
        let base = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let dim = if is_sel {
            base
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", item.label), base),
            Span::styled(format!("  {}", item.hint), dim),
        ]));
        if is_sel {
            selected_line = Some(row_idx);
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let scroll = selected_line
        .map(|idx| idx.saturating_sub(inner_rows.saturating_sub(1)))
        .unwrap_or(0);
    let title = if query.is_empty() {
        " Ctrl+K Palette — type to filter ".to_string()
    } else {
        format!(" Ctrl+K Palette — {query} ")
    };
    let block = Block::default()
        .title(title)
        .title_bottom(
            Line::from(Span::styled(
                " Up/Down · Enter run · Esc close ",
                Style::default().fg(Color::DarkGray),
            ))
            .right_aligned(),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let widget = Paragraph::new(lines)
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
        .block(block);
    f.render_widget(widget, popup_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profiles() -> LlmProfilesPublicState {
        use crate::llm_profile::LlmProfilePublicEntry;
        LlmProfilesPublicState {
            active_profile_id: "default".to_string(),
            provider: "gemini".to_string(),
            model: "gemini-2.5-pro".to_string(),
            effort: "auto".to_string(),
            profiles: vec![
                LlmProfilePublicEntry {
                    id: "default".to_string(),
                    provider: "gemini".to_string(),
                    model: "gemini-2.5-pro".to_string(),
                    effort: "auto".to_string(),
                    available: true,
                    unavailable_reason: None,
                },
                LlmProfilePublicEntry {
                    id: "local-ollama".to_string(),
                    provider: "openai".to_string(),
                    model: "qwen3-8b".to_string(),
                    effort: "high".to_string(),
                    available: false,
                    unavailable_reason: Some("API key env var `OLLAMA_KEY` is not set".to_string()),
                },
            ],
            available_providers: vec!["gemini".to_string()],
        }
    }

    fn sample_agents() -> Vec<AgentRow> {
        vec![
            AgentRow {
                id: ConceptId(1),
                name: "openonyx-1".to_string(),
                is_root: true,
                state: "done".to_string(),
                goal: "audit repo".to_string(),
            },
            AgentRow {
                id: ConceptId(2),
                name: "auditor".to_string(),
                is_root: false,
                state: "acting".to_string(),
                goal: "audit open-onyx".to_string(),
            },
        ]
    }

    #[test]
    fn fuzzy_matches_is_subsequence_case_insensitive() {
        assert!(fuzzy_matches("", "profile default"));
        assert!(fuzzy_matches("pf", "profile default"));
        assert!(fuzzy_matches("PD", "profile default"));
        assert!(fuzzy_matches("pro", "profile default"));
        assert!(!fuzzy_matches("xyz", "profile default"));
        assert!(!fuzzy_matches("dpl", "profile default"));
    }

    #[test]
    fn palette_filters_and_selects_llm_items() {
        let profiles = sample_profiles();
        let agents = sample_agents();
        let items = build_palette_items(Some(&profiles), &agents, None);
        let mut state = PaletteState::closed();
        state.open_with(items);

        // Query vacío: 5 vistas + 1 perfil disponible (el no disponible queda
        // fuera del filtro) + 1 provider + 4 efforts + 2 agents.
        assert_eq!(state.visible_items().len(), 13);

        state.insert_char('p');
        state.insert_char('r');
        state.insert_char('o');
        // "pro" → profile default, profile local-ollama (no, disponible),
        // provider gemini.
        let visible: Vec<String> = state
            .visible_items()
            .iter()
            .map(|i| i.label.clone())
            .collect();
        assert!(
            visible.contains(&"profile default".to_string()),
            "{visible:?}"
        );
        assert!(
            visible.contains(&"provider gemini".to_string()),
            "{visible:?}"
        );
        assert!(
            !visible.contains(&"profile local-ollama".to_string()),
            "el perfil no disponible no debe ser seleccionable: {visible:?}"
        );

        state.move_down();
        let item = state.selected_item().expect("seleccion");
        assert!(matches!(item.action(), PaletteAction::Command(cmd) if cmd == "/profile default"));
    }

    #[test]
    fn palette_items_emit_correct_slash_commands() {
        let profiles = sample_profiles();
        let items = build_palette_items(Some(&profiles), &sample_agents(), None);

        let view = items.iter().find(|i| i.label == "F2 Agents").unwrap();
        assert_eq!(view.action(), &PaletteAction::View(TuiView::Agents));

        let effort = items.iter().find(|i| i.label == "effort high").unwrap();
        assert_eq!(
            effort.action(),
            &PaletteAction::Command("/effort high".to_string())
        );

        let provider = items.iter().find(|i| i.label == "provider gemini").unwrap();
        assert_eq!(
            provider.action(),
            &PaletteAction::Command("/provider gemini".to_string())
        );

        let profile = items.iter().find(|i| i.label == "profile default").unwrap();
        assert_eq!(
            profile.action(),
            &PaletteAction::Command("/profile default".to_string())
        );

        let agent = items.iter().find(|i| i.label == "#2 auditor").unwrap();
        assert_eq!(agent.action(), &PaletteAction::Agent(ConceptId(2)));
    }

    #[test]
    fn palette_without_handle_has_no_llm_items() {
        let items = build_palette_items(None, &sample_agents(), None);
        assert!(items.iter().all(|i| i.group != "LLM"));
        assert_eq!(items.len(), 5 + 2);
    }

    #[test]
    fn palette_navigation_wraps_and_clamps_on_filter() {
        let mut state = PaletteState::closed();
        state.open_with(TuiView::ALL.iter().map(|v| PaletteItem::view(*v)).collect());
        // 5 ítems: Down x5 → vuelve al 0.
        for _ in 0..5 {
            state.move_down();
        }
        assert_eq!(state.selected, 0);
        // Up en 0 → último.
        state.move_up();
        assert_eq!(state.selected, 4);
        // Filtrar de 5 a 1: la selección queda válida (0).
        state.insert_char('t');
        state.insert_char('i');
        state.insert_char('m');
        // "tim" → solo "F5 Timeline".
        assert_eq!(state.visible_items().len(), 1);
        assert_eq!(state.selected_item().unwrap().label, "F5 Timeline");
    }

    #[test]
    fn palette_renders_groups_and_selection() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let profiles = sample_profiles();
        let items = build_palette_items(Some(&profiles), &sample_agents(), None);
        let mut state = PaletteState::closed();
        state.open_with(items);
        state.move_down();
        let visible: Vec<PaletteItem> = state.visible_items().into_iter().cloned().collect();
        let selected = state.selected;
        let query = state.query.clone();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_palette(f, f.area(), &visible, selected, &query);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rendered: String = (0..24)
            .map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Ctrl+K Palette"), "{rendered}");
        assert!(rendered.contains("VIEW"), "{rendered}");
        assert!(rendered.contains("F1 Chat"), "{rendered}");
        assert!(rendered.contains("profile default"), "{rendered}");
        assert!(
            rendered.contains("Up/Down · Enter run · Esc close"),
            "{rendered}"
        );
    }
}
