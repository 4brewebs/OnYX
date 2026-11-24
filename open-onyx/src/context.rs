//! Presupuesto de contexto por niveles (`TieredContextConfig`), compactación determinista
//! (`Tier 0` / `Tier 1` / `Tier 2`), estabilidad de prefijo para Prompt Caching y telemetría
//! normalizada de tokens (`TokenUsage` / `SessionTelemetry`).
//!
//! Compartido por `gemini.rs`, `openai.rs`, `command.rs` y `tui.rs` sin introducir
//! dependencias en `onyx-core` ni `onyx-soul-agents`.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value as JsonValue;

pub const DEFAULT_MODEL_CONTEXT_TOKENS: usize = 200_000;
pub const DEFAULT_KEEP_RECENT_TOOL_OUTPUTS: usize = 10;
pub const DEFAULT_SOFT_CHAR_BUDGET: usize = 120_000;
pub const DEFAULT_AUTO_COMPACT_RATIO: f32 = 0.80;
pub const DEFAULT_CONTEXT_BUDGET_BYTES: usize = DEFAULT_SOFT_CHAR_BUDGET;
pub const COMPACT_MARKER: &str = "[/compact] ";

/// Nivel de esfuerzo de razonamiento / thinking solicitado al proveedor LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" | "med" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(format!(
                "unsupported reasoning effort `{other}` (expected `low`, `medium`, or `high`)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn as_gemini_level(self) -> &'static str {
        match self {
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
        }
    }
}

/// Compatibilidad con la API previa de presupuesto por bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub max_memory_bytes: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_CONTEXT_BUDGET_BYTES,
        }
    }
}

/// Nivel de retención asignado a una observación de herramienta en el contexto proyectado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutputTier {
    /// Contenido íntegro (últimas N salidas dentro del presupuesto blando).
    Tier0Full,
    /// Resumen estructural determinista preservando coordenadas (ruta, líneas, símbolos/hits, exit code).
    Tier1Summary,
    /// Bloque de resumen de sesión tras frontera `/compact` o auto-compactación.
    Tier2Compacted,
}

/// Configuración de presupuesto de contexto por niveles (SDD-01).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TieredContextConfig {
    /// Ventana máxima estimada del modelo activo (en tokens).
    pub model_context_tokens: usize,
    /// Salidas de herramientas recientes preservadas íntegramente.
    pub keep_recent_tool_outputs: usize,
    /// Presupuesto blando en bytes antes de degradar salidas antiguas a Tier 1.
    pub soft_char_budget: usize,
    /// Umbral de llenado (`0.0..=1.0`) para auto-compactación Tier 2.
    pub auto_compact_ratio: f32,
}

impl Default for TieredContextConfig {
    fn default() -> Self {
        Self {
            model_context_tokens: DEFAULT_MODEL_CONTEXT_TOKENS,
            keep_recent_tool_outputs: DEFAULT_KEEP_RECENT_TOOL_OUTPUTS,
            soft_char_budget: DEFAULT_SOFT_CHAR_BUDGET,
            auto_compact_ratio: DEFAULT_AUTO_COMPACT_RATIO,
        }
    }
}

impl TieredContextConfig {
    pub fn for_model(model: &str) -> Self {
        Self {
            model_context_tokens: model_context_window(model),
            ..Self::default()
        }
    }
}

impl From<ContextBudget> for TieredContextConfig {
    fn from(b: ContextBudget) -> Self {
        Self {
            soft_char_budget: b.max_memory_bytes,
            ..Self::default()
        }
    }
}

/// Entrada proyectada agnóstica de proveedor lista para serializar a Gemini u OpenAI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectedTurn {
    User(String),
    AssistantText(String),
    ToolCall {
        call_index: usize,
        call_id: String,
        name: String,
        args_json: String,
    },
    ToolObserve {
        call_id: Option<String>,
        name: String,
        content: String,
        tier: ToolOutputTier,
        is_toon: bool,
        is_superseded: bool,
    },
    ToolError {
        call_id: Option<String>,
        name: String,
        error: String,
    },
    ToolDenied {
        call_id: Option<String>,
        name: String,
        reason: String,
    },
}

