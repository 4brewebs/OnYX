use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use serde_json::Value as JsonValue;

use crate::external_driver::{builtin_external_driver_presets, load_external_drivers};
use crate::impact::ImpactConfig;
use crate::llm_profile::{LlmProfileConfig, load_llm_profiles};
use crate::sandbox::{SandboxConfig, SandboxMode};
use crate::state::WorkspaceMode;
use crate::trust::{WorkspaceTrustReport, evaluate_workspace_trust};
use crate::worktree::{ExternalDriverConfig, SubagentNetworkPolicy};

/// Resultado de evaluar las reglas de permisos para una herramienta y sus argumentos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}

/// Reglas declarativas de permisos cargadas desde `~/.onyx/config.toml` y `<workspace>/.onyx/config.toml`.
///
/// Precedencia: `deny > ask > allow` (con desempate por especificidad entre `allow` y `ask`
/// para que un `allow` específico como `shell_exec(cargo test*)` no sea ocultado por un
/// `ask` comodín como `shell_exec(*)`, mientras que un `ask` de igual o mayor especificidad
/// siempre prevalece sobre `allow`, y `deny` bloquea incondicionalmente).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionRules {
    pub deny: Vec<String>,
    pub allow: Vec<String>,
    pub ask: Vec<String>,
}

impl PermissionRules {
    pub fn is_empty(&self) -> bool {
        self.deny.is_empty() && self.allow.is_empty() && self.ask.is_empty()
    }

    /// Parsea la sección `[permissions]` de un texto TOML (`.onyx/config.toml`).
    /// Ignora otras secciones para convivir limpiamente con futuras opciones de configuración.
    pub fn parse_toml(input: &str) -> Result<Self, String> {
        let mut rules = Self::default();
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);
        let lines: Vec<&str> = text.lines().collect();
        let mut in_permissions = false;
        let mut i = 0usize;

        while i < lines.len() {
            let line_no = i + 1;
            let stripped = strip_toml_comment(lines[i]).trim();
            if stripped.is_empty() {
                i += 1;
                continue;
            }
            if stripped.starts_with('[') && stripped.ends_with(']') {
                let header = stripped[1..stripped.len() - 1].trim();
                in_permissions = header == "permissions";
                i += 1;
                continue;
            }
            if !in_permissions {
                i += 1;
                continue;
            }
            let Some((raw_key, raw_val)) = stripped.split_once('=') else {
                return Err(format!(
                    "config.toml line {line_no}: expected `key = [...]` inside [permissions]"
                ));
            };
            let key = raw_key.trim();
            let mut value_buf = raw_val.trim().to_string();

            // Soporta arrays multi-línea `[ ... ]` acumulando hasta cerrar el corchete.
            if value_buf.starts_with('[') {
                while !array_brackets_closed(&value_buf) {
                    i += 1;
                    if i >= lines.len() {
                        return Err(format!(
                            "config.toml line {line_no}: unterminated array for `{key}`"
                        ));
                    }
                    let next_piece = strip_toml_comment(lines[i]).trim();
                    if !next_piece.is_empty() {
                        value_buf.push(' ');
                        value_buf.push_str(next_piece);
                    }
                }
            }

            let parsed_list = parse_toml_string_array(&value_buf)
                .map_err(|e| format!("config.toml line {line_no} (`{key}`): {e}"))?;
            match key {
                "deny" => rules.deny.extend(parsed_list),
                "allow" => rules.allow.extend(parsed_list),
                "ask" => rules.ask.extend(parsed_list),
                other => {
                    return Err(format!(
                        "config.toml line {line_no}: unknown key `{other}` in [permissions] (expected `deny`, `allow`, or `ask`)"
                    ));
                }
            }
            i += 1;
        }

