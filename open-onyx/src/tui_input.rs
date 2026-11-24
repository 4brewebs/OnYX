//! Ergonomía de entrada TUI (`SDD-03` `REQ-03.4` y `REQ-03.5`):
//! - [`InputBuffer`]: edición UTF-8 segura con cursor (`Left`/`Right`/`Home`/`End`/`Backspace`/`Delete`).
//! - [`InputHistory`]: historial de prompts navegable (`Alt+Up`/`Alt+Down`, `Ctrl+P`/`Ctrl+N`) preservando borrador.
//! - [`PopupState`] + [`WorkspaceFileIndex`]: autocompletado ligero `/` (comandos slash) y `@` (archivos del workspace).
//! - [`QueuedPrompts`]: cola FIFO de prompts de seguimiento mientras el agente raíz está ocupado.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use onyx_workspace::WorkspacePolicy;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

const MAX_HISTORY_ENTRIES: usize = 200;
const MAX_POPUP_ITEMS: usize = 10;
const MAX_INDEXED_FILES: usize = 2_000;
const FILE_INDEX_TTL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlashCommandHint {
    pub command: &'static str,
    pub insert: &'static str,
    pub description: &'static str,
}

pub(crate) const SLASH_COMMANDS: &[SlashCommandHint] = &[
    SlashCommandHint {
        command: "/agent",
        insert: "/agent ",
        description: "Open agent selector or focus `<id|name>`",
    },
    SlashCommandHint {
        command: "/cancel",
        insert: "/cancel",
        description: "Stop active subagent or interrupt current turn (`/cancel all`)",
    },
    SlashCommandHint {
        command: "/compact",
        insert: "/compact ",
        description: "Compact older turns into summary (`[focus]`)",
    },
    SlashCommandHint {
        command: "/discard",
        insert: "/discard ",
        description: "Cancel and drop an async subagent worktree (`<job_id>`)",
    },
    SlashCommandHint {
        command: "/effort",
        insert: "/effort ",
        description: "Set LLM reasoning effort (`auto|low|medium|high`)",
    },
    SlashCommandHint {
        command: "/fork",
        insert: "/fork ",
        description: "Branch snapshots into sibling folder (`[name]`)",
    },
    SlashCommandHint {
        command: "/merge",
        insert: "/merge ",
        description: "3-way merge finished async subagent into root (`<job_id>`)",
    },
    SlashCommandHint {
        command: "/mode",
        insert: "/mode ",
        description: "Switch or toggle mode (`plan|build|explore`)",
    },
    SlashCommandHint {
        command: "/model",
        insert: "/model ",
        description: "Change the LLM model of the active target (`<name>`)",
    },
    SlashCommandHint {
        command: "/new",
        insert: "/new ",
        description: "Start a fresh root agent (`[goal]`)",
    },
    SlashCommandHint {
        command: "/profile",
        insert: "/profile ",
        description: "List LLM profiles or switch to one (`<id>`)",
    },
    SlashCommandHint {
        command: "/provider",
        insert: "/provider ",
        description: "Hot-swap LLM provider (`gemini|openai`)",
    },
    SlashCommandHint {
        command: "/rewind",
        insert: "/rewind ",
        description: "List milestones or rewind live to `<tick>`",
    },
    SlashCommandHint {
        command: "/service",
        insert: "/service ",
        description: "Control Sprout (`<start|stop|logs> <id|all>`)",
    },
    SlashCommandHint {
        command: "/services",
        insert: "/services",
        description: "Show status of `sprout.toml` services",
    },
    SlashCommandHint {
        command: "/spawn",
        insert: "/spawn ",
        description: "Launch background subagent without a root turn (`[--driver <id>] <task>`)",
    },
    SlashCommandHint {
        command: "/subagents",
        insert: "/subagents",
        description: "List background async worktree subagent jobs",
    },
    SlashCommandHint {
        command: "/trust",
        insert: "/trust",
        description: "Approve workspace config/sprout (`status|revoke`)",
    },
    SlashCommandHint {
        command: "/undo",
        insert: "/undo",
        description: "Revert latest reversible Git checkpoint",
    },
    SlashCommandHint {
        command: "/view",
        insert: "/view ",
        description: "Switch TUI view (chat|agents|sprout|plan|timeline)",
    },
];

