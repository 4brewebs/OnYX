use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use serde_json::Value as JsonValue;

use crate::state::WorkspaceMode;

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
        let mut merged = Self::default();
        if let Some(gp) = global_path {
            if gp.is_file() {
                let raw = fs::read_to_string(gp)
                    .map_err(|e| format!("failed to read `{}`: {e}", gp.display()))?;
                let global = Self::parse_toml(&raw)?;
                merged.merge(global);
            }
        }
        if let Some(pp) = project_path {
            if pp.is_file() {
                let raw = fs::read_to_string(pp)
                    .map_err(|e| format!("failed to read `{}`: {e}", pp.display()))?;
                let project = Self::parse_toml(&raw)?;
                merged.merge(project);
            }
        }
        Ok(merged)
    }

    /// Combina otras reglas preservando todas las reglas `deny` existentes.
    pub fn merge(&mut self, other: Self) {
        self.deny.extend(other.deny);
        self.allow.extend(other.allow);
        self.ask.extend(other.ask);
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
                if is_mutating_tool(tool) {
                    PermissionDecision::Ask
                } else {
                    PermissionDecision::Allow
                }
            }
        }
    }
}

/// Indica si una herramienta realiza mutaciones sobre el workspace o ejecuta comandos de shell.
pub fn is_mutating_tool(tool: &str) -> bool {
    matches!(tool, "fs_write" | "fs_edit" | "shell_exec")
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

    if rule_tool != "*" && rule_tool != tool {
        return None;
    }

    let match_slash_for_star = tool == "shell_exec";
    let re = compile_glob_regex(subject_glob, match_slash_for_star).ok()?;
    if !re.is_match(subject) {
        return None;
    }

    let specificity = subject_glob
        .chars()
        .filter(|c| *c != '*' && *c != '?')
        .count();
    Some(specificity)
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
        })
    }

    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// Carga reglas `[permissions]` desde `~/.onyx/config.toml` y `<workspace>/.onyx/config.toml`.
    pub fn with_loaded_config(mut self) -> Self {
        self.permissions = PermissionRules::load_for_workspace(&self.root);
        self
    }

    /// Establece reglas de permisos declarativas explícitas.
    pub fn with_permissions(mut self, permissions: PermissionRules) -> Self {
        self.permissions = permissions;
        self
    }

    /// Permite ignorar reglas `deny` cuando el usuario pasa explícitamente `--ignore-deny`.
    pub fn with_ignore_deny(mut self, ignore_deny: bool) -> Self {
        self.ignore_deny = ignore_deny;
        self
    }

    /// Evalúa las reglas de permisos configuradas (sin tener en cuenta `WorkspaceMode`).
    pub fn evaluate_permission(&self, tool: &str, args_json: &str) -> PermissionDecision {
        self.permissions.evaluate(tool, args_json, self.ignore_deny)
    }

    /// Evalúa las reglas de permisos aplicando la invariante de seguridad de `WorkspaceMode`:
    /// en `Plan` o `Explore`, toda herramienta mutante queda siempre bloqueada (`Deny`),
    /// incluso si coincide con una regla `allow`.
    pub fn evaluate_permission_in_mode(
        &self,
        mode: WorkspaceMode,
        tool: &str,
        args_json: &str,
    ) -> PermissionDecision {
        if is_mutating_tool(tool) && !mode.allows_mutation() {
            return PermissionDecision::Deny;
        }
        self.evaluate_permission(tool, args_json)
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        rel.components().any(|c| {
            if let Component::Normal(name) = c {
                let s = name.to_string_lossy();
                let s_ref = s.as_ref();
                self.ignored_dirs.iter().any(|ignored| ignored == s_ref)
                    || s_ref == "snapshots"
                    || s_ref.starts_with("snaps_")
                    || s_ref.ends_with(".tr")
                    || s_ref.ends_with(".onyxpack")
                    || s_ref.ends_with(".onyxpack.tmp")
                    || (s_ref.starts_with("tick-") && s_ref.ends_with(".bin"))
            } else {
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