        Ok(rules)
    }

    /// Carga y fusiona reglas desde `~/.onyx/config.toml` (global) y `<workspace>/.onyx/config.toml`.
    /// Las reglas `deny` globales nunca pueden ser eliminadas por el proyecto.
    pub fn load_for_workspace(workspace_root: &Path) -> Self {
        let global_path = user_home_dir().map(|h| h.join(".onyx").join("config.toml"));
        let project_path = workspace_root.join(".onyx").join("config.toml");
        Self::load_from_paths(global_path.as_deref(), Some(project_path.as_path()))
            .unwrap_or_default()
    }

    /// Carga y fusiona dos rutas de configuración opcionales (`global` y `project`).
    pub fn load_from_paths(
        global_path: Option<&Path>,
        project_path: Option<&Path>,
    ) -> Result<Self, String> {
        let (rules, _) = Self::load_from_paths_with_trust(global_path, project_path, true)?;
        Ok(rules)
    }

    /// Carga y fusiona `global` y `project` respetando Workspace Trust:
    /// si `trust_project == false`, solo se aplican `deny` y `ask` del proyecto y se descartan
    /// todas sus reglas `allow`, devolviendo cuántas reglas `allow` fueron ignoradas.
    pub fn load_from_paths_with_trust(
        global_path: Option<&Path>,
        project_path: Option<&Path>,
        trust_project: bool,
    ) -> Result<(Self, usize), String> {
        let mut merged = Self::default();
        if let Some(gp) = global_path {
            if gp.is_file() {
                let raw = fs::read_to_string(gp)
                    .map_err(|e| format!("failed to read `{}`: {e}", gp.display()))?;
                let global = Self::parse_toml(&raw)?;
                merged.merge(global);
            }
        }
        let mut ignored_allow = 0usize;
        if let Some(pp) = project_path {
            if pp.is_file() {
                let raw = fs::read_to_string(pp)
                    .map_err(|e| format!("failed to read `{}`: {e}", pp.display()))?;
                let project = Self::parse_toml(&raw)?;
                if trust_project {
                    merged.merge(project);
                } else {
                    ignored_allow = merged.merge_restrictive(project);
                }
            }
        }
        Ok((merged, ignored_allow))
    }

    /// Combina otras reglas preservando todas las reglas `deny` existentes.
    pub fn merge(&mut self, other: Self) {
        self.deny.extend(other.deny);
        self.allow.extend(other.allow);
        self.ask.extend(other.ask);
    }

    /// Combina reglas de un repositorio NO confiado: aplica `deny` y `ask`, pero descarta
    /// todas las reglas `allow` (devuelve el número de reglas `allow` ignoradas).
    pub fn merge_restrictive(&mut self, other: Self) -> usize {
        self.deny.extend(other.deny);
        self.ask.extend(other.ask);
        other.allow.len()
    }

    /// Evalúa una llamada de herramienta frente a `deny`, `ask` y `allow`.
    pub fn evaluate(&self, tool: &str, args_json: &str, ignore_deny: bool) -> PermissionDecision {
        let subject = extract_rule_subject(tool, args_json);

        // 1. `deny` bloquea incondicionalmente (salvo `ignore_deny` explícito).
        if !ignore_deny
            && self
                .deny
                .iter()
                .any(|rule| match_permission_rule(rule, tool, &subject).is_some())
        {
            return PermissionDecision::Deny;
        }

        // 2. Calcular especificidad máxima de coincidencias en `ask` y `allow`.
        let best_ask = self
            .ask
            .iter()
            .filter_map(|rule| match_permission_rule(rule, tool, &subject))
            .max();
        let best_allow = self
            .allow
            .iter()
            .filter_map(|rule| match_permission_rule(rule, tool, &subject))
            .max();

        match (best_ask, best_allow) {
            (Some(ask_spec), Some(allow_spec)) => {
                // Precedencia `ask > allow` ante igual o mayor especificidad;
                // un `allow` más específico (p.ej. `shell_exec(cargo test*)`) vence a un
                // `ask` comodín genérico (`shell_exec(*)`).
                if ask_spec >= allow_spec {
                    PermissionDecision::Ask
                } else {
                    PermissionDecision::Allow
                }
            }
            (Some(_), None) => PermissionDecision::Ask,
            (None, Some(_)) => PermissionDecision::Allow,
            (None, None) => {
                // `SDD-04 REQ-04.4`: toda herramienta `mcp__*` requiere aprobación por
                // defecto (equivalente a mutante para el fallback, aunque no muten el
                // workspace local).
                if is_mutating_tool(tool) || tool.starts_with("mcp__") {
                    PermissionDecision::Ask
                } else {
                    PermissionDecision::Allow
                }
            }
        }
    }
}

/// Indica si una herramienta realiza mutaciones sobre el workspace, ejecuta comandos de shell
/// o modifica el ciclo de vida de servicios/contenedores (`REQ-08.3`, `REQ-08.4`).
pub fn is_mutating_tool(tool: &str) -> bool {
    matches!(
        tool,
        "fs_write"
            | "fs_edit"
            | "shell_exec"
            | "service_start"
            | "service_stop"
            | "service_exec"
            | "subagent_merge"
    )
}

/// Compila un patrón glob (`*`, `**`, `?`, `[abc]`, `{a,b}`) a una `Regex` determinista.
///
/// - `match_slash_for_star = false`: modo ruta de archivo (`*` no cruza `/`, `**` cruza segmentos;
///   un patrón sin `/` como `*.rs` o `.env*` coincide tanto con el nombre base como con la ruta completa).
/// - `match_slash_for_star = true`: modo comando shell (`*` coincide con cualquier carácter).
pub fn compile_glob_regex(pattern: &str, match_slash_for_star: bool) -> Result<Regex, String> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err("glob pattern cannot be empty".to_string());
    }
    let normalized = if match_slash_for_star {
        trimmed.to_string()
    } else {
        let slash = trimmed.replace('\\', "/");
        slash.strip_prefix("./").unwrap_or(&slash).to_string()
    };

    let body = glob_to_regex_body(&normalized, match_slash_for_star)?;
    let full = if match_slash_for_star {
        format!("(?s)^{body}$")
    } else if !normalized.contains('/') {
        format!("^(?:.*/)?{body}$")
    } else {
        format!("^{body}$")
    };

    Regex::new(&full).map_err(|e| format!("invalid glob pattern `{pattern}`: {e}"))
}

