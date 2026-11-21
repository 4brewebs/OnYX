//! *Scent-Heated Approval* monótono y extracción de rangos de impacto (`SDD-mix-1` Oleada A).
//!
//! Evalúa en best-effort el radio de impacto AST de mutaciones (`fs_edit`, `fs_write`)
//! usando `onyx-scent` cuando la feature `scent` está habilitada, garantizando siempre:
//! - **Monotonía estricta:** `Allow → Ask` cuando supera el umbral; nunca `Deny → Allow`,
//!   nunca `Deny → Ask`, nunca `Ask → Allow`.
//! - **Degradación elegante:** si `[impact].enabled = false`, falta la feature `scent` o el
//!   archivo no está indexado, se conserva la decisión estática intacta sin bloquear nada.

use std::fs;
use std::path::Path;
#[cfg(feature = "scent")]
use std::sync::Mutex;

use serde_json::Value as JsonValue;

use crate::policy::{PermissionDecision, WorkspacePolicy, strip_toml_comment_pub};

/// Configuración declarativa `[impact]` cargada desde `~/.onyx/config.toml` y
/// `<workspace>/.onyx/config.toml` (`REQ-MIX1.2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactConfig {
    /// Activa el análisis de impacto AST en aprobaciones (default: `true`).
    pub enabled: bool,
    /// Radio máximo de saltos en el grafo de llamadas (default: `2`, acotado `1..=4`).
    pub max_hops: usize,
    /// Umbral de calor (basis points `0..=10_000`) para escalar `Allow → Ask` cuando
    /// el diff toca código `scope:src` público (default: `2_500`).
    pub ask_heat_bp: u32,
    /// Umbral duro de calor para escalar `Allow → Ask` siempre, incluso en símbolos privados
    /// (default: `8_000`).
    pub hard_ask_bp: u32,
}

impl Default for ImpactConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_hops: 2,
            ask_heat_bp: 2_500,
            hard_ask_bp: 8_000,
        }
    }
}

/// Parche parcial leído desde `[impact]` en un archivo `.onyx/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImpactPatch {
    pub enabled: Option<bool>,
    pub max_hops: Option<usize>,
    pub ask_heat_bp: Option<u32>,
    pub hard_ask_bp: Option<u32>,
}

impl ImpactConfig {
    /// Aplica un parche parcial sobre la configuración actual, acotando `max_hops` a `1..=4`.
    pub fn apply_patch(&mut self, patch: ImpactPatch) {
        if let Some(enabled) = patch.enabled {
            self.enabled = enabled;
        }
        if let Some(hops) = patch.max_hops {
            self.max_hops = hops.clamp(1, 4);
        }
        if let Some(ask) = patch.ask_heat_bp {
            self.ask_heat_bp = ask.min(10_000);
        }
        if let Some(hard) = patch.hard_ask_bp {
            self.hard_ask_bp = hard.min(10_000);
        }
    }