/// Buffer de texto con cursor byte-exacto sobre fronteras UTF-8.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct InputBuffer {
    text: String,
    cursor: usize,
}

impl InputBuffer {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor_byte(&self) -> usize {
        self.cursor
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..next);
        true
    }

    pub fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = prev_char_boundary(&self.text, self.cursor);
        true
    }

    pub fn move_right(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        self.cursor = next_char_boundary(&self.text, self.cursor);
        true
    }

    pub fn move_home(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = 0;
        true
    }

    pub fn move_end(&mut self) -> bool {
        if self.cursor == self.text.len() {
            return false;
        }
        self.cursor = self.text.len();
        true
    }

    pub fn replace_range(&mut self, start: usize, end: usize, replacement: &str) {
        let s = start.min(self.text.len());
        let e = end.clamp(s, self.text.len());
        self.text.replace_range(s..e, replacement);
        self.cursor = s + replacement.len();
    }

    /// Renderiza el buffer con un cursor invertido visible dentro de `ratatui`.
    pub fn render_line(&self, prefix: &str) -> Line<'static> {
        let before = &self.text[..self.cursor];
        let rest = &self.text[self.cursor..];
        let cursor_style = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::REVERSED);
        if rest.is_empty() {
            Line::from(vec![
                Span::raw(format!("{prefix}{before}")),
                Span::styled(" ", cursor_style),
            ])
        } else {
            let next = next_char_boundary(rest, 0);
            let under = &rest[..next];
            let after = &rest[next..];
            Line::from(vec![
                Span::raw(format!("{prefix}{before}")),
                Span::styled(under.to_string(), cursor_style),
                Span::raw(after.to_string()),
            ])
        }
    }
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len()).saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len()).saturating_add(1).min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Historial de prompts de sesión con restauración del borrador en curso.
#[derive(Clone, Debug, Default)]
pub(crate) struct InputHistory {
    entries: Vec<String>,
    cursor: Option<usize>,
    draft: String,
}