fn glob_to_regex_body(pattern: &str, match_slash_for_star: bool) -> Result<String, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len() * 2);
    let mut i = 0usize;

    while i < chars.len() {
        match chars[i] {
            '*' => {
                if !match_slash_for_star && i + 1 < chars.len() && chars[i + 1] == '*' {
                    // `**/` coincide con 0 o más directorios.
                    if i + 2 < chars.len() && chars[i + 2] == '/' {
                        out.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        out.push_str(".*");
                        i += 2;
                    }
                } else if match_slash_for_star {
                    out.push_str(".*");
                    i += 1;
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '/' if !match_slash_for_star
                && i + 2 < chars.len()
                && chars[i + 1] == '*'
                && chars[i + 2] == '*'
                && (i + 3 == chars.len()) =>
            {
                // `docs/**` coincide con `docs/a` y `docs/a/b`.
                out.push_str("(?:/.*)?");
                i += 3;
            }
            '?' => {
                if match_slash_for_star {
                    out.push('.');
                } else {
                    out.push_str("[^/]");
                }
                i += 1;
            }
            '[' => {
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '!' || chars[j] == '^') {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(format!("unclosed character class `[` in `{pattern}`"));
                }
                out.push('[');
                let mut k = i + 1;
                if k < j && chars[k] == '!' {
                    out.push('^');
                    k += 1;
                }
                while k < j {
                    if chars[k] == '\\' {
                        out.push_str("\\\\");
                    } else {
                        out.push(chars[k]);
                    }
                    k += 1;
                }
                out.push(']');
                i = j + 1;
            }
            '{' => {
                let mut depth = 1usize;
                let mut j = i + 1;
                let mut parts: Vec<String> = Vec::new();
                let mut cur = String::new();
                while j < chars.len() && depth > 0 {
                    match chars[j] {
                        '{' => {
                            depth += 1;
                            cur.push('{');
                        }
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                parts.push(cur.clone());
                                cur.clear();
                                break;
                            }
                            cur.push('}');
                        }
                        ',' if depth == 1 => {
                            parts.push(cur.clone());
                            cur.clear();
                        }
                        c => cur.push(c),
                    }
                    j += 1;
                }
                if depth != 0 {
                    return Err(format!("unclosed brace group `{{` in `{pattern}`"));
                }
                out.push_str("(?:");
                for (idx, part) in parts.iter().enumerate() {
                    if idx > 0 {
                        out.push('|');
                    }
                    out.push_str(&glob_to_regex_body(part, match_slash_for_star)?);
                }
                out.push(')');
                i = j + 1;
            }
            c => {
                if matches!(
                    c,
                    '.' | '+' | '(' | ')' | '|' | '^' | '$' | '\\' | ']' | '}'
                ) {
                    out.push('\\');
                }
                out.push(c);
                i += 1;
            }
        }
    }

    Ok(out)
}

fn extract_rule_subject(tool: &str, args_json: &str) -> String {
    let parsed: JsonValue = serde_json::from_str(args_json).unwrap_or(JsonValue::Null);
    match tool {
        "shell_exec" => parsed
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
        "service_start" | "service_stop" => {
            if parsed.get("all").and_then(|v| v.as_bool()).unwrap_or(false) {
                "all".to_string()
            } else {
                parsed
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("all")
                    .to_string()
            }
        }
        "service_exec" => {
            let id = parsed
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or_default();
            let cmd = parsed
                .get("command")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or_default();
            if id.is_empty() {
                cmd.to_string()
            } else {
                format!("{id}:{cmd}")
            }
        }
        "fs_write" | "fs_edit" | "fs_read" | "fs_list" | "glob_files" | "code_search" => {
            let raw = parsed
                .get("path")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or_default();
            let norm = raw.replace('\\', "/");
            norm.strip_prefix("./").unwrap_or(&norm).to_string()
        }
        _ => String::new(),
    }
}

/// Evalúa si `rule` coincide con `(tool, subject)`. Devuelve la especificidad (número de caracteres
/// literales del patrón) si coincide, o `None` si no aplica.
fn match_permission_rule(rule: &str, tool: &str, subject: &str) -> Option<usize> {
    let trimmed = rule.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (rule_tool, subject_glob) = if let Some(open_idx) = trimmed.find('(') {
        if !trimmed.ends_with(')') {
            return None;
        }
        (
            trimmed[..open_idx].trim(),
            trimmed[open_idx + 1..trimmed.len() - 1].trim(),
        )
    } else {
        (trimmed, "*")
    };

    // Parte `tool`: exacta, comodín `*`, o glob (`*`/`?`) — p. ej. `deny = ["mcp__github__*"]`
    // (`SDD-04 REQ-04.4`). Los globs de tool aportan su número de caracteres literales a la
    // especificidad para que un patrón concreto (`mcp__github__*`) prevalezca sobre uno
    // genérico (`mcp__*`) en la decisión `ask`/`allow`.
    let tool_specificity = if rule_tool.contains('*') || rule_tool.contains('?') {
        let tool_re = compile_glob_regex(rule_tool, true).ok()?;
        if !tool_re.is_match(tool) {
            return None;
        }
        rule_tool.chars().filter(|c| *c != '*' && *c != '?').count()
    } else if rule_tool == "*" {
        0
    } else if rule_tool == tool {
        // La coincidencia exacta es siempre más específica que cualquier glob que
        // también coincida (p. ej. `allow = ["mcp__mock__echo"]` vence a
        // `ask = ["mcp__mock__*"]`): `tool.len() + 1` está por encima del máximo
        // posible de chars literales de un glob coincidente (≤ `tool.len()`).
        tool.len() + 1
    } else {
        return None;
    };

    let match_slash_for_star = matches!(tool, "shell_exec" | "service_exec");
    let re = compile_glob_regex(subject_glob, match_slash_for_star).ok()?;
    if !re.is_match(subject) {
        return None;
    }

    let specificity = subject_glob
        .chars()
        .filter(|c| *c != '*' && *c != '?')
        .count();
    Some(tool_specificity + specificity)
}

fn strip_toml_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    for (idx, &b) in bytes.iter().enumerate() {
        if in_double {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_double = false;
            }
        } else if in_single {
            if b == b'\'' {
                in_single = false;
            }
        } else {
            match b {
                b'"' => in_double = true,
                b'\'' => in_single = true,
                b'#' => return &line[..idx],
                _ => {}
            }
        }
    }
    line
}