    /// Aplica un parche proveniente de un repositorio NO confiado: solo permite endurecer
    /// (`enabled` nunca pasa de `true` a `false`, umbrales solo pueden bajar, `max_hops` solo puede subir).
    pub fn apply_patch_restrictive(&mut self, patch: ImpactPatch) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(enabled) = patch.enabled {
            if enabled {
                self.enabled = true;
            } else if self.enabled {
                warnings.push(
                    "untrusted project `.onyx/config.toml` cannot disable `[impact].enabled`"
                        .to_string(),
                );
            }
        }
        if let Some(hops) = patch.max_hops {
            let clamped = hops.clamp(1, 4);
            if clamped > self.max_hops {
                self.max_hops = clamped;
            }
        }
        if let Some(ask) = patch.ask_heat_bp {
            let clamped = ask.min(10_000);
            match clamped.cmp(&self.ask_heat_bp) {
                std::cmp::Ordering::Less => self.ask_heat_bp = clamped,
                std::cmp::Ordering::Greater => warnings.push(
                    "untrusted project `.onyx/config.toml` cannot raise `[impact].ask_heat_bp`"
                        .to_string(),
                ),
                std::cmp::Ordering::Equal => {}
            }
        }
        if let Some(hard) = patch.hard_ask_bp {
            let clamped = hard.min(10_000);
            match clamped.cmp(&self.hard_ask_bp) {
                std::cmp::Ordering::Less => self.hard_ask_bp = clamped,
                std::cmp::Ordering::Greater => warnings.push(
                    "untrusted project `.onyx/config.toml` cannot raise `[impact].hard_ask_bp`"
                        .to_string(),
                ),
                std::cmp::Ordering::Equal => {}
            }
        }
        warnings
    }

    /// Parsea la sección `[impact]` de un texto TOML ignorando el resto de secciones.
    pub fn parse_toml(input: &str) -> Result<ImpactPatch, String> {
        let mut patch = ImpactPatch::default();
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);
        let mut in_impact = false;

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let stripped = strip_toml_comment_pub(raw_line).trim();
            if stripped.is_empty() {
                continue;
            }
            if stripped.starts_with('[') && stripped.ends_with(']') {
                let header = stripped[1..stripped.len() - 1].trim();
                in_impact = header == "impact";
                continue;
            }
            if !in_impact {
                continue;
            }
            let Some((raw_key, raw_val)) = stripped.split_once('=') else {
                return Err(format!(
                    "config.toml line {line_no}: expected `key = value` inside [impact]"
                ));
            };
            let key = raw_key.trim();
            let val = raw_val.trim();
            match key {
                "enabled" => {
                    let parsed = val.parse::<bool>().map_err(|_| {
                        format!(
                            "config.toml line {line_no}: `enabled` must be `true` or `false`, got `{val}`"
                        )
                    })?;
                    patch.enabled = Some(parsed);
                }
                "max_hops" => {
                    let parsed = val.parse::<usize>().map_err(|_| {
                        format!(
                            "config.toml line {line_no}: `max_hops` must be a positive integer, got `{val}`"
                        )
                    })?;
                    patch.max_hops = Some(parsed.clamp(1, 4));
                }
                "ask_heat_bp" | "ask_heat" => {
                    let parsed = val.parse::<u32>().map_err(|_| {
                        format!(
                            "config.toml line {line_no}: `{key}` must be a u32 basis-point value, got `{val}`"
                        )
                    })?;
                    patch.ask_heat_bp = Some(parsed.min(10_000));
                }
                "hard_ask_bp" | "hard_ask_heat" => {
                    let parsed = val.parse::<u32>().map_err(|_| {
                        format!(
                            "config.toml line {line_no}: `{key}` must be a u32 basis-point value, got `{val}`"
                        )
                    })?;
                    patch.hard_ask_bp = Some(parsed.min(10_000));
                }
                other => {
                    return Err(format!(
                        "config.toml line {line_no}: unknown key `{other}` in [impact] (expected `enabled`, `max_hops`, `ask_heat_bp`, or `hard_ask_bp`)"
                    ));
                }
            }
        }
        Ok(patch)
    }

    /// Carga y fusiona `[impact]` desde `~/.onyx/config.toml` y `<workspace>/.onyx/config.toml`
    /// (precedencia: workspace > global > default).
    pub fn load_for_workspace(workspace_root: &Path) -> Self {
        let global_path = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|h| Path::new(&h).join(".onyx").join("config.toml"));
        let project_path = workspace_root.join(".onyx").join("config.toml");
        Self::load_from_paths(global_path.as_deref(), Some(project_path.as_path()))
            .unwrap_or_default()
    }

    pub fn load_from_paths(
        global_path: Option<&Path>,
        project_path: Option<&Path>,
    ) -> Result<Self, String> {
        let (cfg, _) = Self::load_from_paths_with_trust(global_path, project_path, true)?;
        Ok(cfg)
    }

    pub fn load_from_paths_with_trust(
        global_path: Option<&Path>,
        project_path: Option<&Path>,
        trust_project: bool,
    ) -> Result<(Self, Vec<String>), String> {
        let mut cfg = Self::default();
        let mut warnings = Vec::new();
        if let Some(gp) = global_path {
            if gp.is_file() {
                let raw = fs::read_to_string(gp)
                    .map_err(|e| format!("failed to read `{}`: {e}", gp.display()))?;
                cfg.apply_patch(Self::parse_toml(&raw)?);
            }
        }
        if let Some(pp) = project_path {
            if pp.is_file() {
                let raw = fs::read_to_string(pp)
                    .map_err(|e| format!("failed to read `{}`: {e}", pp.display()))?;
                let patch = Self::parse_toml(&raw)?;
                if trust_project {
                    cfg.apply_patch(patch);
                } else {
                    warnings.extend(cfg.apply_patch_restrictive(patch));
                }
            }
        }
        Ok((cfg, warnings))
    }
}