impl InputHistory {
    pub fn push(&mut self, entry: String) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.entries.last().map(String::as_str) != Some(trimmed) {
            self.entries.push(trimmed.to_string());
            if self.entries.len() > MAX_HISTORY_ENTRIES {
                let excess = self.entries.len() - MAX_HISTORY_ENTRIES;
                self.entries.drain(0..excess);
            }
        }
        self.reset_navigation();
    }

    pub fn prev(&mut self, current_input: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next_idx = match self.cursor {
            None => {
                self.draft = current_input.to_string();
                self.entries.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.cursor = Some(next_idx);
        self.entries.get(next_idx).cloned()
    }

    pub fn next(&mut self) -> Option<String> {
        let cur = self.cursor?;
        if cur + 1 < self.entries.len() {
            let next_idx = cur + 1;
            self.cursor = Some(next_idx);
            self.entries.get(next_idx).cloned()
        } else {
            self.cursor = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    pub fn reset_navigation(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }
}

/// Cola FIFO de mensajes de seguimiento escritos mientras el agente raíz está ocupado.
#[derive(Clone, Debug, Default)]
pub(crate) struct QueuedPrompts {
    items: VecDeque<String>,
}

impl QueuedPrompts {
    pub fn push(&mut self, prompt: String) {
        let trimmed = prompt.trim();
        if !trimmed.is_empty() {
            self.items.push_back(trimmed.to_string());
        }
    }

    pub fn pop_front(&mut self) -> Option<String> {
        self.items.pop_front()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn peek_front(&self) -> Option<&str> {
        self.items.front().map(String::as_str)
    }

    pub fn clear(&mut self) -> usize {
        let n = self.items.len();
        self.items.clear();
        n
    }
}

/// Caché ligera de rutas relativas del workspace respetando `WorkspacePolicy`.
#[derive(Clone, Debug, Default)]
pub(crate) struct WorkspaceFileIndex {
    files: Vec<String>,
    last_refresh: Option<Instant>,
}

impl WorkspaceFileIndex {
    pub fn files(&mut self, root: Option<&Path>, policy: Option<&WorkspacePolicy>) -> &[String] {
        let needs_refresh = self
            .last_refresh
            .is_none_or(|t| t.elapsed() >= FILE_INDEX_TTL);
        if needs_refresh {
            if let Some(root_dir) = root {
                self.files = collect_workspace_files(root_dir, policy, MAX_INDEXED_FILES);
                self.last_refresh = Some(Instant::now());
            }
        }
        &self.files
    }

    #[cfg(test)]
    pub fn from_paths(paths: Vec<String>) -> Self {
        Self {
            files: paths,
            last_refresh: Some(Instant::now()),
        }
    }
}

fn collect_workspace_files(
    root: &Path,
    policy: Option<&WorkspacePolicy>,
    limit: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for path in children {
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if is_ignored_path(root, &path, policy) {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                    if out.len() >= limit {
                        out.sort();
                        return out;
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn is_ignored_path(root: &Path, path: &Path, policy: Option<&WorkspacePolicy>) -> bool {
    if let Some(p) = policy {
        return p.is_ignored(path);
    }
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut prev_onyx = false;
    rel.components().any(|c| {
        if let std::path::Component::Normal(name) = c {
            let s = name.to_string_lossy();
            let s_ref = s.as_ref();
            let vault = prev_onyx && s_ref.eq_ignore_ascii_case("vault");
            prev_onyx = s_ref.eq_ignore_ascii_case(".onyx");
            vault
                || matches!(s_ref, ".git" | "target" | "node_modules" | "snapshots")
                || s_ref.starts_with("snaps_")
                || s_ref.ends_with(".tr")
                || s_ref.ends_with(".onyxpack")
        } else {
            prev_onyx = false;
            false
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PopupKind {
    Slash,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PopupItem {
    pub label: String,
    pub detail: String,
    pub replacement: String,
}

/// Estado del popup de autocompletado (`/` comandos o `@` rutas de archivo).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PopupState {
    pub kind: Option<PopupKind>,
    pub query: String,
    pub replace_start: usize,
    pub replace_end: usize,
    pub items: Vec<PopupItem>,
    pub selected: usize,
    dismissed_at: Option<(usize, usize)>,
}

impl PopupState {
    pub fn is_open(&self) -> bool {
        self.kind.is_some() && !self.items.is_empty()
    }

    pub fn close(&mut self) {
        self.dismissed_at = self.kind.map(|_| (self.replace_start, self.replace_end));
        self.kind = None;
        self.items.clear();
        self.selected = 0;
    }

    pub fn reset_dismiss(&mut self) {
        self.dismissed_at = None;
    }

    pub fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn accept_into(&mut self, buf: &mut InputBuffer) -> bool {
        let Some(item) = self.items.get(self.selected).cloned() else {
            return false;
        };
        buf.replace_range(self.replace_start, self.replace_end, &item.replacement);
        self.kind = None;
        self.items.clear();
        self.selected = 0;
        self.dismissed_at = None;
        true
    }

    /// Recalcula el popup activo según el texto y posición del cursor en `buf`.
    pub fn sync_from_buffer(
        &mut self,
        buf: &InputBuffer,
        file_index: &mut WorkspaceFileIndex,
        root: Option<&Path>,
        policy: Option<&WorkspacePolicy>,
    ) {
        let text = buf.as_str();
        let cursor = buf.cursor_byte();
        let prefix = &text[..cursor];

        // 1. Trigger `/` al inicio de la línea (sin espacios antes del cursor).
        if prefix.starts_with('/') && !prefix.chars().any(char::is_whitespace) {
            let token_end = text[cursor..]
                .find(char::is_whitespace)
                .map(|off| cursor + off)
                .unwrap_or(text.len());
            if self.dismissed_at == Some((0, token_end)) {
                return;
            }
            let query = prefix.to_ascii_lowercase();
            let mut items: Vec<PopupItem> = SLASH_COMMANDS
                .iter()
                .filter(|c| c.command.starts_with(&query) || c.command.contains(&query))
                .take(MAX_POPUP_ITEMS)
                .map(|c| PopupItem {
                    label: c.command.to_string(),
                    detail: c.description.to_string(),
                    replacement: c.insert.to_string(),
                })
                .collect();
            if items.is_empty() {
                self.kind = None;
                self.items.clear();
                self.selected = 0;
                return;
            }
            items.sort_by_key(|it| (!it.label.starts_with(&query), it.label.clone()));
            self.kind = Some(PopupKind::Slash);
            self.query = prefix.to_string();
            self.replace_start = 0;
            self.replace_end = token_end;
            self.items = items;
            if self.selected >= self.items.len() {
                self.selected = 0;
            }
            return;
        }

        // 2. Trigger `@` al inicio de un token (`@` al principio o precedido de espacio).
        if let Some((at_pos, query)) = find_at_trigger(prefix) {
            let token_end = text[cursor..]
                .find(char::is_whitespace)
                .map(|off| cursor + off)
                .unwrap_or(text.len());
            if self.dismissed_at == Some((at_pos, token_end)) {
                return;
            }
            let all_files = file_index.files(root, policy);
            let matches = rank_file_matches(all_files, query, MAX_POPUP_ITEMS);
            if matches.is_empty() {
                self.kind = None;
                self.items.clear();
                self.selected = 0;
                return;
            }
            self.kind = Some(PopupKind::File);
            self.query = query.to_string();
            self.replace_start = at_pos;
            self.replace_end = token_end;
            self.items = matches
                .into_iter()
                .map(|rel| PopupItem {
                    label: rel.clone(),
                    detail: String::new(),
                    replacement: format!("{rel} "),
                })
                .collect();
            if self.selected >= self.items.len() {
                self.selected = 0;
            }
            return;
        }

        self.kind = None;
        self.items.clear();
        self.selected = 0;
        self.dismissed_at = None;
    }
}

fn find_at_trigger(prefix: &str) -> Option<(usize, &str)> {
    let at_pos = prefix.rfind('@')?;
    // Evitar disparar en direcciones de correo (`user@domain`).
    if at_pos > 0 {
        let prev_char = prefix[..at_pos].chars().next_back()?;
        if !prev_char.is_whitespace() {
            return None;
        }
    }
    let query = &prefix[at_pos + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at_pos, query))
}

fn rank_file_matches(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let q = query.to_ascii_lowercase();
    if q.is_empty() {
        return files.iter().take(limit).cloned().collect();
    }
    let mut scored: Vec<(u8, usize, &str)> = Vec::new();
    for path in files {
        let lower = path.to_ascii_lowercase();
        let basename = lower.rsplit('/').next().unwrap_or(&lower);
        if basename.starts_with(&q) {
            scored.push((0, path.len(), path.as_str()));
        } else if basename.contains(&q) {
            scored.push((1, path.len(), path.as_str()));
        } else if lower.contains(&q) {
            scored.push((2, path.len(), path.as_str()));
        } else if is_subsequence(&q, &lower) {
            scored.push((3, path.len(), path.as_str()));
        }
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, p)| p.to_string())
        .collect()
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut it = haystack.chars();
    for nc in needle.chars() {
        if !it.any(|hc| hc == nc) {
            return false;
        }
    }
    true
}

/// Renderiza el popup flotante de autocompletado justo encima de `input_area`.
pub(crate) fn render_input_popup(
    f: &mut Frame<'_>,
    frame_area: Rect,
    input_area: Rect,
    popup: &PopupState,
) {
    if !popup.is_open() || frame_area.width < 24 || input_area.y < 4 {
        return;
    }
    let visible_items = popup.items.len().min(MAX_POPUP_ITEMS);
    let max_h = input_area.y.saturating_sub(1).min(12);
    let popup_h = u16::try_from(visible_items.saturating_add(2))
        .unwrap_or(10)
        .min(max_h);
    if popup_h < 3 {
        return;
    }
    let popup_w = frame_area.width.saturating_sub(4).clamp(24, 92);
    let x = input_area
        .x
        .saturating_add(1)
        .min(frame_area.width - popup_w);
    let y = input_area.y.saturating_sub(popup_h);
    let area = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, area);

    let max_rows = usize::from(popup_h.saturating_sub(2));
    let scroll_start = if popup.selected >= max_rows {
        popup.selected + 1 - max_rows
    } else {
        0
    };

    let mut lines = Vec::with_capacity(max_rows);
    for (idx, item) in popup
        .items
        .iter()
        .enumerate()
        .skip(scroll_start)
        .take(max_rows)
    {
        let is_sel = idx == popup.selected;
        let label_style = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        };
        let detail_style = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let marker = if is_sel { "▸ " } else { "  " };
        if item.detail.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("{marker}{}", item.label),
                label_style,
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{marker}{:<12}", item.label), label_style),
                Span::styled(format!(" {}", item.detail), detail_style),
            ]));
        }
    }

    let title = match popup.kind {
        Some(PopupKind::Slash) => " Commands (↑/↓ · Tab/Enter accept · Esc close) ".to_string(),
        Some(PopupKind::File) => {
            if popup.query.is_empty() {
                " Files @ (↑/↓ · Tab/Enter insert · Esc close) ".to_string()
            } else {
                format!(
                    " Files @{} (↑/↓ · Tab/Enter insert · Esc close) ",
                    popup.query
                )
            }
        }
        None => String::new(),
    };

    let widget = Paragraph::new(lines).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(widget, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn input_buffer_supports_utf8_cursor_navigation_and_editing() {
        let mut buf = InputBuffer::default();
        for ch in "añó!".chars() {
            buf.insert_char(ch);
        }
        assert_eq!(buf.as_str(), "añó!");
        assert!(buf.move_left());
        assert!(buf.move_left());
        buf.insert_char('X');
        assert_eq!(buf.as_str(), "añXó!");
        assert!(buf.backspace());
        assert_eq!(buf.as_str(), "añó!");
        assert!(buf.delete());
        assert_eq!(buf.as_str(), "añ!");
        assert!(buf.move_home());
        assert_eq!(buf.cursor_byte(), 0);
        assert!(buf.move_end());
        assert_eq!(buf.cursor_byte(), buf.as_str().len());
    }

    #[test]
    fn input_history_navigates_and_restores_unsent_draft() {
        let mut hist = InputHistory::default();
        hist.push("first goal".to_string());
        hist.push("first goal".to_string()); // dedup consecutive
        hist.push("second goal".to_string());

        assert_eq!(hist.prev("typing draft").as_deref(), Some("second goal"));
        assert_eq!(hist.prev("second goal").as_deref(), Some("first goal"));
        assert_eq!(hist.prev("first goal").as_deref(), Some("first goal"));
        assert_eq!(hist.next().as_deref(), Some("second goal"));
        assert_eq!(hist.next().as_deref(), Some("typing draft"));
        assert_eq!(hist.next(), None);
    }

    #[test]
    fn popup_completes_slash_commands_and_at_files_and_renders_in_test_backend() {
        let mut files = WorkspaceFileIndex::from_paths(vec![
            "open-onyx/src/main.rs".to_string(),
            "open-onyx/src/tui.rs".to_string(),
            "open-onyx/src/vault.rs".to_string(),
            "onyx-workspace/src/policy.rs".to_string(),
        ]);
        let mut buf = InputBuffer::default();
        let mut popup = PopupState::default();

        // 1. Slash command autocomplete
        buf.set_text("/re");
        popup.sync_from_buffer(&buf, &mut files, None, None);
        assert_eq!(popup.kind, Some(PopupKind::Slash));
        assert!(popup.items.iter().any(|i| i.label == "/rewind"));
        assert!(popup.accept_into(&mut buf));
        assert_eq!(buf.as_str(), "/rewind ");

        // 2. Email should NOT trigger `@` popup
        buf.set_text("contact user@example.com");
        popup.sync_from_buffer(&buf, &mut files, None, None);
        assert!(!popup.is_open());

        // 3. `@` file autocomplete + TestBackend render
        buf.set_text("inspect @tui");
        popup.sync_from_buffer(&buf, &mut files, None, None);
        assert_eq!(popup.kind, Some(PopupKind::File));
        assert_eq!(popup.items[0].label, "open-onyx/src/tui.rs");

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let input_area = Rect::new(0, 16, 80, 4);
                render_input_popup(f, area, input_area, &popup);
            })
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("Files @tui"), "{rendered}");
        assert!(rendered.contains("open-onyx/src/tui.rs"), "{rendered}");

        assert!(popup.accept_into(&mut buf));
        assert_eq!(buf.as_str(), "inspect open-onyx/src/tui.rs ");
    }

    #[test]
    fn queued_prompts_preserve_fifo_order_and_clear() {
        let mut q = QueuedPrompts::default();
        q.push("  step one  ".to_string());
        q.push("step two".to_string());
        assert_eq!(q.len(), 2);
        assert_eq!(q.peek_front(), Some("step one"));
        assert_eq!(q.pop_front().as_deref(), Some("step one"));
        assert_eq!(q.clear(), 1);
        assert!(q.is_empty());
    }
}