impl ProjectedTurn {
    pub fn byte_len(&self) -> usize {
        match self {
            Self::User(s) | Self::AssistantText(s) => s.len(),
            Self::ToolCall {
                name, args_json, ..
            } => name.len() + args_json.len(),
            Self::ToolObserve { content, .. } => content.len(),
            Self::ToolError { error, .. } => error.len(),
            Self::ToolDenied { reason, .. } => reason.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectedMemory {
    pub turns: Vec<ProjectedTurn>,
    pub total_bytes: usize,
    pub highest_tier: ToolOutputTier,
}

fn normalize_tool_path(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("./")
        .trim_start_matches(".\\")
        .replace('\\', "/")
}

fn extract_call_str_arg(args_json: &str, key: &str) -> Option<String> {
    let parsed: JsonValue = serde_json::from_str(args_json).ok()?;
    parsed
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn extract_call_u64_arg(args_json: &str, key: &str) -> Option<u64> {
    let parsed: JsonValue = serde_json::from_str(args_json).ok()?;
    parsed.get(key).and_then(|v| v.as_u64())
}

fn extract_call_path(args_json: &str) -> Option<String> {
    extract_call_str_arg(args_json, "path").map(|p| normalize_tool_path(&p))
}

/// Identifica observaciones `fs_read` invalidadas por un `fs_edit` / `fs_write` posterior
/// sobre el mismo archivo dentro de `memory`.
pub(crate) fn find_superseded_reads(memory: &[(String, String)]) -> HashMap<usize, String> {
    let mut read_observe_indices: HashMap<String, Vec<usize>> = HashMap::new();
    let mut superseded: HashMap<usize, String> = HashMap::new();
    let mut last_call: Option<(String, Option<String>)> = None;

    for (idx, (kind, text)) in memory.iter().enumerate() {
        match kind.as_str() {
            "call" => {
                let (tool, args) = match text.split_once(' ') {
                    Some((t, a)) => (t, a),
                    None => (text.as_str(), "{}"),
                };
                last_call = Some((tool.to_string(), extract_call_path(args)));
            }
            "observe" => {
                if let Some((tool, Some(path))) = last_call.take() {
                    if tool == "fs_read" {
                        read_observe_indices.entry(path).or_default().push(idx);
                    } else if matches!(tool.as_str(), "fs_write" | "fs_edit") {
                        if let Some(indices) = read_observe_indices.remove(&path) {
                            for old_idx in indices {
                                superseded.insert(old_idx, path.clone());
                            }
                        }
                    }
                }
            }
            "tool_error" | "denied" => {
                last_call = None;
            }
            _ => {}
        }
    }
    superseded
}

fn fnv1a_8hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", (hash ^ (hash >> 32)) as u32)
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn infer_read_line_range(call_args: &str, output: &str) -> Option<(u64, u64)> {
    let arg_start = extract_call_u64_arg(call_args, "start_line");
    let arg_end = extract_call_u64_arg(call_args, "end_line");
    if let (Some(a), Some(b)) = (arg_start, arg_end) {
        return Some((a, b));
    }
    let mut first: Option<u64> = None;
    let mut last: Option<u64> = None;
    for line in output.lines() {
        if let Some((num_part, _)) = line.split_once(':') {
            if let Ok(n) = num_part.trim().parse::<u64>() {
                if first.is_none() {
                    first = Some(n);
                }
                last = Some(n);
            }
        }
    }
    match (arg_start.or(first), arg_end.or(last)) {
        (Some(a), Some(b)) => Some((a, b)),
        (Some(a), None) => Some((a, a)),
        _ => None,
    }
}

/// Genera un resumen estructural determinista Tier 1 para una salida antigua de herramienta,
/// conservando coordenadas verificables (ruta, rango de líneas, símbolos/hits, exit code)
/// sin incluir ticks ni índices relativos para preservar estabilidad de Prompt Caching.
pub fn summarize_tool_output_tier1(tool_name: &str, call_args: &str, output: &str) -> String {
    let bytes = output.len();
    match tool_name {
        "fs_read" => {
            let path = extract_call_path(call_args).unwrap_or_else(|| "?".to_string());
            let sha = fnv1a_8hex(output.as_bytes());
            if let Some((a, b)) = infer_read_line_range(call_args, output) {
                format!("[summary fs_read {path} lines {a}..{b} ({bytes}B, sha:{sha})]")
            } else {
                let lines = output.lines().count();
                format!("[summary fs_read {path} ({lines} lines, {bytes}B, sha:{sha})]")
            }
        }
        "scent_outline" | "scent_impact" => {
            let target = extract_call_str_arg(call_args, "path")
                .or_else(|| extract_call_str_arg(call_args, "target"))
                .unwrap_or_else(|| ".".to_string());
            let mut total: Option<usize> = None;
            let mut row_count = 0usize;
            let mut top_symbols: Vec<String> = Vec::new();
            for line in output.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with("@TOON") {
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("# total:") {
                    total = rest.trim().parse::<usize>().ok();
                    continue;
                }
                if trimmed.starts_with('#') {
                    continue;
                }
                row_count += 1;
                if top_symbols.len() < 5 {
                    let sym = trimmed
                        .split('|')
                        .next()
                        .map(str::trim)
                        .unwrap_or("")
                        .to_string();
                    if !sym.is_empty() && !top_symbols.contains(&sym) {
                        top_symbols.push(sym);
                    }
                }
            }
            let total_n = total.unwrap_or(row_count);
            let top = if top_symbols.is_empty() {
                "-".to_string()
            } else {
                top_symbols.join(", ")
            };
            format!("[summary {tool_name} {target}: {total_n} symbols, top: {top}]")
        }
        "code_search" => {
            let pat = extract_call_str_arg(call_args, "pattern").unwrap_or_default();
            let mut hits = 0usize;
            let mut files = BTreeSet::new();
            for line in output.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with("...[") || trimmed == "--" {
                    continue;
                }
                if let Some((file, rest)) = trimmed.split_once(':') {
                    if rest
                        .split_once(':')
                        .and_then(|(ln, _)| ln.trim().parse::<u64>().ok())
                        .is_some()
                    {
                        hits += 1;
                        files.insert(file.trim().to_string());
                    }
                }
            }
            format!(
                "[summary code_search '{}': {hits} hits in {} files]",
                truncate_chars(&pat, 48),
                files.len()
            )
        }
        "glob_files" => {
            let pat = extract_call_str_arg(call_args, "pattern").unwrap_or_default();
            let files: Vec<&str> = output
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with("...") && *l != "no files matched")
                .collect();
            let top: Vec<&str> = files.iter().take(5).copied().collect();
            if files.is_empty() {
                format!(
                    "[summary glob_files '{}': 0 files]",
                    truncate_chars(&pat, 48)
                )
            } else {
                format!(
                    "[summary glob_files '{}': {} files (top: {})]",
                    truncate_chars(&pat, 48),
                    files.len(),
                    truncate_chars(&top.join(", "), 180)
                )
            }
        }
        "service_status" | "service_start" | "service_stop" => {
            let id = extract_call_str_arg(call_args, "id").unwrap_or_else(|| "all".to_string());
            let first = output
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && *l != "@TOON v1" && !l.starts_with('#'))
                .unwrap_or("ok");
            format!(
                "[summary {tool_name} ({id}): {}]",
                truncate_chars(first, 140)
            )
        }
        "service_logs" => {
            let id = extract_call_str_arg(call_args, "id").unwrap_or_default();
            let non_empty: Vec<&str> = output
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            let tail_start = non_empty.len().saturating_sub(4);
            let tail = truncate_chars(&non_empty[tail_start..].join(" | "), 220);
            format!(
                "[summary service_logs '{id}' ({} lines, {bytes}B): {tail}]",
                non_empty.len()
            )
        }
        "service_exec" => {
            let id = extract_call_str_arg(call_args, "id").unwrap_or_default();
            let cmd = extract_call_str_arg(call_args, "command").unwrap_or_default();
            let exit_code = extract_exit_code(output).unwrap_or_else(|| "?".to_string());
            format!(
                "[summary service_exec '{id}' '{}' exit={exit_code} ({bytes}B)]",
                truncate_chars(&cmd, 64)
            )
        }
        "shell_exec" => {
            let cmd = extract_call_str_arg(call_args, "command").unwrap_or_default();
            let exit_code = extract_exit_code(output).unwrap_or_else(|| "?".to_string());
            let non_empty: Vec<&str> = output
                .lines()
                .map(str::trim)
                .filter(|l| {
                    !l.is_empty() && !l.starts_with("exit_code:") && !l.starts_with("exit=")
                })
                .collect();
            let tail_start = non_empty.len().saturating_sub(5);
            let tail = truncate_chars(&non_empty[tail_start..].join(" | "), 240);
            format!(
                "[summary shell_exec '{}' exit={exit_code} ({bytes}B): {tail}]",
                truncate_chars(&cmd, 64)
            )
        }
        _ => {
            let lines = output.lines().count();
            let preview = output
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(|l| truncate_chars(l, 120))
                .unwrap_or_default();
            format!("[summary {tool_name} ({lines} lines, {bytes}B): {preview}]")
        }
    }
}

fn extract_exit_code(output: &str) -> Option<String> {
    for line in output.lines().take(5) {
        let lower = line.trim().to_ascii_lowercase();
        for prefix in ["exit_code:", "exit_code=", "exit:", "exit=", "status="] {
            if let Some(rest) = lower.strip_prefix(prefix) {
                let token = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches([',', ';', ')']);
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }
    None
}

/// Construye un resumen determinista estructurado para `/compact` o auto-compactación Tier 2,
/// preservando hechos verificables: objetivo, plan activo, archivos leídos/modificados,
/// errores recientes, últimas conclusiones y directiva opcional del usuario.
pub fn build_compaction_summary(
    goal: &str,
    memory: &[(String, String)],
    active_plan: Option<&str>,
    focus: Option<&str>,
) -> String {
    let start_idx = memory
        .iter()
        .rposition(|(k, t)| k == "system" && t.starts_with(COMPACT_MARKER))
        .unwrap_or(0);

    let mut files_read = BTreeSet::new();
    let mut files_modified = BTreeSet::new();
    let mut files_found = BTreeSet::new();
    let mut tools_used = BTreeSet::new();
    let mut recent_errors: Vec<String> = Vec::new();
    let mut recent_says: Vec<String> = Vec::new();
    let mut user_prompts: Vec<String> = Vec::new();
    let mut prior_summary: Option<String> = None;
    let mut last_call_tool: Option<String> = None;

    let focus_keywords = focus.map(extract_focus_keywords).unwrap_or_default();
    let mut focus_candidates: Vec<(usize, usize, String)> = Vec::new();

    for (idx, (kind, text)) in memory.iter().enumerate().skip(start_idx) {
        match kind.as_str() {
            "system" if idx == start_idx && text.starts_with(COMPACT_MARKER) => {
                if let Some(rest) = text.strip_prefix(COMPACT_MARKER) {
                    prior_summary = Some(truncate_chars(rest.trim(), 600));
                }
            }
            "user" => {
                user_prompts.push(truncate_chars(text.trim(), 160));
            }
            "call" => {
                let (tool, args) = match text.split_once(' ') {
                    Some((t, a)) => (t, a),
                    None => (text.as_str(), "{}"),
                };
                tools_used.insert(tool.to_string());
                last_call_tool = Some(tool.to_string());
                if let Some(path) = extract_call_path(args) {
                    match tool {
                        "fs_read" => {
                            files_read.insert(path);
                        }
                        "fs_write" | "fs_edit" => {
                            files_modified.insert(path);
                        }
                        _ => {}
                    }
                }
            }
            "observe" => {
                if last_call_tool.as_deref() == Some("code_search") {
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if let Some((file, rest)) = trimmed.split_once(':') {
                            if rest
                                .split_once(':')
                                .and_then(|(ln, _)| ln.trim().parse::<u64>().ok())
                                .is_some()
                            {
                                files_found.insert(file.trim().to_string());
                            }
                        }
                    }
                }
                if !focus_keywords.is_empty() {
                    if let Some((score, snippet)) =
                        extract_focus_snippet(text, &focus_keywords, 280)
                    {
                        let label = last_call_tool.as_deref().unwrap_or("observe");
                        focus_candidates.push((score, idx, format!("[{label}] {snippet}")));
                    }
                }
            }
            "tool_error" | "denied" => {
                let trimmed = text.trim();
                recent_errors.push(truncate_chars(trimmed, 180));
                if !focus_keywords.is_empty() {
                    if let Some((score, snippet)) =
                        extract_focus_snippet(trimmed, &focus_keywords, 220)
                    {
                        focus_candidates.push((score, idx, format!("[{kind}] {snippet}")));
                    }
                }
            }
            "say" => {
                let trimmed = text.trim();
                recent_says.push(truncate_chars(trimmed, 240));
                if !focus_keywords.is_empty() {
                    if let Some((score, snippet)) =
                        extract_focus_snippet(trimmed, &focus_keywords, 280)
                    {
                        focus_candidates.push((score, idx, format!("[say] {snippet}")));
                    }
                }
            }
            _ => {}
        }
    }

    let mut parts = Vec::new();
    parts.push(format!("goal: {}", truncate_chars(goal.trim(), 240)));
    if let Some(f) = focus.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("focus: {f}"));
    }
    if let Some(prev) = prior_summary {
        parts.push(format!("prior_summary: {prev}"));
    }
    if !user_prompts.is_empty() {
        let keep = user_prompts.len().saturating_sub(3);
        parts.push(format!(
            "recent_user_turns: {}",
            user_prompts[keep..].join(" | ")
        ));
    }
    if !files_modified.is_empty() {
        parts.push(format!(
            "files_modified: {}",
            files_modified.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if !files_read.is_empty() {
        parts.push(format!(
            "files_read: {}",
            files_read.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if !files_found.is_empty() {
        let top_found: Vec<String> = files_found.into_iter().take(12).collect();
        parts.push(format!("files_found: {}", top_found.join(", ")));
    }
    if !tools_used.is_empty() {
        parts.push(format!(
            "tools_used: {}",
            tools_used.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if let Some(plan) = active_plan.map(str::trim).filter(|p| !p.is_empty()) {
        parts.push(format!("active_plan:\n{plan}"));
    }
    if !focus_candidates.is_empty() {
        // Prioriza mayor coincidencia de palabras clave y luego orden cronológico reciente.
        focus_candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        let mut chosen: Vec<(usize, String)> = Vec::new();
        let mut seen_snippets = BTreeSet::new();
        for (_, idx, snippet) in focus_candidates {
            if seen_snippets.insert(snippet.clone()) {
                chosen.push((idx, snippet));
                if chosen.len() >= 4 {
                    break;
                }
            }
        }
        chosen.sort_by_key(|(idx, _)| *idx);
        let bullets: Vec<String> = chosen.into_iter().map(|(_, s)| format!("- {s}")).collect();
        parts.push(format!("focus_matches:\n{}", bullets.join("\n")));
    }
    if !recent_errors.is_empty() {
        let keep = recent_errors.len().saturating_sub(3);
        parts.push(format!(
            "pending_errors: {}",
            recent_errors[keep..].join(" | ")
        ));
    }
    if !recent_says.is_empty() {
        let keep = recent_says.len().saturating_sub(2);
        parts.push(format!(
            "latest_conclusions: {}",
            recent_says[keep..].join(" | ")
        ));
    }
    parts.join("\n")
}

fn extract_focus_keywords(focus: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "para",
        "como",
        "sobre",
        "conservar",
        "guardar",
        "mantener",
        "resumen",
        "ubicacion",
        "ubicación",
        "archivo",
        "archivos",
        "donde",
        "dónde",
        "esta",
        "está",
        "keep",
        "with",
        "from",
        "that",
        "this",
        "about",
        "focus",
        "summary",
        "details",
        "preserve",
        "file",
        "files",
    ];
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for raw in focus.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.')) {
        let lower = raw.trim_matches('.').to_ascii_lowercase();
        if lower.chars().count() < 4 || STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        if seen.insert(lower.clone()) {
            out.push(lower);
        }
    }
    out
}

fn extract_focus_snippet(
    text: &str,
    keywords: &[String],
    max_chars: usize,
) -> Option<(usize, String)> {
    let lower_full = text.to_ascii_lowercase();
    let total_score = keywords
        .iter()
        .filter(|kw| lower_full.contains(kw.as_str()))
        .count();
    if total_score == 0 {
        return None;
    }

    // Extrae las líneas que realmente coinciden con el foco (más denso que truncar desde el inicio).
    let mut matched_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "--" {
            continue;
        }
        let lower_line = trimmed.to_ascii_lowercase();
        if keywords.iter().any(|kw| lower_line.contains(kw.as_str())) {
            matched_lines.push(trimmed);
            if matched_lines.len() >= 3 {
                break;
            }
        }
    }
    let body = if matched_lines.is_empty() {
        text.trim().to_string()
    } else {
        matched_lines.join(" | ")
    };
    Some((total_score, truncate_chars(&body, max_chars)))
}

/// Proyecta `(goal, memory)` aplicando:
/// 1. Frontera Tier 2 (`/compact`) si existe un marcador `[/compact] ` en `memory`.
/// 2. Invalidación `superseded` de `fs_read` tras `fs_edit`/`fs_write`.
/// 3. Retención Tier 0 en las últimas `keep_recent_tool_outputs` observaciones y degradación
///    determinista Tier 1 en las anteriores (preservando estabilidad del prefijo para Prompt Cache).
/// 4. Auto-compactación Tier 2 si se supera `auto_compact_ratio` de la ventana del modelo.
pub fn project_memory(
    goal: &str,
    memory: &[(String, String)],
    cfg: &TieredContextConfig,
) -> ProjectedMemory {
    let compact_pos = memory
        .iter()
        .rposition(|(k, t)| k == "system" && t.starts_with(COMPACT_MARKER));

    let (active_slice, mut turns, mut compacted, mut call_index) = match compact_pos {
        Some(idx) => {
            let summary_body = memory[idx]
                .1
                .strip_prefix(COMPACT_MARKER)
                .unwrap_or(&memory[idx].1)
                .trim();
            let initial = vec![ProjectedTurn::User(format!(
                "[session_summary]\n{summary_body}"
            ))];
            let prior_calls = memory[..=idx].iter().filter(|(k, _)| k == "call").count();
            (&memory[idx + 1..], initial, true, prior_calls)
        }
        None => (
            memory,
            vec![ProjectedTurn::User(format!("Goal: {goal}"))],
            false,
            0usize,
        ),
    };

    let superseded = find_superseded_reads(active_slice);
    let mut observe_meta: Vec<(usize, String, String, String)> = Vec::new();
    let mut last_tool: Option<(String, String, String)> = None;

    for (mem_idx, (kind, text)) in active_slice.iter().enumerate() {
        match kind.as_str() {
            "user" | "system" => turns.push(ProjectedTurn::User(text.clone())),
            "say" => turns.push(ProjectedTurn::AssistantText(text.clone())),
            "call" => {
                let (tool, args) = match text.split_once(' ') {
                    Some((t, a)) => (t, a),
                    None => (text.as_str(), "{}"),
                };
                let args_norm = if args.trim().is_empty() { "{}" } else { args };
                let call_id = format!("call_{call_index}");
                last_tool = Some((tool.to_string(), call_id.clone(), args_norm.to_string()));
                turns.push(ProjectedTurn::ToolCall {
                    call_index,
                    call_id,
                    name: tool.to_string(),
                    args_json: args_norm.to_string(),
                });
                call_index += 1;
            }
            "observe" => {
                if let Some((name, call_id, call_args)) = last_tool.take() {
                    if let Some(sup_path) = superseded.get(&mem_idx) {
                        let marker = format!(
                            "[superseded: prior fs_read of `{sup_path}` was invalidated by a subsequent edit]"
                        );
                        turns.push(ProjectedTurn::ToolObserve {
                            call_id: Some(call_id),
                            name,
                            content: marker,
                            tier: ToolOutputTier::Tier1Summary,
                            is_toon: false,
                            is_superseded: true,
                        });
                    } else {
                        let is_toon = text.trim_start().starts_with("@TOON v1");
                        let turn_idx = turns.len();
                        observe_meta.push((turn_idx, name.clone(), call_args, text.clone()));
                        turns.push(ProjectedTurn::ToolObserve {
                            call_id: Some(call_id),
                            name,
                            content: text.clone(),
                            tier: ToolOutputTier::Tier0Full,
                            is_toon,
                            is_superseded: false,
                        });
                    }
                } else {
                    turns.push(ProjectedTurn::User(text.clone()));
                }
            }
            "tool_error" => {
                if let Some((name, call_id, _)) = last_tool.take() {
                    turns.push(ProjectedTurn::ToolError {
                        call_id: Some(call_id),
                        name,
                        error: text.clone(),
                    });
                } else {
                    turns.push(ProjectedTurn::ToolError {
                        call_id: None,
                        name: "tool".to_string(),
                        error: text.clone(),
                    });
                }
            }
            "denied" => {
                if let Some((name, call_id, _)) = last_tool.take() {
                    turns.push(ProjectedTurn::ToolDenied {
                        call_id: Some(call_id),
                        name,
                        reason: text.clone(),
                    });
                } else {
                    turns.push(ProjectedTurn::ToolDenied {
                        call_id: None,
                        name: "tool".to_string(),
                        reason: text.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    // 1. Degradar a Tier 1 las observaciones fuera de las últimas `keep_recent_tool_outputs`.
    let keep_recent = cfg.keep_recent_tool_outputs;
    let tier1_cutoff = observe_meta.len().saturating_sub(keep_recent);
    for (turn_idx, tool_name, call_args, raw_text) in observe_meta.iter().take(tier1_cutoff) {
        let summary = summarize_tool_output_tier1(tool_name, call_args, raw_text);
        if summary.len() < raw_text.len() {
            if let Some(ProjectedTurn::ToolObserve { content, tier, .. }) = turns.get_mut(*turn_idx)
            {
                *content = summary;
                *tier = ToolOutputTier::Tier1Summary;
            }
        }
    }

    // 2. Si aún se supera `soft_char_budget`, resumir observaciones Tier 0 más antiguas
    //    (primero no-TOON y después TOON) protegiendo errores y el tail inmediato.
    let mut total_bytes: usize = turns.iter().map(ProjectedTurn::byte_len).sum();
    if total_bytes > cfg.soft_char_budget {
        let protected_tail_turn = turns.len().saturating_sub(2);
        for pass_toon in [false, true] {
            for (turn_idx, tool_name, call_args, raw_text) in &observe_meta {
                if total_bytes <= cfg.soft_char_budget {
                    break;
                }
                if *turn_idx >= protected_tail_turn {
                    continue;
                }
                if let Some(ProjectedTurn::ToolObserve {
                    content,
                    tier,
                    is_toon,
                    is_superseded,
                    ..
                }) = turns.get_mut(*turn_idx)
                {
                    if *tier != ToolOutputTier::Tier0Full || *is_superseded || *is_toon != pass_toon
                    {
                        continue;
                    }
                    let summary = summarize_tool_output_tier1(tool_name, call_args, raw_text);
                    if summary.len() < content.len() {
                        total_bytes = total_bytes - content.len() + summary.len();
                        *content = summary;
                        *tier = ToolOutputTier::Tier1Summary;
                    }
                }
            }
        }
    }

    // 3. Auto-compactación Tier 2 si se supera `auto_compact_ratio` de la ventana del modelo.
    let auto_char_limit = ((cfg.model_context_tokens as f64)
        * (cfg.auto_compact_ratio.clamp(0.1, 1.0) as f64)
        * 4.0) as usize;
    if auto_char_limit > 0 && total_bytes > auto_char_limit && active_slice.len() > 6 {
        let keep_slice = active_slice.len().saturating_sub(4);
        let prefix_summary =
            build_compaction_summary(goal, &active_slice[..keep_slice], None, None);
        let mut compacted_turns = vec![ProjectedTurn::User(format!(
            "[session_summary]\n{prefix_summary}"
        ))];
        let mut tail_turns = project_memory(
            goal,
            &active_slice[keep_slice..],
            &TieredContextConfig {
                auto_compact_ratio: 1.0,
                ..*cfg
            },
        )
        .turns;
        if !tail_turns.is_empty() {
            // Saltar `Goal:` redundante del tail.
            tail_turns.remove(0);
        }
        compacted_turns.extend(tail_turns);
        turns = compacted_turns;
        total_bytes = turns.iter().map(ProjectedTurn::byte_len).sum();
        compacted = true;
    }

    let highest_tier = if compacted {
        ToolOutputTier::Tier2Compacted
    } else if turns.iter().any(|t| {
        matches!(
            t,
            ProjectedTurn::ToolObserve {
                tier: ToolOutputTier::Tier1Summary,
                ..
            }
        )
    }) {
        ToolOutputTier::Tier1Summary
    } else {
        ToolOutputTier::Tier0Full
    };

    ProjectedMemory {
        turns,
        total_bytes,
        highest_tier,
    }
}

/// Uso de tokens normalizado por turno (`REQ-01.4`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

impl TokenUsage {
    pub fn add_assign(&mut self, other: TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }
}

/// Telemetría acumulada de la sesión para renderizado en la TUI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionTelemetry {
    pub last_turn: TokenUsage,
    pub cumulative: TokenUsage,
    pub estimated_cost_usd: f64,
    pub context_fill_pct: u8,
    pub model_context_tokens: usize,
    pub model: String,
    pub is_local_endpoint: bool,
}

impl SessionTelemetry {
    pub fn new(model: impl Into<String>, is_local_endpoint: bool) -> Self {
        let model = model.into();
        let model_context_tokens = model_context_window(&model);
        Self {
            last_turn: TokenUsage::default(),
            cumulative: TokenUsage::default(),
            estimated_cost_usd: 0.0,
            context_fill_pct: 0,
            model_context_tokens,
            model,
            is_local_endpoint,
        }
    }

    /// `SDD-03` Fase 2: conmuta el modelo activo en caliente (cambio de perfil entre
    /// fronteras de tick). Rederiva la ventana de contexto y recalcula
    /// `context_fill_pct` desde el último turno registrado.
    pub fn set_active_model(&mut self, model: &str, is_local_endpoint: bool) {
        if self.model == model && self.is_local_endpoint == is_local_endpoint {
            return;
        }
        self.model = model.to_string();
        self.is_local_endpoint = is_local_endpoint;
        self.model_context_tokens = model_context_window(model);
        if self.model_context_tokens == 0 || self.last_turn.input_tokens == 0 {
            self.context_fill_pct = 0;
        } else {
            let pct = ((self.last_turn.input_tokens as f64) / (self.model_context_tokens as f64)
                * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8;
            self.context_fill_pct = pct;
        }
    }

    /// `SDD-03` Fase 2: registra un turno con el modelo/endpoint **de esa llamada**
    /// (el modelo en vuelo puede diferir del del handle si hubo hot-swap antes de que
    /// llegara la respuesta): coste estimado por el modelo real de la llamada.
    pub fn record_turn_for(&mut self, model: &str, is_local_endpoint: bool, usage: TokenUsage) {
        self.set_active_model(model, is_local_endpoint);
        self.last_turn = usage;
        self.cumulative.add_assign(usage);
        if !is_local_endpoint {
            self.estimated_cost_usd += estimate_usage_cost_usd(model, usage);
        }
        if self.model_context_tokens > 0 && usage.input_tokens > 0 {
            let pct = ((usage.input_tokens as f64) / (self.model_context_tokens as f64) * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8;
            self.context_fill_pct = pct;
        }
    }

    /// Atajo legacy (tests): registra con el modelo de arranque del handle.
    #[cfg(test)]
    pub fn record_turn(&mut self, usage: TokenUsage) {
        let model = self.model.clone();
        let is_local = self.is_local_endpoint;
        self.record_turn_for(&model, is_local, usage);
    }

    pub fn update_estimated_context_bytes(&mut self, projected_bytes: usize) {
        if self.model_context_tokens == 0 {
            self.context_fill_pct = 0;
            return;
        }
        let est_tokens = projected_bytes.div_ceil(4) as f64;
        let pct = ((est_tokens / (self.model_context_tokens as f64)) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8;
        self.context_fill_pct = pct;
    }

    /// Indicador compacto para la barra de estado de la TUI (SDD-01 §3.2):
    /// `ctx: 34% · in: 18.4k (cache 12.1k) · out: 1.9k · ~$0.024`
    pub fn format_status_bar(&self) -> String {
        let mut out = if self.context_fill_pct == 0
            && (self.last_turn.input_tokens > 0 || self.cumulative.input_tokens > 0)
        {
            "ctx: <1%".to_string()
        } else {
            format!("ctx: {}%", self.context_fill_pct)
        };
        if self.cumulative.input_tokens > 0 || self.cumulative.output_tokens > 0 {
            let in_s = format_token_count(self.cumulative.input_tokens);
            if self.cumulative.cached_input_tokens > 0 {
                let cache_s = format_token_count(self.cumulative.cached_input_tokens);
                out.push_str(&format!(" · in: {in_s} (cache {cache_s})"));
            } else {
                out.push_str(&format!(" · in: {in_s}"));
            }
            out.push_str(&format!(
                " · out: {}",
                format_token_count(self.cumulative.output_tokens)
            ));
            if self.cumulative.reasoning_tokens > 0 {
                out.push_str(&format!(
                    " · think: {}",
                    format_token_count(self.cumulative.reasoning_tokens)
                ));
            }
            if !self.is_local_endpoint {
                out.push_str(&format!(" · ~${:.3}", self.estimated_cost_usd));
            }
        }
        out
    }
}

/// Handle thread-safe de telemetría efímera (fuera del grafo determinista).
#[derive(Clone, Default)]
pub struct SessionTelemetryHandle {
    inner: Arc<Mutex<SessionTelemetry>>,
    dirty: Arc<AtomicBool>,
}

impl SessionTelemetryHandle {
    pub fn new(model: impl Into<String>, is_local_endpoint: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SessionTelemetry::new(model, is_local_endpoint))),
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `SDD-03` Fase 2: conmuta el modelo activo en caliente (hot-swap LLM). Marca
    /// `dirty` solo si realmente cambió (refresco de cabecera TUI/Web sin polling).
    pub fn set_active_model(&self, model: &str, is_local_endpoint: bool) {
        if let Ok(mut guard) = self.inner.lock() {
            let changed = guard.model != model || guard.is_local_endpoint != is_local_endpoint;
            guard.set_active_model(model, is_local_endpoint);
            if changed {
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    /// `SDD-03` Fase 2: registra un turno con el modelo/endpoint de esa llamada.
    pub fn record_turn_for(&self, model: &str, is_local_endpoint: bool, usage: TokenUsage) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.record_turn_for(model, is_local_endpoint, usage);
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// Atajo legacy (tests): registra con el modelo de arranque del handle.
    #[cfg(test)]
    pub fn record_turn(&self, usage: TokenUsage) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.record_turn(usage);
            self.dirty.store(true, Ordering::Release);
        }
    }

    pub fn sync_estimated_context_bytes(&self, projected_bytes: usize) {
        if let Ok(mut guard) = self.inner.lock() {
            let prev = guard.context_fill_pct;
            guard.update_estimated_context_bytes(projected_bytes);
            if guard.context_fill_pct != prev {
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    pub fn snapshot(&self) -> SessionTelemetry {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}

pub fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", (n as f64) / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", (n as f64) / 1_000.0)
    } else {
        n.to_string()
    }
}

pub fn model_context_window(model: &str) -> usize {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("gemini-") || lower.starts_with("gpt-4.1") {
        1_000_000
    } else if lower.starts_with("gpt-4o") || lower.starts_with("o1") || lower.starts_with("o3") {
        128_000
    } else {
        DEFAULT_MODEL_CONTEXT_TOKENS
    }
}

/// Estima el coste en USD de un turno a partir de tarifas aproximadas por millón de tokens.
pub fn estimate_usage_cost_usd(model: &str, usage: TokenUsage) -> f64 {
    let lower = model.to_ascii_lowercase();
    // (input $/1M, cached_input $/1M, output $/1M)
    let (in_rate, cache_rate, out_rate) = if lower.contains("flash-lite") {
        (0.075, 0.01875, 0.30)
    } else if lower.contains("flash") {
        (0.15, 0.0375, 0.60)
    } else if lower.starts_with("gemini-") {
        (1.25, 0.3125, 5.00)
    } else if lower.contains("gpt-4o-mini") {
        (0.15, 0.075, 0.60)
    } else if lower.starts_with("gpt-4o") || lower.starts_with("gpt-4.1") {
        (2.50, 1.25, 10.00)
    } else if lower.starts_with("o1") || lower.starts_with("o3") {
        (3.00, 1.50, 12.00)
    } else {
        return 0.0;
    };
    let uncached_in = usage.input_tokens.saturating_sub(usage.cached_input_tokens);
    let billed_out = usage.output_tokens.saturating_add(usage.reasoning_tokens);
    (uncached_in as f64 / 1_000_000.0) * in_rate
        + (usage.cached_input_tokens as f64 / 1_000_000.0) * cache_rate
        + (billed_out as f64 / 1_000_000.0) * out_rate
}

/// Extrae `TokenUsage` desde el objeto `usageMetadata` de Gemini (REST o SSE).
pub fn extract_gemini_usage(json: &JsonValue) -> Option<TokenUsage> {
    let meta = json.get("usageMetadata")?;
    let input_tokens = meta.get("promptTokenCount").and_then(|v| v.as_u64())?;
    let cached_input_tokens = meta
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = meta
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning_tokens = meta
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_tokens,
    })
}

/// Extrae `TokenUsage` desde el objeto `usage` de OpenAI (`/v1/chat/completions` o SSE).
pub fn extract_openai_usage(json: &JsonValue) -> Option<TokenUsage> {
    let usage = json.get("usage")?;
    if usage.is_null() {
        return None;
    }
    let input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64())?;
    let cached_input_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("cached_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning_tokens = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiered_context_preserves_last_10_and_summarizes_older_tool_outputs() {
        // TEST-01.1: 12 consecutive tool calls -> last 10 remain intact (Tier 0),
        // oldest 2 become deterministic Tier 1 summaries retaining path/lines/symbols/hits/exit code.
        let mut memory: Vec<(String, String)> = Vec::new();
        // Call 1 (oldest -> Tier 1): fs_read
        memory.push((
            "call".to_string(),
            r#"fs_read {"path":"src/auth.rs","start_line":10,"end_line":40}"#.to_string(),
        ));
        memory.push((
            "observe".to_string(),
            format!(
                "10: fn authenticate() {{}}\n{}",
                "11: // padding line\n".repeat(20)
            ),
        ));
        // Call 2 (2nd oldest -> Tier 1): code_search
        memory.push((
            "call".to_string(),
            r#"code_search {"pattern":"TokenUsage"}"#.to_string(),
        ));
        memory.push((
            "observe".to_string(),
            format!(
                "src/context.rs:10: pub struct TokenUsage\nsrc/tui.rs:42: telemetry: TokenUsage\n{}",
                "src/other.rs:99: TokenUsage ref\n".repeat(12)
            ),
        ));
        // Calls 3..=12 (10 recent -> Tier 0 intact)
        for i in 3..=12 {
            if i == 3 {
                memory.push((
                    "call".to_string(),
                    r#"scent_outline {"path":"src/lib.rs"}"#.to_string(),
                ));
                memory.push((
                    "observe".to_string(),
                    "@TOON v1\n# cols: sym | kind | range\nalpha | fn | 1..10\n# total: 1\n"
                        .to_string(),
                ));
            } else {
                memory.push((
                    "call".to_string(),
                    format!(r#"fs_read {{"path":"src/mod_{i}.rs"}}"#),
                ));
                memory.push((
                    "observe".to_string(),
                    format!("1: pub fn recent_{i}() {{}} // RECENT_BODY_{i}"),
                ));
            }
        }

        let cfg = TieredContextConfig::default();
        let projected = project_memory("audit auth", &memory, &cfg);
        let observes: Vec<(&String, &String, ToolOutputTier)> = projected
            .turns
            .iter()
            .filter_map(|t| match t {
                ProjectedTurn::ToolObserve {
                    name,
                    content,
                    tier,
                    ..
                } => Some((name, content, *tier)),
                _ => None,
            })
            .collect();

        assert_eq!(observes.len(), 12);
        // Oldest 2 downgraded to Tier 1 with preserved coordinates
        assert_eq!(observes[0].2, ToolOutputTier::Tier1Summary);
        assert!(
            observes[0]
                .1
                .contains("[summary fs_read src/auth.rs lines 10..40")
                && observes[0].1.contains("sha:"),
            "{}",
            observes[0].1
        );
        assert_eq!(observes[1].2, ToolOutputTier::Tier1Summary);
        assert!(
            observes[1]
                .1
                .contains("[summary code_search 'TokenUsage': 14 hits in 3 files]"),
            "{}",
            observes[1].1
        );
        // Last 10 preserved intact at Tier 0
        for (idx, (_, content, tier)) in observes.iter().enumerate().skip(2) {
            assert_eq!(
                *tier,
                ToolOutputTier::Tier0Full,
                "index {idx} must be Tier0"
            );
            if idx == 2 {
                assert!(content.contains("@TOON v1"));
            } else {
                assert!(content.contains(&format!("RECENT_BODY_{}", idx + 1)));
            }
        }

        // Prefix stability: adding a 13th call leaves summaries of #1 and #2 byte-identical.
        let mut memory_next = memory.clone();
        memory_next.push((
            "call".to_string(),
            r#"shell_exec {"command":"cargo test"}"#.to_string(),
        ));
        memory_next.push((
            "observe".to_string(),
            "exit_code: 0\ntest result: ok. 10 passed".to_string(),
        ));
        let projected_next = project_memory("audit auth", &memory_next, &cfg);
        let next_observes: Vec<&String> = projected_next
            .turns
            .iter()
            .filter_map(|t| match t {
                ProjectedTurn::ToolObserve { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(next_observes[0], observes[0].1);
        assert_eq!(next_observes[1], observes[1].1);

        // Verify Tier 1 formatting for scent_outline and shell_exec directly.
        let scent_sum = summarize_tool_output_tier1(
            "scent_outline",
            r#"{"path":"onyx-core"}"#,
            "@TOON v1\n# cols: sym | kind\nGraph | trait\nEventBus | trait\n# total: 2\n",
        );
        assert_eq!(
            scent_sum,
            "[summary scent_outline onyx-core: 2 symbols, top: Graph, EventBus]"
        );
        let shell_sum = summarize_tool_output_tier1(
            "shell_exec",
            r#"{"command":"cargo test"}"#,
            "exit_code: 101\nrunning 2 tests\ntest a ... ok\ntest b ... FAILED\n",
        );
        assert!(
            shell_sum.contains("[summary shell_exec 'cargo test' exit=101")
                && shell_sum.contains("test b ... FAILED"),
            "{shell_sum}"
        );
    }

    #[test]
    fn telemetry_tracks_usage_cost_and_formats_status_bar() {
        let handle = SessionTelemetryHandle::new("gemini-4-hydrogen", false);
        handle.record_turn(TokenUsage {
            input_tokens: 18_400,
            cached_input_tokens: 12_100,
            output_tokens: 1_900,
            reasoning_tokens: 0,
        });
        assert!(handle.take_dirty());
        assert!(!handle.take_dirty());
        let snap = handle.snapshot();
        assert_eq!(snap.cumulative.input_tokens, 18_400);
        assert_eq!(snap.cumulative.cached_input_tokens, 12_100);
        assert_eq!(snap.cumulative.output_tokens, 1_900);
        assert!(snap.estimated_cost_usd > 0.0);
        let bar = snap.format_status_bar();
        assert!(
            bar.contains("in: 18.4k (cache 12.1k)")
                && bar.contains("out: 1.9k")
                && bar.contains("~$"),
            "{bar}"
        );

        // Small input on a 1M context window formats as `ctx: <1%` instead of `ctx: 0%`.
        let small = SessionTelemetryHandle::new("gemini-4-hydrogen", false);
        small.record_turn(TokenUsage {
            input_tokens: 4_000,
            cached_input_tokens: 0,
            output_tokens: 100,
            reasoning_tokens: 0,
        });
        assert!(
            small.snapshot().format_status_bar().starts_with("ctx: <1%"),
            "{}",
            small.snapshot().format_status_bar()
        );
    }

    /// `SDD-03` Fase 2: `record_turn_for` imputa el coste al modelo **de la llamada**
    /// (el objetivo en vuelo), aunque el modelo activo haya cambiado por hot-swap
    /// antes de que llegara la respuesta. Un endpoint local (Ollama/vLLM) no
    /// acumula coste estimado.
    #[test]
    fn telemetry_record_turn_for_charges_the_call_model_after_hot_swap() {
        // `gpt-4o`: input $2.50/1M, output $10.00/1M, ventana 128k.
        let handle = SessionTelemetryHandle::new("gpt-4o", false);
        handle.record_turn_for(
            "gpt-4o",
            false,
            TokenUsage {
                input_tokens: 128_000,
                cached_input_tokens: 0,
                output_tokens: 100_000,
                reasoning_tokens: 0,
            },
        );
        let snap = handle.snapshot();
        assert_eq!(snap.model, "gpt-4o");
        // 128k in → $0.32; 100k out → $1.00.
        assert!(
            (snap.estimated_cost_usd - 1.32).abs() < 1e-9,
            "{}",
            snap.estimated_cost_usd
        );

        // Hot-swap entre frontera de tick: el activo pasa a Gemini (ventana 1M).
        handle.set_active_model("gemini-2.5-pro", false);
        let swapped = handle.snapshot();
        assert_eq!(swapped.model, "gemini-2.5-pro");
        assert_eq!(swapped.model_context_tokens, 1_000_000);

        // Llega la respuesta de la llamada gpt-4o en vuelo: el coste se imputa a
        // `gpt-4o` (tarifa $1.32), no al modelo activo Gemini ($0.16 + $0.50 = $0.66).
        handle.record_turn_for(
            "gpt-4o",
            false,
            TokenUsage {
                input_tokens: 128_000,
                cached_input_tokens: 0,
                output_tokens: 100_000,
                reasoning_tokens: 0,
            },
        );
        let snap = handle.snapshot();
        // La telemetría se re-sincroniza al modelo de la llamada (auto-corregible:
        // la siguiente respuesta volverá a reflejar el modelo del hot-swap).
        assert_eq!(snap.model, "gpt-4o");
        assert_eq!(snap.cumulative.input_tokens, 256_000);
        assert!(
            (snap.estimated_cost_usd - 2.64).abs() < 1e-9,
            "{}",
            snap.estimated_cost_usd
        );

        // Un endpoint local nunca acumula coste, aunque el modelo sea desconocido.
        handle.record_turn_for(
            "qwen3-local",
            true,
            TokenUsage {
                input_tokens: 5_000,
                cached_input_tokens: 0,
                output_tokens: 100,
                reasoning_tokens: 0,
            },
        );
        let snap = handle.snapshot();
        assert!(snap.is_local_endpoint);
        assert!(
            (snap.estimated_cost_usd - 2.64).abs() < 1e-9,
            "{}",
            snap.estimated_cost_usd
        );
        assert_eq!(snap.cumulative.input_tokens, 261_000);
        // El coste estimado deja de mostrarse en la barra de estado local.
        assert!(
            !snap.format_status_bar().contains("~$"),
            "{}",
            snap.format_status_bar()
        );
    }

    #[test]
    fn compaction_summary_rescues_focus_matches_and_search_files_from_older_turns() {
        let memory = vec![
            (
                "call".to_string(),
                r#"code_search {"pattern":"evaluate_permission","regex":true}"#.to_string(),
            ),
            (
                "observe".to_string(),
                "onyx-workspace/src/policy.rs-485- /// Evalúa reglas\nonyx-workspace/src/policy.rs:487: pub fn evaluate_permission(&self, tool: &str) -> PermissionDecision {\n".to_string(),
            ),
            (
                "say".to_string(),
                "evaluate_permission está definida en onyx-workspace/src/policy.rs:487.".to_string(),
            ),
            ("user".to_string(), "turn 3".to_string()),
            ("say".to_string(), "unrelated answer 3".to_string()),
            ("user".to_string(), "turn 4".to_string()),
            ("say".to_string(), "unrelated answer 4".to_string()),
            ("user".to_string(), "turn 5".to_string()),
            ("say".to_string(), "unrelated answer 5".to_string()),
        ];

        let summary = build_compaction_summary(
            "audit SDD-02",
            &memory,
            None,
            Some("conservar la ubicacion de evaluate_permission"),
        );
        assert!(
            summary.contains("files_found: onyx-workspace/src/policy.rs"),
            "{summary}"
        );
        assert!(
            summary.contains("focus_matches:")
                && summary.contains("onyx-workspace/src/policy.rs:487")
                && summary.contains("evaluate_permission"),
            "{summary}"
        );
    }
}