/// Resultado estructurado de la evaluación de impacto sobre una mutación propuesta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactAssessment {
    pub max_heat_bp: u32,
    pub callers: usize,
    pub hops: usize,
    pub touches_public_src: bool,
    pub hard_threshold_hit: bool,
    pub escalates_allow_to_ask: bool,
    pub epicenters: Vec<String>,
    pub summary_line: String,
}

/// Regla de monotonía estricta (`Allow ≤ Ask < Deny`, `REQ-MIX1.2`, `TEST-MIX1.3`):
/// solo puede subir `Allow → Ask`; nunca debilita `Deny` ni `Ask`.
pub fn apply_impact_escalation(
    static_decision: PermissionDecision,
    assessment: Option<&ImpactAssessment>,
) -> PermissionDecision {
    match static_decision {
        PermissionDecision::Deny => PermissionDecision::Deny,
        PermissionDecision::Ask => PermissionDecision::Ask,
        PermissionDecision::Allow => {
            if assessment.is_some_and(|a| a.escalates_allow_to_ask) {
                PermissionDecision::Ask
            } else {
                PermissionDecision::Allow
            }
        }
    }
}

/// Extrae los rangos de líneas 1-based inclusivos `(rel_path, start_line, end_line)`
/// afectados por una llamada `fs_edit` o `fs_write` sobre el archivo actual en disco.
pub fn extract_edit_line_ranges(
    policy: &WorkspacePolicy,
    tool: &str,
    args_json: &str,
) -> Vec<(String, usize, usize)> {
    if !matches!(tool, "fs_edit" | "fs_write") {
        return Vec::new();
    }
    let Ok(parsed) = serde_json::from_str::<JsonValue>(args_json) else {
        return Vec::new();
    };
    let Some(raw_path) = parsed.get("path").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let Ok(abs_path) = policy.resolve_existing(raw_path) else {
        // Archivo nuevo que todavía no existe en disco: no tiene callers indexados.
        return Vec::new();
    };
    let Ok(content) = fs::read_to_string(&abs_path) else {
        return Vec::new();
    };
    let rel_path = abs_path
        .strip_prefix(&policy.root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| raw_path.trim().replace('\\', "/"));

    let total_lines = content.lines().count().max(1);
    if tool == "fs_write" {
        return vec![(rel_path, 1, total_lines)];
    }

    // `fs_edit`: soporta tanto `old_text` único como `edits: [{ old_text, ... }]`.
    let mut old_snippets: Vec<&str> = Vec::new();
    if let Some(edits) = parsed.get("edits").and_then(|v| v.as_array()) {
        for item in edits {
            if let Some(old) = item.get("old_text").and_then(|v| v.as_str()) {
                if !old.is_empty() {
                    old_snippets.push(old);
                }
            }
        }
    } else if let Some(old) = parsed.get("old_text").and_then(|v| v.as_str()) {
        if !old.is_empty() {
            old_snippets.push(old);
        }
    }

    let mut ranges = Vec::new();
    for snippet in old_snippets {
        if let Some((start, end)) = locate_snippet_line_range(&content, snippet) {
            ranges.push((rel_path.clone(), start, end));
        }
    }
    ranges
}