fn array_brackets_closed(buf: &str) -> bool {
    let mut depth = 0i32;
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    for b in buf.bytes() {
        if in_double {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_double = false;
            }
        } else if in_single {
            if b == b'\'' {
                in_single = false;
            }
        } else {
            match b {
                b'"' => in_double = true,
                b'\'' => in_single = true,
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

fn parse_toml_string_array(raw: &str) -> Result<Vec<String>, String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| "expected TOML string array `[...]`".to_string())?;

    let mut out = Vec::new();
    let chars: Vec<char> = inner.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        match chars[i] {
            '"' => {
                i += 1;
                let mut item = String::new();
                let mut closed = false;
                while i < chars.len() {
                    let c = chars[i];
                    i += 1;
                    if c == '\\' {
                        let Some(&esc) = chars.get(i) else {
                            return Err("unterminated escape sequence in string".to_string());
                        };
                        i += 1;
                        match esc {
                            '"' => item.push('"'),
                            '\\' => item.push('\\'),
                            'n' => item.push('\n'),
                            'r' => item.push('\r'),
                            't' => item.push('\t'),
                            other => item.push(other),
                        }
                    } else if c == '"' {
                        closed = true;
                        break;
                    } else {
                        item.push(c);
                    }
                }
                if !closed {
                    return Err("unterminated double-quoted string".to_string());
                }
                out.push(item);
            }
            '\'' => {
                i += 1;
                let mut item = String::new();
                let mut closed = false;
                while i < chars.len() {
                    let c = chars[i];
                    i += 1;
                    if c == '\'' {
                        closed = true;
                        break;
                    }
                    item.push(c);
                }
                if !closed {
                    return Err("unterminated single-quoted string".to_string());
                }
                out.push(item);
            }
            other => {
                return Err(format!(
                    "expected quoted string inside array, found `{other}`"
                ));
            }
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() {
            if chars[i] == ',' {
                i += 1;
            } else {
                return Err(format!(
                    "expected `,` between array items, found `{}`",
                    chars[i]
                ));
            }
        }
    }

    Ok(out)
}

pub(crate) fn strip_toml_comment_pub(line: &str) -> &str {
    strip_toml_comment(line)
}

pub(crate) fn array_brackets_closed_pub(buf: &str) -> bool {
    array_brackets_closed(buf)
}

pub(crate) fn parse_toml_string_array_pub(raw: &str) -> Result<Vec<String>, String> {
    parse_toml_string_array(raw)
}

/// Comprueba si una ruta relativa pertenece al *Control Plane* protegido en solo lectura (`REQ-06.4`):
/// `.onyx/config.toml`, `.onyx/vault/**`, `.git/hooks/**`, `.git/config` y `.env*`.
pub fn is_control_plane_rel_path(raw: &str) -> bool {
    let norm = raw.trim().replace('\\', "/");
    let stripped = norm.trim_start_matches("./").trim_start_matches('/');
    let lower = stripped.to_ascii_lowercase();
    if lower == ".onyx/config.toml"
        || lower.starts_with(".onyx/config.")
        || lower == ".onyx/vault"
        || lower.starts_with(".onyx/vault/")
        || lower == ".git/config"
        || lower == ".git/hooks"
        || lower.starts_with(".git/hooks/")
    {
        return true;
    }
    let file_name = lower.rsplit('/').next().unwrap_or("");
    file_name == ".env" || file_name.starts_with(".env.")
}

/// Veredicto de la clasificación del delta de un merge 3-way (`SDD-10` Fase 3):
/// - [`MergeDeltaVerdict::Safe`]: el delta no toca superficies sensibles → aprobación
///   estándar por impacto (`apply_impact_escalation`).
/// - [`MergeDeltaVerdict::Ask`]: el delta toca `sprout.toml` o `.github/workflows/**`
///   (gobernanza) → la aprobación del merge debe ser explícita (`Ask`), nunca auto-aprobada.
/// - [`MergeDeltaVerdict::Deny`]: el delta toca el *Control Plane* solo lectura
///   (`.onyx/config.toml`, `.env*`, `.git/hooks/**`, ...) → el merge debe bloquearse sin
///   tocar el disco de `root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeDeltaVerdict {
    Safe,
    Ask,
    Deny,
}

/// Ruta de gobernanza no crítica (`SDD-10` Fase 3): un subagente (especialmente uno con
/// driver externo) que la toca fuerza aprobación explícita (`Ask`) en el merge.
pub fn is_governance_rel_path(raw: &str) -> bool {
    let norm = raw.trim().replace('\\', "/");
    let lower = norm
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_ascii_lowercase();
    lower == "sprout.toml" || lower.starts_with(".github/workflows/")
}