fn locate_snippet_line_range(content: &str, snippet: &str) -> Option<(usize, usize)> {
    // 1. Coincidencia exacta por subcadena.
    if let Some(byte_pos) = content.find(snippet) {
        let start_line = content[..byte_pos].bytes().filter(|&b| b == b'\n').count() + 1;
        let span_newlines = snippet
            .trim_end_matches('\n')
            .bytes()
            .filter(|&b| b == b'\n')
            .count();
        return Some((start_line, start_line + span_newlines));
    }

    // 2. Fallback por líneas recortadas (misma filosofía que `fs_edit`).
    let file_lines: Vec<&str> = content.lines().collect();
    let needle_lines: Vec<&str> = snippet.lines().map(str::trim).collect();
    if needle_lines.is_empty() || needle_lines.len() > file_lines.len() {
        return None;
    }
    for i in 0..=(file_lines.len() - needle_lines.len()) {
        let matched = needle_lines
            .iter()
            .enumerate()
            .all(|(off, nl)| file_lines[i + off].trim() == *nl);
        if matched {
            return Some((i + 1, i + needle_lines.len()));
        }
    }
    None
}

/// Evalúa en best-effort el impacto semántico de una mutación `fs_edit`/`fs_write`
/// usando el `WorkspaceIndex` compartido.
#[cfg(feature = "scent")]
pub fn evaluate_mutation_impact(
    policy: &WorkspacePolicy,
    tool: &str,
    args_json: &str,
    scent_index: &Mutex<onyx_scent::WorkspaceIndex>,
) -> Option<ImpactAssessment> {
    if !policy.impact.enabled || !matches!(tool, "fs_edit" | "fs_write") {
        return None;
    }
    let ranges = extract_edit_line_ranges(policy, tool, args_json);
    if ranges.is_empty() {
        return None;
    }

    let mut guard = scent_index.lock().ok()?;
    // Refresco incremental por blake3 (solo reparsea archivos modificados; < 20ms en caché caliente).
    let _ = guard.index_workspace(&policy.root);

    let cfg = onyx_scent::DistillerConfig {
        iterations: u8::try_from(policy.impact.max_hops.clamp(1, 4)).unwrap_or(2),
        threshold_bp: 1_000,
        ..onyx_scent::DistillerConfig::default()
    };
    let summary = guard.impact_for_line_ranges(&ranges, &cfg)?;

    let hard_threshold_hit = summary.max_heat_bp >= policy.impact.hard_ask_bp;
    let escalates_allow_to_ask = hard_threshold_hit
        || (summary.max_heat_bp >= policy.impact.ask_heat_bp && summary.touches_public_src);

    let summary_line = format_impact_line(
        summary.callers,
        summary.hops,
        summary.touches_public_src,
        summary.max_heat_bp,
        &summary.epicenters,
    );

    Some(ImpactAssessment {
        max_heat_bp: summary.max_heat_bp,
        callers: summary.callers,
        hops: summary.hops,
        touches_public_src: summary.touches_public_src,
        hard_threshold_hit,
        escalates_allow_to_ask,
        epicenters: summary.epicenters,
        summary_line,
    })
}

#[cfg(not(feature = "scent"))]
pub fn evaluate_mutation_impact(
    _policy: &WorkspacePolicy,
    _tool: &str,
    _args_json: &str,
) -> Option<ImpactAssessment> {
    None
}

/// Formatea la línea de impacto legible que se muestra en TUI y CLI (`REQ-MIX1.3`).
pub fn format_impact_line(
    callers: usize,
    hops: usize,
    touches_public_src: bool,
    max_heat_bp: u32,
    epicenters: &[String],
) -> String {
    let scope = if touches_public_src {
        "scope:src(pub)"
    } else {
        "scope:priv/test"
    };
    let ep_preview = if epicenters.is_empty() {
        String::new()
    } else {
        let shown: Vec<&str> = epicenters
            .iter()
            .take(3)
            .map(|q| q.split_once("::").map_or(q.as_str(), |(_, tail)| tail))
            .collect();
        let extra = if epicenters.len() > 3 {
            format!(" +{}", epicenters.len() - 3)
        } else {
            String::new()
        };
        format!(" (epicenters: {}{extra})", shown.join(", "))
    };
    let caller_word = if callers == 1 { "caller" } else { "callers" };
    let hop_word = if hops == 1 { "hop" } else { "hops" };
    format!(
        "# impact: {callers} {caller_word}, {hops} {hop_word}, {scope} [heat {max_heat_bp}bp]{ep_preview}"
    )
}