/// Clasifica el delta `<root_now_commit>..<merged_tree>` de un merge 3-way contra el
/// *Control Plane* (Deny) y las superficies de gobernanza (Ask). `Deny` corta a la primera
/// ruta de Control Plane: un solo `.onyx/config.toml` o `.env*` bloquea el merge entero
/// (`SDD-10` Fase 3).
pub fn classify_merge_delta(changed_files: &[String]) -> MergeDeltaVerdict {
    let mut verdict = MergeDeltaVerdict::Safe;
    for f in changed_files {
        if is_control_plane_rel_path(f) {
            return MergeDeltaVerdict::Deny;
        }
        if verdict == MergeDeltaVerdict::Safe && is_governance_rel_path(f) {
            verdict = MergeDeltaVerdict::Ask;
        }
    }
    verdict
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Política de seguridad y límites de I/O para operaciones sobre el workspace.
#[derive(Clone, Debug)]
pub struct WorkspacePolicy {
    pub root: PathBuf,
    pub ignored_dirs: Vec<String>,
    pub max_read_bytes: usize,
    pub max_output_bytes: usize,
    pub command_timeout: Duration,
    pub permissions: PermissionRules,
    pub ignore_deny: bool,
    pub sandbox: SandboxConfig,
    pub impact: ImpactConfig,
    pub trust: WorkspaceTrustReport,
    pub trust_warnings: Vec<String>,
    external_drivers: Vec<ExternalDriverConfig>,
    llm_profiles: Vec<LlmProfileConfig>,
}

impl WorkspacePolicy {
    /// Crea una política fijando `root` canonizado en disco.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        let canonical = fs::canonicalize(root)?;
        Ok(Self {
            root: canonical,
            ignored_dirs: vec![
                ".git".to_string(),
                "target".to_string(),
                "node_modules".to_string(),
            ],
            max_read_bytes: 64 * 1024,
            max_output_bytes: 32 * 1024,
            command_timeout: Duration::from_secs(30),
            permissions: PermissionRules::default(),
            ignore_deny: false,
            sandbox: SandboxConfig::default(),
            impact: ImpactConfig::default(),
            trust: WorkspaceTrustReport::no_project_config(),
            trust_warnings: Vec::new(),
            external_drivers: builtin_external_driver_presets(),
            // Sin presets: el perfil implícito de arranque CLI vive en el anfitrión
            // (`LlmProfileHandle`, perfil `"default"`); aquí solo hay perfiles declarados.
            llm_profiles: Vec::new(),
        })
    }

    /// Drivers de harness CLI externos cargados (`preset < global < workspace (trusted)`; `SDD-10` Fase 1).
    pub fn external_drivers(&self) -> &[ExternalDriverConfig] {
        &self.external_drivers
    }

    /// Establece la lista de drivers externos explícitamente (tests, overrides de serie).
    pub fn with_external_drivers(mut self, drivers: Vec<ExternalDriverConfig>) -> Self {
        self.external_drivers = drivers;
        self
    }

    /// Perfiles LLM declarados en `[[llm_profiles]]` (`global < workspace (trusted)`; `SDD-03` hot-swap).
    pub fn llm_profiles(&self) -> &[LlmProfileConfig] {
        &self.llm_profiles
    }

    /// Establece la lista de perfiles LLM explícitamente (tests, overrides de arranque).
    pub fn with_llm_profiles(mut self, profiles: Vec<LlmProfileConfig>) -> Self {
        self.llm_profiles = profiles;
        self
    }

    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// Deriva una política confinada para ejecutar un subagente nativo dentro de un *worktree*
    /// desechable (`SDD-09` Fase 2):
    /// - Re-enraiza `root` en `worktree_root` canonizado.
    /// - Fuerza al menos `SandboxMode::WorkspaceWrite` (preservando `StrictContainer` si ya es más estricto).
    /// - Aplica [`SubagentNetworkPolicy::derived_network_policy`] (`NativeOffline` -> `Offline`;
    ///   `ExternalHarnessAllowlist` -> no-`Offline` para que el harness hable con su API).
    /// - Concede auto-aprobación confinada al worktree (`fs_write`/`fs_edit`/`shell_exec`)
    ///   preservando intactas todas las reglas `deny` y la protección del Control Plane.
    pub fn for_worktree(
        &self,
        worktree_root: impl AsRef<Path>,
        network_policy: &SubagentNetworkPolicy,
    ) -> io::Result<Self> {
        let canonical = fs::canonicalize(worktree_root)?;
        let mut derived = self.clone();
        derived.root = canonical;
        if derived.sandbox.mode == SandboxMode::Off {
            derived.sandbox.mode = SandboxMode::WorkspaceWrite;
        }
        // `SDD-10` Fase 2: el harness CLI externo deriva una política **no-`Offline`** (sin
        // hints `*_OFFLINE`/`NO_PROXY`) para poder hablar con su API; el allowlist de hosts
        // es declarativo (ver [`SubagentNetworkPolicy::derived_network_policy`]).
        derived.sandbox.network = network_policy.derived_network_policy();
        derived.permissions = PermissionRules {
            deny: self.permissions.deny.clone(),
            allow: vec![
                "fs_write(*)".to_string(),
                "fs_edit(*)".to_string(),
                "shell_exec(*)".to_string(),
            ],
            ask: Vec::new(),
        };
        Ok(derived)
    }

    /// Carga secciones `[permissions]`, `[sandbox]` e `[impact]` desde `~/.onyx/config.toml` y
    /// `<workspace>/.onyx/config.toml` aplicando **Workspace Trust** (`~/.onyx/trusted_workspaces.toml`):
    /// si el repositorio no está confiado o cambió de hash `blake3`, `.onyx/config.toml` solo
    /// puede restringir (nunca ampliar `allow` ni relajar `[sandbox]`/`[impact]`).
    pub fn with_loaded_config(self) -> Self {
        self.with_loaded_config_from_store(None, false)
    }

    /// Carga configuración del workspace permitiendo forzar confianza explícita en memoria
    /// (p. ej. `--trust-workspace` o tests unitarios de precedencia).
    pub fn with_loaded_config_trusted(self, force_trust: bool) -> Self {
        self.with_loaded_config_from_store(None, force_trust)
    }

    /// Carga configuración usando una ruta explícita de almacén de confianza (útil en tests herméticos).
    pub fn with_loaded_config_from_store(
        mut self,
        trust_store_path: Option<&Path>,
        force_trust: bool,
    ) -> Self {
        let trust = if force_trust {
            WorkspaceTrustReport::trusted_override(&self.root)
        } else {
            evaluate_workspace_trust(&self.root, trust_store_path)
        };
        let trust_project = trust.config_trusted;
        let global_path = user_home_dir().map(|h| h.join(".onyx").join("config.toml"));
        let project_path = self.root.join(".onyx").join("config.toml");

        let mut warnings = Vec::new();
        match PermissionRules::load_from_paths_with_trust(
            global_path.as_deref(),
            Some(project_path.as_path()),
            trust_project,
        ) {
            Ok((perms, ignored_allow)) => {
                self.permissions = perms;
                if ignored_allow > 0 {
                    warnings.push(format!(
                        "untrusted project `.onyx/config.toml`: ignored {ignored_allow} `allow` rule(s); run `openonyx trust` or `/trust` to approve"
                    ));
                }
            }
            Err(err) => warnings.push(format!("failed to parse `[permissions]`: {err}")),
        }

        match SandboxConfig::load_from_paths_with_trust(
            global_path.as_deref(),
            Some(project_path.as_path()),
            trust_project,
        ) {
            Ok((sb, sb_warns)) => {
                self.sandbox = sb;
                warnings.extend(sb_warns);
            }
            Err(err) => warnings.push(format!("failed to parse `[sandbox]`: {err}")),
        }

        match ImpactConfig::load_from_paths_with_trust(
            global_path.as_deref(),
            Some(project_path.as_path()),
            trust_project,
        ) {
            Ok((imp, imp_warns)) => {
                self.impact = imp;
                warnings.extend(imp_warns);
            }
            Err(err) => warnings.push(format!("failed to parse `[impact]`: {err}")),
        }

        // `[[external_drivers]]` (`SDD-10` Fase 1): parse error → warning + presets intactos.
        match load_external_drivers(
            global_path.as_deref(),
            Some(project_path.as_path()),
            trust_project,
        ) {
            Ok((drivers, drv_warns)) => {
                self.external_drivers = drivers;
                warnings.extend(drv_warns);
            }
            Err(err) => warnings.push(format!("failed to parse `[[external_drivers]]`: {err}")),
        }

        // `[[llm_profiles]]` (`SDD-03` hot-swap): parse error → warning sin perfiles declarados.
        match load_llm_profiles(
            global_path.as_deref(),
            Some(project_path.as_path()),
            trust_project,
        ) {
            Ok((profiles, llm_warns)) => {
                self.llm_profiles = profiles;
                warnings.extend(llm_warns);
            }
            Err(err) => warnings.push(format!("failed to parse `[[llm_profiles]]`: {err}")),
        }

        self.trust = trust;
        self.trust_warnings = warnings;
        self
    }

    /// Fija un informe de confianza explícito (p. ej. tras aprobar en caliente o en tests).
    pub fn with_trust_report(mut self, trust: WorkspaceTrustReport) -> Self {
        self.trust = trust;
        self
    }

    /// Establece la configuración `[impact]` explícita (*Scent-Heated Approval*).
    pub fn with_impact(mut self, impact: ImpactConfig) -> Self {
        self.impact = impact;
        self
    }

    /// Establece reglas de permisos declarativas explícitas.
    pub fn with_permissions(mut self, permissions: PermissionRules) -> Self {
        self.permissions = permissions;
        self
    }

    /// Establece la configuración de sandbox OS explícita (`REQ-06.1`..`REQ-06.5`).
    pub fn with_sandbox(mut self, sandbox: SandboxConfig) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Permite ignorar reglas `deny` cuando el usuario pasa explícitamente `--ignore-deny`.
    pub fn with_ignore_deny(mut self, ignore_deny: bool) -> Self {
        self.ignore_deny = ignore_deny;
        self
    }

    /// Indica si `raw` apunta a una ruta del *Control Plane* protegida en solo lectura (`REQ-06.4`).
    pub fn is_control_plane_path(&self, raw: &str) -> bool {
        if is_control_plane_rel_path(raw) {
            return true;
        }
        if let Ok(resolved) = self.join_raw(raw) {
            if let Ok(rel) = resolved.strip_prefix(&self.root) {
                return is_control_plane_rel_path(&rel.to_string_lossy());
            }
        }
        false
    }

    /// Rechaza intentos de escritura sobre rutas del *Control Plane* (`REQ-06.4`, `TEST-06.4`).
    pub fn check_control_plane_write(&self, raw: &str) -> Result<(), String> {
        if self.is_control_plane_path(raw) {
            return Err(format!(
                "blocked by control-plane protection (read-only): `{raw}` cannot be modified by agent tools"
            ));
        }
        Ok(())
    }

    /// Evalúa las reglas de permisos configuradas (sin tener en cuenta `WorkspaceMode`).
    pub fn evaluate_permission(&self, tool: &str, args_json: &str) -> PermissionDecision {
        self.permissions.evaluate(tool, args_json, self.ignore_deny)
    }

    /// Gate de aprobación/trust para `spawn_subagent` con driver externo no-nativo (`SDD-10`).
    ///
    /// Devuelve la decisión y un motivo accionable (no vacío en `Deny`/`Ask`):
    /// - Workspace no confiado → `Deny` absoluto (límite de confianza; ni `ignore_deny` lo anula).
    /// - Regla `deny` explícita `spawn_subagent(<driver>)` → `Deny`.
    /// - Precedencia `ask` vs `allow` idéntica a [`PermissionRules::evaluate`] (especificidad;
    ///   ante igual o mayor especificidad gana `ask`).
    /// - Sin reglas `ask`/`allow` → `Ask` (nunca cae en el fallback estático `Allow` de
    ///   herramientas no mutantes, que aplicaría a `spawn_subagent` sin driver).
    pub fn external_driver_gate(&self, driver_id: &str) -> (PermissionDecision, String) {
        if !self.trust.config_trusted {
            return (
                PermissionDecision::Deny,
                format!(
                    "blocked: external harness driver `{driver_id}` requires a trusted workspace (run `openonyx trust` or `/trust` to approve)"
                ),
            );
        }
        if !self.ignore_deny
            && self
                .permissions
                .deny
                .iter()
                .any(|rule| match_permission_rule(rule, "spawn_subagent", driver_id).is_some())
        {
            return (
                PermissionDecision::Deny,
                format!(
                    "blocked by permission policy (deny rule): `spawn_subagent` with external driver `{driver_id}`"
                ),
            );
        }
        // Misma semántica de especificidad que `PermissionRules::evaluate`: ante igual o
        // mayor especificidad, `ask >= allow`; un `allow` más específico vence al `ask` genérico.
        let best_ask = self
            .permissions
            .ask
            .iter()
            .filter_map(|rule| match_permission_rule(rule, "spawn_subagent", driver_id))
            .max();
        let best_allow = self
            .permissions
            .allow
            .iter()
            .filter_map(|rule| match_permission_rule(rule, "spawn_subagent", driver_id))
            .max();
        let decision = match (best_ask, best_allow) {
            (Some(ask_spec), Some(allow_spec)) => {
                if ask_spec >= allow_spec {
                    PermissionDecision::Ask
                } else {
                    PermissionDecision::Allow
                }
            }
            (Some(_), None) => PermissionDecision::Ask,
            (None, Some(_)) => PermissionDecision::Allow,
            (None, None) => PermissionDecision::Ask,
        };
        if decision == PermissionDecision::Allow {
            (PermissionDecision::Allow, String::new())
        } else {
            (
                PermissionDecision::Ask,
                format!(
                    "external harness driver `{driver_id}` will run an external CLI process in an isolated Git worktree"
                ),
            )
        }
    }

    /// Evalúa las reglas de permisos aplicando las invariantes de seguridad:
    /// 1. En `Plan` o `Explore`, toda herramienta mutante queda siempre bloqueada (`Deny`),
    ///    incluso si coincide con una regla `allow`.
    /// 2. Las rutas del *Control Plane* (`.onyx/config.toml`, `.git/hooks/**`, `.git/config`,
    ///    `.env*`) quedan bloqueadas para `fs_write`/`fs_edit` (`REQ-06.4`).
    pub fn evaluate_permission_in_mode(
        &self,
        mode: WorkspaceMode,
        tool: &str,
        args_json: &str,
    ) -> PermissionDecision {
        // `SDD-04 REQ-04.4`: en `Plan`/`Explore`, cualquier `mcp__*` queda `Deny` aunque
        // no sea mutante local: un servidor MCP remoto ejecuta procesos/red fuera del
        // control de `WorkspaceToolBridge`.
        if (is_mutating_tool(tool) || tool.starts_with("mcp__")) && !mode.allows_mutation() {
            return PermissionDecision::Deny;
        }
        if matches!(tool, "fs_write" | "fs_edit") {
            let subject = extract_rule_subject(tool, args_json);
            if !subject.is_empty() && self.is_control_plane_path(&subject) {
                return PermissionDecision::Deny;
            }
        }
        self.evaluate_permission(tool, args_json)
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let mut prev_is_onyx = false;
        rel.components().any(|c| {
            if let Component::Normal(name) = c {
                let s = name.to_string_lossy();
                let s_ref = s.as_ref();
                let is_vault_under_onyx = prev_is_onyx && s_ref.eq_ignore_ascii_case("vault");
                prev_is_onyx = s_ref.eq_ignore_ascii_case(".onyx");
                is_vault_under_onyx
                    || self.ignored_dirs.iter().any(|ignored| ignored == s_ref)
                    || s_ref == "snapshots"
                    || s_ref.starts_with("snaps_")
                    || s_ref.ends_with(".tr")
                    || s_ref.ends_with(".onyxpack")
                    || s_ref.ends_with(".onyxpack.tmp")
                    || (s_ref.starts_with("tick-") && s_ref.ends_with(".bin"))
            } else {
                prev_is_onyx = false;
                false
            }
        })
    }

    /// Resuelve una ruta existente dentro de `root`, bloqueando escapes por `..` o symlinks.
    pub fn resolve_existing(&self, raw: &str) -> Result<PathBuf, String> {
        let candidate = self.join_raw(raw)?;
        let canonical = fs::canonicalize(&candidate)
            .map_err(|e| format!("cannot resolve path `{raw}`: {e}"))?;
        if !canonical.starts_with(&self.root) {
            return Err(format!("path `{raw}` escapes workspace root"));
        }
        Ok(canonical)
    }

    /// Resuelve una ruta destino (puede no existir aún) verificando que su
    /// ancestro existente más cercano esté dentro de `root`.
    pub fn resolve_for_write(&self, raw: &str) -> Result<PathBuf, String> {
        let candidate = self.join_raw(raw)?;
        let mut rest = Vec::new();
        let mut cursor = candidate.as_path();

        while !cursor.exists() {
            let Some(name) = cursor.file_name() else {
                return Err(format!("invalid write path `{raw}`"));
            };
            rest.push(name.to_os_string());
            let Some(parent) = cursor.parent() else {
                return Err(format!("invalid write path `{raw}`"));
            };
            cursor = parent;
        }

        let mut resolved = fs::canonicalize(cursor)
            .map_err(|e| format!("cannot resolve parent of `{raw}`: {e}"))?;
        if !resolved.starts_with(&self.root) {
            return Err(format!("path `{raw}` escapes workspace root"));
        }
        for part in rest.into_iter().rev() {
            resolved.push(part);
        }
        Ok(resolved)
    }

    fn join_raw(&self, raw: &str) -> Result<PathBuf, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("path cannot be empty".to_string());
        }
        if trimmed.contains('\0') {
            return Err("path contains NUL byte".to_string());
        }
        let p = Path::new(trimmed);
        let mut out = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        // Normaliza componentes `.` y `..` antes de verificar existencia.
        let mut normalized = PathBuf::new();
        for comp in out.components() {
            match comp {
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(format!("path `{raw}` escapes workspace root"));
                    }
                }
                Component::CurDir => {}
                other => normalized.push(other.as_os_str()),
            }
        }
        out = normalized;
        if !out.starts_with(&self.root) {
            return Err(format!("path `{raw}` escapes workspace root"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod merge_delta_tests {
    use super::*;

    fn files(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_merge_delta_denies_control_plane_short_circuit() {
        // `.onyx/config.toml` → Deny, aunque vayan otras rutas inofensivas.
        assert_eq!(
            classify_merge_delta(&files(&[".onyx/config.toml", "src/main.rs"])),
            MergeDeltaVerdict::Deny
        );
        // `.env` / `.env.local` → Deny.
        assert_eq!(
            classify_merge_delta(&files(&[".env"])),
            MergeDeltaVerdict::Deny
        );
        assert_eq!(
            classify_merge_delta(&files(&[".env.local"])),
            MergeDeltaVerdict::Deny
        );
        // `.git/hooks/**` y `.git/config` → Deny.
        assert_eq!(
            classify_merge_delta(&files(&[".git/hooks/pre-commit"])),
            MergeDeltaVerdict::Deny
        );
        assert_eq!(
            classify_merge_delta(&files(&[".git/config"])),
            MergeDeltaVerdict::Deny
        );
        // `.onyx/vault/**` → Deny.
        assert_eq!(
            classify_merge_delta(&files(&[".onyx/vault/sess/a.tr"])),
            MergeDeltaVerdict::Deny
        );
        // Deny corta a la primera: no importa el orden ni las rutas Ask.
        assert_eq!(
            classify_merge_delta(&files(&["sprout.toml", ".env", "src/a.rs"])),
            MergeDeltaVerdict::Deny
        );
    }

    #[test]
    fn classify_merge_delta_asks_for_governance_paths() {
        // `sprout.toml` → Ask.
        assert_eq!(
            classify_merge_delta(&files(&["sprout.toml"])),
            MergeDeltaVerdict::Ask
        );
        // `.github/workflows/**` → Ask.
        assert_eq!(
            classify_merge_delta(&files(&[".github/workflows/ci.yml"])),
            MergeDeltaVerdict::Ask
        );
        // Ask + inofensivo sigue siendo Ask.
        assert_eq!(
            classify_merge_delta(&files(&["sprout.toml", "src/main.rs"])),
            MergeDeltaVerdict::Ask
        );
    }

    #[test]
    fn classify_merge_delta_safe_for_normal_sources() {
        assert_eq!(classify_merge_delta(&files(&[])), MergeDeltaVerdict::Safe);
        assert_eq!(
            classify_merge_delta(&files(&["src/main.rs", "docs/readme.md"])),
            MergeDeltaVerdict::Safe
        );
        // Un `workflows/` suelto (sin `.github/`) NO es gobernanza.
        assert_eq!(
            classify_merge_delta(&files(&["workflows/ci.yml"])),
            MergeDeltaVerdict::Safe
        );
        // Separadores Windows se normalizan.
        assert_eq!(
            classify_merge_delta(&files(&[".github\\workflows\\ci.yml"])),
            MergeDeltaVerdict::Ask
        );
        assert_eq!(
            classify_merge_delta(&files(&[".onyx\\config.toml"])),
            MergeDeltaVerdict::Deny
        );
    }
}

#[cfg(test)]
mod permission_rule_specificity_tests {
    use super::*;

    fn policy_with(deny: &[&str], ask: &[&str], allow: &[&str]) -> WorkspacePolicy {
        WorkspacePolicy::new(std::env::temp_dir())
            .expect("policy")
            .with_permissions(PermissionRules {
                deny: deny.iter().map(|s| s.to_string()).collect(),
                ask: ask.iter().map(|s| s.to_string()).collect(),
                allow: allow.iter().map(|s| s.to_string()).collect(),
            })
    }

    #[test]
    fn exact_allow_beats_generic_mcp_ask_glob() {
        // Regresión `SDD-04` Fase 3B: la coincidencia exacta de `tool` tiene
        // especificidad `tool.len() + 1`, por encima de cualquier glob coincidente.
        let p = policy_with(&[], &["mcp__mock__*"], &["mcp__mock__echo"]);
        assert_eq!(
            p.evaluate_permission_in_mode(WorkspaceMode::Build, "mcp__mock__echo", "{}"),
            PermissionDecision::Allow
        );
        // Un glob intermedio sigue pidiendo aprobación para el resto del servidor.
        assert_eq!(
            p.evaluate_permission_in_mode(WorkspaceMode::Build, "mcp__mock__big", "{}"),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn generic_ask_beats_generic_allow() {
        let p = policy_with(&[], &["*"], &["*"]);
        assert_eq!(
            p.evaluate_permission_in_mode(WorkspaceMode::Build, "fs_read", "{}"),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn exact_deny_beats_exact_allow_on_mcp_tool() {
        let p = policy_with(
            &["mcp__mock__echo"],
            &[],
            &["mcp__mock__echo", "mcp__mock__*"],
        );
        assert_eq!(
            p.evaluate_permission_in_mode(WorkspaceMode::Build, "mcp__mock__echo", "{}"),
            PermissionDecision::Deny
        );
    }
}
