//! Cliente MCP (Model Context Protocol) `stdio` — `SDD-04` Fase 3B (`REQ-04.3`/`REQ-04.4`).
//!
//! - **Parser hand-rolled 0-deps** de `[mcp.servers.<nombre>]` en `config.toml`
//!   (mismo patrón que `llm_profile`/`external_driver`): `transport` (`"stdio"`/`"http"`),
//!   `command`, `args`, `env` (tabla inline con referencias `${ENV_VAR}`), `url`,
//!   `timeout_secs` (default 20) y `enabled` (default true).
//! - **Trust gate**: la configuración de MCP del workspace se ignora (con advertencia)
//!   si el proyecto no está confiado; la global (`~/.onyx/config.toml`) siempre aplica.
//! - **JSON-RPC 2.0 sobre stdio** con hilo lector dedicado, timeout real por llamada
//!   (`mpsc::recv_timeout`) y limpieza del árbol de procesos al fallar (`ProcessJob`),
//!   Windows y Unix.
//! - **Aislamiento de fallos** en [`McpRegistry::discover_from_config`]: un servidor caído
//!   no aborta la sesión; cada servidor expone un [`McpServerStatus`] para el `doctor`
//!   de la Fase 4.
//! - **Seguridad**: `mcp__*` exige aprobación (`ToolSchema.requires_approval = true`);
//!   salidas y errores pasan por `truncate_utf8` + `scrub_secrets`; las referencias
//!   `${ENV_VAR}` solo se expanden al lanzar el proceso (nunca se loguean ni persisten).
//!
//! El transport `http` se acepta en el parser y reporta `McpServerStatus::HttpPending`
//! ("http transport pending") hasta la Fase 4.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value as JsonValue, json};

use crate::policy::{
    array_brackets_closed_pub, parse_toml_string_array_pub, strip_toml_comment_pub,
};
use crate::sandbox::{
    NetworkPolicy, ProcessJob, ProcessLimits, SandboxConfig, apply_sanitized_env,
    clean_process_path, configure_process_isolation, ensure_sandbox_tmp_dir,
    resolve_process_binary,
};
use crate::tools::{ToolSchema, scrub_secrets, truncate_utf8};

/// Timeout por defecto (segundos) de cada llamada JSON-RPC a un servidor MCP.
pub const MCP_DEFAULT_TIMEOUT_SECS: u64 = 20;

/// Límite de una línea en el `stdout` de un servidor MCP (4 MiB): más allá se rompe
/// la sesión para no crecer sinbound en memoria.
const MCP_MAX_STDOUT_LINE_BYTES: usize = 4 * 1024 * 1024;

/// `protocolVersion` declarado en el handshake `initialize` (SDD-04 Fase 3B).
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Paginación máxima de `tools/list` (`nextCursor`): evita bucles infinitos en
/// servidores maliciosos/defectuosos.
const MCP_MAX_TOOLS_PAGES: usize = 16;

// ────────────────────────────────────────────── configuración (parser) ──

/// Configuración de un servidor MCP (`[mcp.servers.<nombre>]`).
///
/// `Debug` está a mano para que `env` (valores `${VAR}`) y la query de `url`
/// nunca aparezcan en logs ni pánico.
#[derive(Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub transport: McpTransport,
    pub timeout_secs: u64,
    pub enabled: bool,
}

impl fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.debug_fields(f.debug_struct("McpServerConfig"))
    }
}

impl McpServerConfig {
    fn debug_fields(&self, mut f: fmt::DebugStruct<'_, '_>) -> fmt::Result {
        let _ = f
            .field("timeout_secs", &self.timeout_secs)
            .field("enabled", &self.enabled);
        match &self.transport {
            McpTransport::Stdio { command, args, env } => {
                let _ = f
                    .field("command", command)
                    .field("args", args)
                    // Valores de `env` son `${REF}`: aun así se redactan por precaución.
                    .field(
                        "env",
                        &env.iter()
                            .map(|(k, _)| format!("{k}=<redacted>"))
                            .collect::<Vec<_>>(),
                    );
            }
            McpTransport::Http { url } => {
                let _ = f.field("url", &redact_url_for_debug(url));
            }
        }
        Ok(())
    }
}

fn redact_url_for_debug(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _query)) => format!("{base}?<redacted>"),
        None => url.to_string(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransport {
    /// Proceso hijo sobre stdio. `env` son pares `(CLAVE, "${REF}")` que solo se
    /// expanden contra el entorno del proceso OnYX al momento del `spawn`.
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// Servidor remoto. Aceptado en el parser; ejecución pendiente de la Fase 4.
    Http { url: String },
}

#[derive(Debug)]
pub enum McpError {
    /// Fallo de transporte (spawn, escritura, disconnect, línea excesiva).
    Transport(String),
    /// La llamada venció su timeout: el proceso hijo se mata (árbol completo).
    Timeout { method: String, secs: u64 },
    /// El servidor respondió con un error JSON-RPC 2.0.
    Rpc { code: i64, message: String },
    /// El servidor respondió `isError: true` en `tools/call`.
    ServerError(String),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::Transport(s) => write!(f, "MCP transport error: {s}"),
            McpError::Timeout { method, secs } => {
                write!(f, "MCP timeout after {secs}s waiting for `{method}`")
            }
            McpError::Rpc { code, message } => {
                write!(f, "MCP JSON-RPC error {code}: {message}")
            }
            McpError::ServerError(m) => write!(f, "MCP tool returned an error: {m}"),
        }
    }
}

impl std::error::Error for McpError {}

/// Estado de un servidor tras [`McpRegistry::discover_from_config`] (alimenta el
/// `doctor` de la Fase 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerStatus {
    /// Conectado y con `tools/list` completado.
    Connected { tool_count: usize },
    /// `enabled = false` (global o del servidor).
    Disabled,
    /// Transport `http`: aceptado en el parser, ejecución pendiente de la Fase 4.
    HttpPending,
    /// Fallo concreto de handshake (spawn, timeout, error JSON-RPC).
    Failed(String),
}

impl McpServerStatus {
    pub fn is_connected(&self) -> bool {
        matches!(self, McpServerStatus::Connected { .. })
    }
}

/// Borrador de configuración MCP (pre-`finalize`): defaults `[mcp]` globales y
/// los bloques `[mcp.servers.<nombre>]` encontrados en un `config.toml`.
#[derive(Debug, Default)]
pub struct McpDraft {
    global_timeout_secs: Option<u64>,
    global_enabled: Option<bool>,
    servers: BTreeMap<String, PartialMcpServer>,
}

#[derive(Clone, Debug, Default)]
struct PartialMcpServer {
    line: usize,
    transport: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    url: Option<String>,
    timeout_secs: Option<u64>,
    enabled: Option<bool>,
}

impl PartialMcpServer {
    fn finalize(self, name: &str, draft: &McpDraft) -> Result<McpServerConfig, String> {
        let ctx = format!("config.toml line {} (`[mcp.servers.{name}]`)", self.line);
        let transport = self
            .transport
            .ok_or_else(|| format!("{ctx}: `transport` is required (\"stdio\" or \"http\")"))?;
        let transport = match transport.as_str() {
            "stdio" => {
                let command = self
                    .command
                    .filter(|c| !c.trim().is_empty())
                    .ok_or_else(|| {
                        format!("{ctx}: stdio transport requires a non-empty `command`")
                    })?;
                McpTransport::Stdio {
                    args: self.args.unwrap_or_default(),
                    env: self.env.unwrap_or_default().into_iter().collect(),
                    command,
                }
            }
            "http" => {
                let url = self
                    .url
                    .filter(|u| !u.trim().is_empty())
                    .ok_or_else(|| format!("{ctx}: http transport requires a non-empty `url`"))?;
                validate_mcp_url(&url).map_err(|e| format!("{ctx}: {e}"))?;
                McpTransport::Http { url }
            }
            other => {
                return Err(format!(
                    "{ctx}: unsupported transport `{other}` (supported: \"stdio\", \"http\")"
                ));
            }
        };
        if let McpTransport::Stdio { env, .. } = &transport {
            for (k, v) in env {
                if !is_env_ref(v) {
                    return Err(format!(
                        "{ctx}: `env[{k}]` must be a `${{ENV_VAR}}` reference, got `{v}`"
                    ));
                }
            }
        }
        Ok(McpServerConfig {
            timeout_secs: self
                .timeout_secs
                .or(draft.global_timeout_secs)
                .unwrap_or(MCP_DEFAULT_TIMEOUT_SECS),
            enabled: self.enabled.or(draft.global_enabled).unwrap_or(true),
            transport,
        })
    }
}

/// Validación de nombre de servidor: `[a-z0-9_-]+` y sin `__` (biyectividad del
/// prefijo `mcp__<server>__<tool>`).
fn validate_mcp_server_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("[mcp.servers.<nombre>]: empty server name".to_string());
    }
    if name.contains("__") {
        return Err(format!(
            "[mcp.servers.{name}]: server names must not contain `__` (reserved for the `mcp__<server>__<tool>` prefix)"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!(
            "[mcp.servers.{name}]: server names must match `[a-z0-9_-]+` (lowercase, digits, `_`, `-`)"
        ));
    }
    Ok(())
}

/// `url` debe ser `https://`, salvo hosts loopback (`127.0.0.1`, `localhost`,
/// `::1`) que pueden usar `http://`.
///
/// Hardening Fase 3B → 4A: la *authority* termina en el primer `/`, `?` o `#`;
/// el userinfo (`user@host`) queda prohibido porque ocultaría el host real
/// (p. ej. `http://localhost:80@evil.com/mcp`); los IPv6 van entre corchetes
/// (`http://[::1]:8787/mcp`).
fn validate_mcp_url(url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    let (rest, is_https) = if let Some(rest) = trimmed.strip_prefix("https://") {
        (rest, true)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        (rest, false)
    } else {
        return Err(format!(
            "`url` must start with `https://` (or loopback `http://`): `{url}`"
        ));
    };
    // Authority: hasta el primer `/`, `?` o `#`.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(format!("`url` has no host: `{url}`"));
    }
    if authority.contains('@') {
        return Err(format!(
            "userinfo (`@`) is not allowed in MCP `url` (it would hide the real host): `{url}`"
        ));
    }
    if is_https {
        return Ok(());
    }
    let bare_host = if let Some(inner) = authority.strip_prefix('[') {
        // IPv6 entre corchetes: `[::1]` o `[::1]:8787`.
        inner.split(']').next().unwrap_or("")
    } else {
        // `host:port` (IPv4/hostname): el host es todo lo que queda ANTES del
        // último `:` (la autoridad IPv4 nunca lleva más `:` que el del puerto).
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    if is_loopback_host(bare_host) {
        return Ok(());
    }
    Err(format!(
        "http transport requires `https://` (or `http://` on a loopback host): `{url}`"
    ))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "0:0:0:0:0:0:0:1")
}

/// Un valor de `env` válido es exactamente `${NOMBRE}` (identificador ASCII).
fn is_env_ref(value: &str) -> bool {
    let inner = value
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or("");
    !inner.is_empty() && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parsea el bloque MCP de un `config.toml` (global o workspace).
/// Devuelve el borrador; [`merge_mcp_configs`] convierte en configs finales.
pub(crate) fn parse_mcp_config(content: &str) -> Result<McpDraft, String> {
    // BOM UTF-8 (edición con Notepad & cía.): `read_to_string` lo conserva al
    // inicio de la cadena y rompería la primera línea (`[mcp]` etc.).
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = content.lines().collect();
    let mut draft = McpDraft::default();
    let mut section: Option<&str> = None; // "mcp" | nombre de servidor
    let mut i = 0;

    while i < lines.len() {
        let line_no = i + 1;
        let stripped = strip_toml_comment_pub(lines[i]).trim();
        i += 1;
        if stripped.is_empty() {
            continue;
        }
        if stripped.starts_with('[') && !stripped.starts_with("[[") {
            section = match stripped {
                "[mcp]" => Some("mcp"),
                other if other.ends_with(']') => {
                    let inner = &other[1..other.len() - 1];
                    if let Some(rest) = inner.strip_prefix("mcp.servers.") {
                        let name = rest.trim();
                        validate_mcp_server_name(name)
                            .map_err(|e| format!("config.toml line {line_no}: {e}"))?;
                        draft
                            .servers
                            .entry(name.to_string())
                            .or_insert_with(|| PartialMcpServer {
                                line: line_no,
                                ..Default::default()
                            });
                        Some(name)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            continue;
        }
        let Some(sec) = section else {
            continue; // clave fuera de [mcp]/[mcp.servers.*]
        };
        let (raw_key, raw_val) = stripped.split_once('=').ok_or_else(|| {
            format!("config.toml line {line_no}: expected `key = value` inside MCP config")
        })?;
        let key = raw_key.trim();
        let mut value_buf = raw_val.trim().to_string();

        // Arrays / tablas multi-línea: acumula hasta cerrar corchetes o llaves.
        if value_buf.starts_with('[') {
            while !array_brackets_closed_pub(&value_buf) {
                if i >= lines.len() {
                    return Err(format!(
                        "config.toml line {line_no}: unterminated array for `{key}`"
                    ));
                }
                let piece = strip_toml_comment_pub(lines[i]).trim();
                i += 1;
                if !piece.is_empty() {
                    value_buf.push(' ');
                    value_buf.push_str(piece);
                }
            }
        } else if value_buf.starts_with('{') {
            while !braces_closed(&value_buf) {
                if i >= lines.len() {
                    return Err(format!(
                        "config.toml line {line_no}: unterminated inline table for `{key}`"
                    ));
                }
                let piece = strip_toml_comment_pub(lines[i]).trim();
                i += 1;
                if !piece.is_empty() {
                    value_buf.push(' ');
                    value_buf.push_str(piece);
                }
            }
        }

        if sec == "mcp" {
            match key {
                "timeout_secs" => {
                    draft.global_timeout_secs = Some(parse_mcp_u64(&value_buf, line_no, key)?);
                }
                "enabled" => {
                    draft.global_enabled = Some(parse_mcp_bool(&value_buf, line_no, key)?);
                }
                other => {
                    return Err(format!(
                        "config.toml line {line_no}: unknown key `{other}` in `[mcp]` (expected `timeout_secs` or `enabled`)"
                    ));
                }
            }
            continue;
        }

        let name = sec;
        let Some(block) = draft.servers.get_mut(name) else {
            unreachable!("server `{name}` was registered at its header")
        };
        match key {
            "transport" => {
                if block.transport.is_some() {
                    return Err(mcp_dup_key_err(line_no, key));
                }
                block.transport = Some(parse_mcp_string(&value_buf, line_no, key)?);
            }
            "command" => {
                if block.command.is_some() {
                    return Err(mcp_dup_key_err(line_no, key));
                }
                block.command = Some(parse_mcp_string(&value_buf, line_no, key)?);
            }
            "args" => {
                if block.args.is_some() {
                    return Err(mcp_dup_key_err(line_no, key));
                }
                block.args = Some(
                    parse_toml_string_array_pub(&value_buf)
                        .map_err(|e| format!("config.toml line {line_no} (`{key}`): {e}"))?,
                );
            }
            "env" => {
                if block.env.is_some() {
                    return Err(mcp_dup_key_err(line_no, key));
                }
                block.env = Some(
                    parse_mcp_env_table(&value_buf, line_no)
                        .map_err(|e| format!("config.toml line {line_no} (`env`): {e}"))?,
                );
            }
            "url" => {
                if block.url.is_some() {
                    return Err(mcp_dup_key_err(line_no, key));
                }
                block.url = Some(parse_mcp_string(&value_buf, line_no, key)?);
            }
            "timeout_secs" => {
                if block.timeout_secs.is_some() {
                    return Err(mcp_dup_key_err(line_no, key));
                }
                block.timeout_secs = Some(parse_mcp_u64(&value_buf, line_no, key)?);
            }
            "enabled" => {
                if block.enabled.is_some() {
                    return Err(mcp_dup_key_err(line_no, key));
                }
                block.enabled = Some(parse_mcp_bool(&value_buf, line_no, key)?);
            }
            other => {
                return Err(format!(
                    "config.toml line {line_no}: unknown key `{other}` in `[mcp.servers.{name}]` (expected `transport`, `command`, `args`, `env`, `url`, `timeout_secs` or `enabled`)"
                ));
            }
        }
    }

    // Sin cabeceras `[mcp.*]` el borrador queda vacío (config sin MCP).
    Ok(draft)
}

fn mcp_dup_key_err(line_no: usize, key: &str) -> String {
    format!("config.toml line {line_no}: duplicate key `{key}` in MCP block")
}

fn parse_mcp_u64(raw: &str, line_no: usize, key: &str) -> Result<u64, String> {
    raw.parse::<u64>().map_err(|_| {
        format!("config.toml line {line_no}: `{key}` must be a non-negative integer, got `{raw}`")
    })
}

fn parse_mcp_bool(raw: &str, line_no: usize, key: &str) -> Result<bool, String> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!(
            "config.toml line {line_no}: `{key}` must be `true`/`false`, got `{other}`"
        )),
    }
}

/// Cadena TOML con escapes (duplica la semántica de `external_driver::parse_toml_string`).
fn parse_mcp_string(raw: &str, line_no: usize, key: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        return Ok(inner.to_string());
    }
    if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => {
                        return Err(format!(
                            "config.toml line {line_no} (`{key}`): unterminated escape sequence"
                        ));
                    }
                }
            } else {
                out.push(c);
            }
        }
        return Ok(out);
    }
    Err(format!(
        "config.toml line {line_no} (`{key}`): expected quoted string, found `{trimmed}`"
    ))
}

/// Tabla inline `env = { K = "${V}", ... }` (una línea o varias hasta cerrar llaves).
fn parse_mcp_env_table(raw: &str, line_no: usize) -> Result<BTreeMap<String, String>, String> {
    let inner = raw
        .trim()
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or("expected an inline table `{ ... }`")?;
    let mut out = BTreeMap::new();
    for part in split_top_level(inner, ',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| format!("expected `KEY = \"value\"`, got `{part}`"))?;
        let k = k.trim();
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("invalid env key `{k}`"));
        }
        let v = parse_mcp_string(v, line_no, "env")?;
        if out.insert(k.to_string(), v).is_some() {
            return Err(format!("duplicate env key `{k}`"));
        }
    }
    Ok(out)
}

/// Divide en el separador indicado ignorando lo que esté dentro de cadenas dobles.
fn split_top_level(input: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in input.chars() {
        if in_string {
            cur.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                cur.push(c);
            }
            c if c == sep => {
                out.push(cur);
                cur = String::new();
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// `buf` tiene todas sus llaves balanceadas (ignorando llaves dentro de cadenas).
fn braces_closed(buf: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for c in buf.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

/// Fusión `replace-by-name` con precedencia `global < workspace (trusted)`.
pub fn merge_mcp_configs(
    global: McpDraft,
    workspace: McpDraft,
) -> Result<BTreeMap<String, McpServerConfig>, String> {
    let mut merged = BTreeMap::new();
    let all = {
        let mut names: Vec<&String> = global
            .servers
            .keys()
            .chain(workspace.servers.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        names.sort();
        names
    };
    for name in all {
        let draft = if workspace.servers.contains_key(name) {
            &workspace
        } else {
            &global
        };
        let block = draft.servers.get(name).expect("name came from draft");
        let cfg = block.clone().finalize(name, draft)?;
        merged.insert(name.to_string(), cfg);
    }
    Ok(merged)
}

/// Carga y fusiona la configuración MCP desde `~/.onyx/config.toml` (global) y
/// `<workspace>/.onyx/config.toml` aplicando el gate de Workspace Trust
/// (mismo patrón que `load_external_drivers`).
///
/// Devuelve `(configs, warnings)`. Con `trust_project == false` y existir
/// configuración MCP de workspace, esta se ignora por completo y se devuelve
/// una advertencia.
pub fn load_mcp_config(
    global_root: Option<&Path>,
    workspace_root: Option<&Path>,
    trust_project: bool,
) -> Result<(BTreeMap<String, McpServerConfig>, Vec<String>), String> {
    let mut warnings = Vec::new();
    let mut global = McpDraft::default();
    if let Some(gr) = global_root {
        let path = gr.join(".onyx").join("config.toml");
        if path.is_file() {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read `{}`: {e}", path.display()))?;
            global = parse_mcp_config(&raw)?;
        }
    }

    let mut workspace = McpDraft::default();
    if let Some(wr) = workspace_root {
        let path = wr.join(".onyx").join("config.toml");
        if path.is_file() {
            if !trust_project {
                warnings.push(format!(
                    "MCP config in `{}` ignored: workspace is not trusted (use `--trust-workspace` or `openonyx trust`)"
                        ,
                    path.display()
                ));
            } else {
                let raw = std::fs::read_to_string(&path)
                    .map_err(|e| format!("failed to read `{}`: {e}", path.display()))?;
                workspace = parse_mcp_config(&raw)?;
            }
        }
        if wr.join(".mcp.json").is_file() {
            warnings.push(
                "`.mcp.json` detected: JSON MCP config is pending (SDD-04 Fase 4); it is ignored for now"
                    .to_string(),
            );
        }
    }

    let configs = merge_mcp_configs(global, workspace)?;
    Ok((configs, warnings))
}

// ────────────────────────────────────────────── cliente stdio ──

/// Cliente MCP sobre un transport. `Send + Sync` para compartir vía `Arc` entre
/// el despacho de herramientas y el `doctor`.
pub trait McpClient: Send + Sync {
    /// Handshake completo: spawn (si hace falta), `initialize`,
    /// `notifications/initialized` y `tools/list` (con `nextCursor` acotado).
    /// Idempotente: si ya hay sesión activa devuelve los schemas cacheados.
    fn handshake(&self) -> Result<Vec<ToolSchema>, McpError>;

    /// `tools/call` sobre la herramienta `tool` del servidor.
    fn call_tool(&self, tool: &str, args: &JsonValue) -> Result<String, McpError>;
}

struct McpSession {
    child: Child,
    job: Option<ProcessJob>,
    stdin: ChildStdin,
    next_id: u64,
    inbox: mpsc::Receiver<InboxMsg>,
}

enum InboxMsg {
    /// Una línea JSON-RPC válida parseada del `stdout` del servidor.
    Response(JsonValue),
    /// Fallo de lectura o línea por encima del límite: la sesión debe morir.
    Broken(String),
}

impl McpSession {
    /// Mata el proceso (y su árbol vía `ProcessJob`) y libera la sesión. El `wait`
    /// tras el `kill` evita dejar zombis en Unix (el `Child` es recolectado aquí).
    fn kill(&mut self) {
        if let Some(job) = self.job.as_mut() {
            job.terminate();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Cliente MCP `stdio`: proceso hijo con stdio pipeado, hilo lector dedicado y
/// timeout real por llamada.
pub struct StdioMcpClient {
    name: String,
    cfg: McpServerConfig,
    session: Mutex<Option<McpSession>>,
    /// `None` hasta que el handshake ha completado; `Some(tools)` son los
    /// schemas cacheados de ese handshake.
    cached_tools: Mutex<Option<Vec<ToolSchema>>>,
}

impl fmt::Debug for StdioMcpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StdioMcpClient")
            .field("name", &self.name)
            .field("config", &self.cfg)
            .finish()
    }
}

impl StdioMcpClient {
    pub fn new(name: String, cfg: McpServerConfig) -> Arc<Self> {
        Arc::new(Self {
            name,
            cfg,
            session: Mutex::new(None),
            cached_tools: Mutex::new(None),
        })
    }

    fn spawn_session(&self) -> Result<McpSession, McpError> {
        let McpTransport::Stdio { command, args, env } = &self.cfg.transport else {
            return Err(McpError::Transport(format!(
                "MCP server `{}`: not a stdio transport",
                self.name
            )));
        };
        // Resolución explícita de `command` contra `PATH`/`PATHEXT` **antes** de
        // `Command::new` (misma vía que `external_driver`/`sandbox.rs`): los shims
        // `npx`/`uvx`/`.cmd` funcionan en Windows y Linux.
        let resolved = resolve_process_binary(command).ok_or_else(|| {
            McpError::Transport(format!(
                "MCP server `{}`: command `{command}` was not resolved against PATH{} (install it or use an absolute path)",
                self.name,
                if cfg!(windows) { "/PATHEXT" } else { "" }
            ))
        })?;
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let sandbox_tmp = ensure_sandbox_tmp_dir(&workspace_root)
            .map_err(|e| McpError::Transport(format!("MCP server `{}`: {e}", self.name)))?;
        let mut cmd = Command::new(&resolved.path);
        cmd.args(args)
            .current_dir(clean_process_path(&workspace_root))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // `stderr` a la nada: un servidor defectuoso podría volcar secretos a
            // su stderr; nunca lo capturamos.
            .stderr(Stdio::null());
        // Entorno base seguro (misma vía que `sandbox.rs::apply_sanitized_env`):
        // solo la allowlist de runtime/toolchain (`PATH`, `PATHEXT`, `COMSPEC`, ...),
        // sin heredar credenciales; encima se añaden únicamente los `env` explícitos
        // del bloque MCP (expandidos `${REF}`).
        let sandbox_cfg = SandboxConfig {
            network: NetworkPolicy::Unrestricted,
            ..SandboxConfig::default()
        };
        apply_sanitized_env(&mut cmd, &sandbox_cfg, &workspace_root, &sandbox_tmp);
        for (k, v) in env {
            let expanded = expand_env_ref(v).ok_or_else(|| {
                McpError::Transport(format!(
                    "MCP server `{}`: env reference `{v}` is missing or malformed",
                    self.name
                ))
            })?;
            cmd.env(k, expanded);
        }
        configure_process_isolation(&mut cmd, true);
        let mut child = cmd.spawn().map_err(|e| {
            McpError::Transport(format!(
                "MCP server `{}`: failed to spawn `{}`: {e}",
                self.name,
                resolved.path.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("no stdin pipe".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("no stdout pipe".to_string()))?;
        let mut job = ProcessJob::assign(&child, ProcessLimits::default());

        let (tx, rx) = mpsc::channel::<InboxMsg>();
        std::thread::Builder::new()
            .name(format!("mcp-{}", self.name))
            .spawn(move || mcp_reader_loop(stdout, tx))
            .map_err(|e| {
                if let Some(job) = job.as_mut() {
                    job.terminate();
                }
                let _ = child.kill();
                let _ = child.wait();
                McpError::Transport(format!("failed to start reader thread: {e}"))
            })?;

        Ok(McpSession {
            next_id: 1,
            child,
            job,
            stdin,
            inbox: rx,
        })
    }

    /// Una llamada JSON-RPC esperando su respuesta (descarta notificaciones y
    /// respuestas con otros `id`). Timeout real: si vence, mata el proceso.
    fn rpc(
        &self,
        method: &str,
        params: JsonValue,
        timeout_secs: u64,
    ) -> Result<JsonValue, McpError> {
        let mut guard = self.session.lock().unwrap();
        let session = guard.as_mut().ok_or_else(|| {
            McpError::Transport(format!("MCP server `{}`: not connected", self.name))
        })?;
        let id = session.next_id;
        session.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let mut bytes = request.to_string().into_bytes();
        bytes.push(b'\n');
        if let Err(e) = session
            .stdin
            .write_all(&bytes)
            .and_then(|_| session.stdin.flush())
        {
            drop_session_inplace(&mut guard);
            return Err(McpError::Transport(format!(
                "MCP server `{}`: failed to write `{method}`: {e}",
                self.name
            )));
        }
        // Deadline absoluto ANTES del loop: las notificaciones periódicas del
        // servidor no pueden prolongar el timeout (cada `recv_timeout` consume
        // solo el resto de presupuesto).
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop_session_inplace(&mut guard);
                return Err(McpError::Timeout {
                    method: method.to_string(),
                    secs: timeout_secs.max(1),
                });
            }
            match session.inbox.recv_timeout(remaining) {
                Ok(InboxMsg::Response(v)) if v.get("id").and_then(|i| i.as_u64()) == Some(id) => {
                    return extract_jsonrpc_result(&v, method);
                }
                Ok(InboxMsg::Response(_)) => {} // notificación u otro id: se ignora
                Ok(InboxMsg::Broken(reason)) => {
                    drop_session_inplace(&mut guard);
                    return Err(McpError::Transport(format!(
                        "MCP server `{}`: stdio broken: {reason}",
                        self.name
                    )));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    drop_session_inplace(&mut guard);
                    return Err(McpError::Timeout {
                        method: method.to_string(),
                        secs: timeout_secs.max(1),
                    });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    drop_session_inplace(&mut guard);
                    return Err(McpError::Transport(format!(
                        "MCP server `{}`: reader thread exited",
                        self.name
                    )));
                }
            }
        }
    }

    fn send_notification(&self, method: &str) -> Result<(), McpError> {
        let mut guard = self.session.lock().unwrap();
        let Some(session) = guard.as_mut() else {
            return Err(McpError::Transport(format!(
                "MCP server `{}`: not connected",
                self.name
            )));
        };
        let request = json!({ "jsonrpc": "2.0", "method": method });
        let mut bytes = request.to_string().into_bytes();
        bytes.push(b'\n');
        if let Err(e) = session
            .stdin
            .write_all(&bytes)
            .and_then(|_| session.stdin.flush())
        {
            drop_session_inplace(&mut guard);
            return Err(McpError::Transport(format!(
                "MCP server `{}`: failed to send `{method}`: {e}",
                self.name
            )));
        }
        Ok(())
    }
}

/// En fallo terminal de `rpc` (write/broken/timeout/disconnect) mata el proceso
/// y desecha la sesión: no se reutiliza un servidor cuya línea se rompió.
fn drop_session_inplace(guard: &mut Option<McpSession>) {
    if let Some(session) = guard.as_mut() {
        session.kill();
    }
    *guard = None;
}

impl McpClient for StdioMcpClient {
    fn handshake(&self) -> Result<Vec<ToolSchema>, McpError> {
        // Idempotencia: sesión viva + handshake ya completado (`Some`) → sin
        // trabajo (aunque el servidor exponga 0 tools).
        {
            let cached = self.cached_tools.lock().unwrap();
            if cached.is_some() && self.session.lock().unwrap().is_some() {
                return Ok(cached.clone().unwrap_or_default());
            }
        }
        {
            let mut guard = self.session.lock().unwrap();
            if guard.is_none() {
                let session = self.spawn_session().inspect_err(|_| *guard = None)?;
                *guard = Some(session);
            }
        }
        // El handshake post-spawn; si falla **después** del spawn, no dejamos
        // una sesión a medio hacer (la siguiente llamada reutilizaría una
        // conexión rota).
        let outcome = (|| -> Result<Vec<ToolSchema>, McpError> {
            let timeout = self.cfg.timeout_secs;
            self.rpc(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "onyx", "version": "0.1" },
                }),
                timeout,
            )?;
            self.send_notification("notifications/initialized")?;

            // tools/list con paginación `nextCursor` acotada.
            let mut tools: Vec<ToolSchema> = Vec::new();
            let mut cursor: Option<String> = None;
            for _page in 0..MCP_MAX_TOOLS_PAGES {
                let params = match &cursor {
                    Some(c) => json!({ "cursor": c }),
                    None => json!({}),
                };
                let result = self.rpc("tools/list", params, timeout)?;
                let list = result
                    .get("tools")
                    .and_then(|t| t.as_array())
                    .ok_or_else(|| {
                        McpError::Transport(format!(
                            "MCP server `{}`: tools/list returned no `tools` array",
                            self.name
                        ))
                    })?;
                for item in list {
                    let name = item
                        .get("name")
                        .and_then(|n| n.as_str())
                        .filter(|n| !n.is_empty() && !n.contains("__"))
                        .ok_or_else(|| {
                            McpError::Transport(format!(
                                "MCP server `{}`: malformed tool name (empty or contains `__`)",
                                self.name
                            ))
                        })?
                        .to_string();
                    let description = item
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string();
                    let parameters = item
                        .get("inputSchema")
                        .filter(|s| s.is_object())
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                    // REQ-04.4: toda herramienta MCP requiere aprobación por defecto.
                    tools.push(ToolSchema {
                        name: Cow::Owned(format!("mcp__{}__{}", self.name, name)),
                        description: Cow::Owned(description),
                        parameters,
                        requires_approval: true,
                    });
                }
                match result.get("nextCursor").and_then(|c| c.as_str()) {
                    Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                    _ => break,
                }
            }
            *self.cached_tools.lock().unwrap() = Some(tools.clone());
            Ok(tools)
        })();
        match outcome {
            Ok(tools) => Ok(tools),
            Err(e) => {
                let mut guard = self.session.lock().unwrap();
                if let Some(mut session) = guard.take() {
                    session.kill();
                }
                Err(e)
            }
        }
    }

    fn call_tool(&self, tool: &str, args: &JsonValue) -> Result<String, McpError> {
        let result = self.rpc(
            "tools/call",
            json!({ "name": tool, "arguments": args }),
            self.cfg.timeout_secs,
        )?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let text = mcp_content_to_text(&result)?;
        if is_error {
            return Err(McpError::ServerError(text));
        }
        Ok(text)
    }
}

impl Drop for StdioMcpClient {
    fn drop(&mut self) {
        let mut guard = self.session.lock().unwrap();
        if let Some(mut session) = guard.take() {
            session.kill();
        }
    }
}

/// Hilo lector: posee el `stdout` y envía cada línea JSON al canal. Líneas no
/// JSON se ignoran (tolerancia a servidores que loguean); una línea > 4 MiB o un
/// fallo de I/O rompen la sesión.
fn mcp_reader_loop(stdout: ChildStdout, tx: mpsc::Sender<InboxMsg>) {
    let mut reader = std::io::BufReader::new(stdout);
    loop {
        match read_bounded_line(&mut reader, MCP_MAX_STDOUT_LINE_BYTES) {
            Ok(Some(line)) => {
                let text = String::from_utf8_lossy(&line);
                if let Ok(v) = serde_json::from_str::<JsonValue>(text.trim()) {
                    if tx.send(InboxMsg::Response(v)).is_err() {
                        return;
                    }
                }
            }
            Ok(None) => return, // EOF: el servidor cerró su stdout
            Err(e) => {
                let _ = tx.send(InboxMsg::Broken(e.to_string()));
                return;
            }
        }
    }
}

/// Lee una línea (sin el `\n`) con tope duro de `cap` bytes usando
/// `fill_buffer`/`consume` (sin copias ni pérdida de bytes entre líneas).
fn read_bounded_line(
    reader: &mut std::io::BufReader<ChildStdout>,
    cap: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    loop {
        let avail = reader.fill_buf()?;
        if avail.is_empty() {
            return Ok(if buf.is_empty() { None } else { Some(buf) });
        }
        match avail.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                // El `\n` puede llegar en un chunk posterior a varios sin
                // newline: validar el tope también aquí, no solo en `None`.
                if buf.len() + pos > cap {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("line exceeds {cap} bytes"),
                    ));
                }
                buf.extend_from_slice(&avail[..pos]);
                reader.consume(pos + 1);
                return Ok(Some(buf));
            }
            None => {
                let n = avail.len();
                if buf.len() + n > cap {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("line exceeds {cap} bytes"),
                    ));
                }
                buf.extend_from_slice(avail);
                reader.consume(n);
            }
        }
    }
}

/// Expande `${NOMBRE}` contra el entorno del proceso OnYX. Solo se invoca en el
/// momento del `spawn`; el valor nunca se loguea ni persiste.
fn expand_env_ref(value: &str) -> Option<String> {
    let inner = value.strip_prefix("${")?.strip_suffix('}')?;
    if !is_env_ref(value) {
        return None;
    }
    std::env::var(inner).ok()
}

/// Extrae `result` de una respuesta JSON-RPC 2.0, mapeando el miembro `error`.
fn extract_jsonrpc_result(v: &JsonValue, method: &str) -> Result<JsonValue, McpError> {
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("<no message>")
            .to_string();
        return Err(McpError::Rpc {
            code,
            message: scrub_secrets(&message),
        });
    }
    v.get("result").cloned().ok_or_else(|| {
        McpError::Transport(format!(
            "response to `{method}` has neither `result` nor `error`"
        ))
    })
}

/// `result.content` de `tools/call`: join de los bloques `text` con `\n`.
fn mcp_content_to_text(result: &JsonValue) -> Result<String, McpError> {
    let content = result
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| {
            McpError::Transport("tools/call result has no `content` array".to_string())
        })?;
    let mut parts: Vec<&str> = Vec::new();
    for item in content {
        if item.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                parts.push(text);
            }
        }
    }
    Ok(parts.join("\n"))
}

/// Separa `mcp__<server>__<tool>` en `(server, tool)`. `None` si el prefijo o el
/// separador no están (biyectividad garantizada por el charset del parser).
pub(crate) fn split_prefixed_name(prefixed: &str) -> Option<(&str, &str)> {
    let rest = prefixed.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

// ────────────────────────────────────────────── registro ──

struct McpServerHandle {
    name: String,
    status: McpServerStatus,
    /// Cliente genérico por transporte (`StdioMcpClient` hoy; `HttpMcpClient`
    /// en la Fase 4B) para que el despacho no dependa del transporte.
    client: Option<Arc<dyn McpClient>>,
    tools: Vec<ToolSchema>,
}

/// Registro de servidores MCP descubiertos con aislamiento de fallos: cada
/// servidor se handshakea por separado y un fallo solo marca `Failed` para ese
/// servidor, sin abortar la sesión.
pub struct McpRegistry {
    max_output_bytes: usize,
    servers: Vec<McpServerHandle>,
}

impl fmt::Debug for McpRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpRegistry")
            .field(
                "servers",
                &self
                    .servers
                    .iter()
                    .map(|h| (h.name.as_str(), &h.status))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl McpRegistry {
    /// Descubre los servidores de `configs` (orden determinista por nombre).
    /// `stdio` → handshake completo; `http` → `HttpPending`; `enabled = false` →
    /// `Disabled`. Ningún fallo aborta la función.
    pub fn discover_from_config(
        configs: &BTreeMap<String, McpServerConfig>,
        max_output_bytes: usize,
    ) -> Self {
        let mut servers = Vec::with_capacity(configs.len());
        for (name, cfg) in configs {
            if !cfg.enabled {
                servers.push(McpServerHandle {
                    name: name.clone(),
                    status: McpServerStatus::Disabled,
                    client: None,
                    tools: Vec::new(),
                });
                continue;
            }
            if matches!(cfg.transport, McpTransport::Http { .. }) {
                servers.push(McpServerHandle {
                    name: name.clone(),
                    status: McpServerStatus::HttpPending,
                    client: None,
                    tools: Vec::new(),
                });
                continue;
            }
            let client = StdioMcpClient::new(name.clone(), cfg.clone());
            match client.handshake() {
                Ok(tools) => servers.push(McpServerHandle {
                    name: name.clone(),
                    status: McpServerStatus::Connected {
                        tool_count: tools.len(),
                    },
                    client: Some(client),
                    tools,
                }),
                Err(e) => {
                    // Aislamiento: el servidor caído se descarta (su `Drop` mata
                    // el proceso si llegó a spawnearse) y los demás siguen.
                    servers.push(McpServerHandle {
                        name: name.clone(),
                        status: McpServerStatus::Failed(scrub_secrets(&e.to_string())),
                        client: None,
                        tools: Vec::new(),
                    });
                }
            }
        }
        McpRegistry {
            max_output_bytes,
            servers,
        }
    }

    /// Schemas `mcp__*` de los servidores conectados (para inyectar al LLM).
    pub fn tool_schemas(&self) -> Vec<ToolSchema> {
        self.servers
            .iter()
            .flat_map(|h| h.tools.iter().cloned())
            .collect()
    }

    /// Estados por servidor (para el `doctor` de la Fase 4).
    pub fn server_statuses(&self) -> Vec<(String, McpServerStatus)> {
        self.servers
            .iter()
            .map(|h| (h.name.clone(), h.status.clone()))
            .collect()
    }

    /// `tools/call` sobre `mcp__<server>__<tool>`. El resultado (y los mensajes
    /// de error que transportan texto del servidor) pasan por `truncate_utf8` +
    /// `scrub_secrets`.
    pub fn call(&self, prefixed: &str, args: &JsonValue) -> Result<String, McpError> {
        let (server, tool) = split_prefixed_name(prefixed).ok_or_else(|| {
            McpError::Transport(format!(
                "malformed MCP tool name `{prefixed}` (expected `mcp__<server>__<tool>`) (SDD-04)"
            ))
        })?;
        let handle = self
            .servers
            .iter()
            .find(|h| h.name == server)
            .ok_or_else(|| {
                McpError::Transport(format!("unknown MCP server `{server}` (SDD-04)"))
            })?;
        let client = handle.client.as_ref().ok_or_else(|| {
            McpError::Transport(format!(
                "MCP server `{server}` is not connected: {}",
                match &handle.status {
                    McpServerStatus::Failed(msg) => msg.clone(),
                    McpServerStatus::Disabled => "disabled".to_string(),
                    McpServerStatus::HttpPending => "http transport pending".to_string(),
                    McpServerStatus::Connected { .. } => "no client".to_string(),
                }
            ))
        })?;
        let raw = client
            .call_tool(tool, args)
            .map_err(|e| self.scrub_error(e))?;
        Ok(scrub_secrets(&truncate_utf8(&raw, self.max_output_bytes)))
    }

    fn scrub_error(&self, e: McpError) -> McpError {
        match e {
            McpError::Rpc { code, message } => McpError::Rpc {
                code,
                message: scrub_secrets(&truncate_utf8(&message, self.max_output_bytes)),
            },
            McpError::ServerError(m) => {
                McpError::ServerError(scrub_secrets(&truncate_utf8(&m, self.max_output_bytes)))
            }
            other => other,
        }
    }
}

// ────────────────────────────────────────────── tests unitarios ──

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_cfg(command: &str) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransport::Stdio {
                command: command.to_string(),
                args: Vec::new(),
                env: Vec::new(),
            },
            timeout_secs: MCP_DEFAULT_TIMEOUT_SECS,
            enabled: true,
        }
    }

    #[test]
    fn parser_accepts_stdio_server_with_env_and_defaults() {
        let content = r#"
[mcp.servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
"#;
        let draft = parse_mcp_config(content).expect("parse");
        let merged = merge_mcp_configs(McpDraft::default(), draft).expect("merge");
        let cfg = merged.get("github").expect("github present");
        match &cfg.transport {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(
                    args,
                    &vec![
                        "-y".to_string(),
                        "@modelcontextprotocol/server-github".to_string()
                    ]
                );
                assert_eq!(
                    env,
                    &vec![("GITHUB_TOKEN".to_string(), "${GITHUB_TOKEN}".to_string())]
                );
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        assert_eq!(cfg.timeout_secs, MCP_DEFAULT_TIMEOUT_SECS);
        assert!(cfg.enabled);
    }

    #[test]
    fn parser_accepts_http_server_with_https_url() {
        let content = r#"
[mcp.servers.remote]
transport = "http"
url = "https://mcp.example.com/sse"
timeout_secs = 5
enabled = false
"#;
        let draft = parse_mcp_config(content).expect("parse");
        let merged = merge_mcp_configs(McpDraft::default(), draft).expect("merge");
        let cfg = merged.get("remote").expect("remote present");
        assert!(matches!(
            cfg.transport,
            McpTransport::Http {
                ref url
            } if url == "https://mcp.example.com/sse"
        ));
        assert_eq!(cfg.timeout_secs, 5);
        assert!(!cfg.enabled);
    }

    #[test]
    fn parser_accepts_loopback_http_url() {
        let content = r#"
[mcp.servers.local]
transport = "http"
url = "http://127.0.0.1:3320/mcp"
"#;
        let draft = parse_mcp_config(content).expect("parse");
        assert!(merge_mcp_configs(McpDraft::default(), draft).is_ok());
    }

    #[test]
    fn parser_rejects_non_loopback_http_url() {
        let content = r#"
[mcp.servers.bad]
transport = "http"
url = "http://mcp.example.com/sse"
"#;
        let draft = parse_mcp_config(content).expect("parse");
        let err = merge_mcp_configs(McpDraft::default(), draft).unwrap_err();
        assert!(err.contains("requires `https://`"), "{err}");
    }

    #[test]
    fn url_accepts_ipv6_loopback_in_brackets() {
        assert!(validate_mcp_url("http://127.0.0.1:3320/mcp").is_ok());
        assert!(validate_mcp_url("http://[::1]:8787/mcp").is_ok());
        assert!(validate_mcp_url("http://[0:0:0:0:0:0:0:1]/mcp").is_ok());
        // La query/fragment terminan la authority (host sigue siendo loopback).
        assert!(validate_mcp_url("http://127.0.0.1?x=1").is_ok());
        assert!(validate_mcp_url("http://127.0.0.1#frag").is_ok());
    }

    #[test]
    fn url_rejects_userinfo_and_non_loopback_ipv6() {
        // `localhost:80@evil.com`: el userinfo ocultaría el host real.
        let err = validate_mcp_url("http://localhost:80@evil.com/mcp").unwrap_err();
        assert!(err.contains("userinfo"), "{err}");
        let err = validate_mcp_url("http://[::1]:80@evil.com/mcp").unwrap_err();
        assert!(err.contains("userinfo"), "{err}");
        // El userinfo se rechaza también con `https://`.
        assert!(validate_mcp_url("https://admin@mcp.example.com/sse").is_err());
        // IPv6 no loopback: `http://` no es admisible.
        let err = validate_mcp_url("http://[2001:db8::1]:8787/mcp").unwrap_err();
        assert!(err.contains("requires `https://`"), "{err}");
        // …mientras que con `https://` sí.
        assert!(validate_mcp_url("https://[2001:db8::1]:8787/mcp").is_ok());
    }

    #[test]
    fn url_rejects_empty_authority() {
        assert!(validate_mcp_url("http://").is_err());
        assert!(validate_mcp_url("https://").is_err());
        assert!(validate_mcp_url("ftp://127.0.0.1:8787").is_err());
    }

    #[test]
    fn parser_rejects_unknown_transport() {
        let content = r#"
[mcp.servers.bad]
transport = "websocket"
command = "x"
"#;
        let draft = parse_mcp_config(content).expect("parse");
        let err = merge_mcp_configs(McpDraft::default(), draft).unwrap_err();
        assert!(err.contains("unsupported transport"), "{err}");
    }

    #[test]
    fn parser_rejects_stdio_without_command() {
        let content = r#"
[mcp.servers.bad]
transport = "stdio"
"#;
        let draft = parse_mcp_config(content).expect("parse");
        let err = merge_mcp_configs(McpDraft::default(), draft).unwrap_err();
        assert!(err.contains("requires a non-empty `command`"), "{err}");
    }

    #[test]
    fn parser_rejects_env_value_not_a_ref() {
        let content = r#"
[mcp.servers.bad]
transport = "stdio"
command = "x"
env = { TOKEN = "literal-secret" }
"#;
        let draft = parse_mcp_config(content).expect("parse");
        let err = merge_mcp_configs(McpDraft::default(), draft).unwrap_err();
        assert!(err.contains("must be a `${ENV_VAR}` reference"), "{err}");
    }

    #[test]
    fn parser_rejects_server_name_with_double_underscore() {
        let content = r#"
[mcp.servers.bad__name]
transport = "stdio"
command = "x"
"#;
        let err = parse_mcp_config(content).unwrap_err();
        assert!(err.contains("must not contain `__`"), "{err}");
    }

    #[test]
    fn parser_rejects_uppercase_server_name() {
        let content = r#"
[mcp.servers.GitHub]
transport = "stdio"
command = "x"
"#;
        let err = parse_mcp_config(content).unwrap_err();
        assert!(err.contains("[a-z0-9_-]+"), "{err}");
    }

    #[test]
    fn parser_rejects_duplicate_key() {
        let content = r#"
[mcp.servers.a]
transport = "stdio"
command = "x"
command = "y"
"#;
        let err = parse_mcp_config(content).unwrap_err();
        assert!(err.contains("duplicate key"), "{err}");
    }

    #[test]
    fn parser_rejects_unknown_key() {
        let content = r#"
[mcp.servers.a]
transport = "stdio"
command = "x"
bogus = 1
"#;
        let err = parse_mcp_config(content).unwrap_err();
        assert!(err.contains("unknown key"), "{err}");
    }

    #[test]
    fn parser_global_defaults_apply() {
        let content = r#"
[mcp]
timeout_secs = 9
enabled = false

[mcp.servers.a]
transport = "stdio"
command = "x"
"#;
        let draft = parse_mcp_config(content).expect("parse");
        let merged = merge_mcp_configs(McpDraft::default(), draft).expect("merge");
        let cfg = merged.get("a").expect("a present");
        assert_eq!(cfg.timeout_secs, 9);
        assert!(!cfg.enabled);
    }

    #[test]
    fn merge_is_replace_by_name_workspace_wins() {
        let global = parse_mcp_config(
            r#"
[mcp.servers.shared]
transport = "stdio"
command = "global-cmd"

[mcp.servers.gonly]
transport = "stdio"
command = "g"
"#,
        )
        .expect("global parse");
        let workspace = parse_mcp_config(
            r#"
[mcp.servers.shared]
transport = "stdio"
command = "workspace-cmd"
"#,
        )
        .expect("workspace parse");
        let merged = merge_mcp_configs(global, workspace).expect("merge");
        assert_eq!(merged.len(), 2);
        let shared = merged.get("shared").expect("shared");
        match &shared.transport {
            McpTransport::Stdio { command, .. } => assert_eq!(command, "workspace-cmd"),
            other => panic!("expected stdio, got {other:?}"),
        }
        assert!(merged.contains_key("gonly"));
    }

    #[test]
    fn load_mcp_config_trust_gate_ignores_untrusted_workspace() {
        let dir = std::env::temp_dir().join(format!(
            "onyx-mcp-trust-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ws = dir.join("ws");
        std::fs::create_dir_all(ws.join(".onyx")).expect("mkdir");
        std::fs::write(
            ws.join(".onyx").join("config.toml"),
            r#"
[mcp.servers.ws-only]
transport = "stdio"
command = "x"
"#,
        )
        .expect("write");

        // Sin trust: se ignora y hay advertencia.
        let (configs, warnings) = load_mcp_config(None, Some(&ws), false).expect("load");
        assert!(configs.is_empty(), "{configs:?}");
        assert!(
            warnings.iter().any(|w| w.contains("not trusted")),
            "{warnings:?}"
        );

        // Con trust: aparece.
        let (configs, warnings) = load_mcp_config(None, Some(&ws), true).expect("load");
        assert!(configs.contains_key("ws-only"));
        assert!(warnings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_prefixed_name_round_trips() {
        assert_eq!(
            split_prefixed_name("mcp__github__search"),
            Some(("github", "search"))
        );
        assert_eq!(split_prefixed_name("mcp__a-b__c_d"), Some(("a-b", "c_d")));
        assert_eq!(split_prefixed_name("fs_read"), None);
        assert_eq!(split_prefixed_name("mcp__"), None);
        assert_eq!(split_prefixed_name("mcp__github__"), None);
        assert_eq!(split_prefixed_name("mcp____tool"), None);
    }

    #[test]
    fn debug_redacts_env_and_url_query() {
        let mut cfg = stdio_cfg("cmd");
        cfg.transport = McpTransport::Stdio {
            command: "cmd".to_string(),
            args: Vec::new(),
            env: vec![("TOKEN".to_string(), "${TOKEN}".to_string())],
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("${TOKEN}"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");

        let url_cfg = McpServerConfig {
            transport: McpTransport::Http {
                url: "https://x.example/mcp?secret=abc".to_string(),
            },
            timeout_secs: 20,
            enabled: true,
        };
        let dbg = format!("{url_cfg:?}");
        assert!(!dbg.contains("secret=abc"), "{dbg}");
    }

    #[test]
    fn expand_env_ref_reads_process_env() {
        // En los tests, el entorno no tiene `ONYX_MCP_TEST_REF_XYZ`; no se
        // pruebe aquí con `set_var` (unsafe en edition 2024) — lo cubre el
        // E2E `tests/mcp_stdio.rs` con un ref que sí existe.
        assert_eq!(expand_env_ref("${PATH}"), std::env::var("PATH").ok());
        assert_eq!(expand_env_ref("literal"), None);
        assert_eq!(expand_env_ref("${}"), None);
    }

    #[test]
    fn registry_call_rejects_malformed_prefixed_name() {
        let reg = McpRegistry::discover_from_config(&BTreeMap::new(), 1024);
        let err = reg.call("mcp__server", &json!({})).unwrap_err();
        assert!(err.to_string().contains("malformed"), "{err}");
        let err = reg.call("mcp__nope__tool", &json!({})).unwrap_err();
        assert!(err.to_string().contains("unknown MCP server"), "{err}");
    }

    #[test]
    fn registry_reports_disabled_and_http_pending_without_spawning() {
        let mut configs = BTreeMap::new();
        let mut off = stdio_cfg("x");
        off.enabled = false;
        configs.insert("off".to_string(), off);
        configs.insert(
            "remote".to_string(),
            McpServerConfig {
                transport: McpTransport::Http {
                    url: "https://mcp.example.com".to_string(),
                },
                timeout_secs: 20,
                enabled: true,
            },
        );
        let reg = McpRegistry::discover_from_config(&configs, 1024);
        let statuses = reg.server_statuses();
        assert_eq!(
            statuses,
            vec![
                ("off".to_string(), McpServerStatus::Disabled),
                ("remote".to_string(), McpServerStatus::HttpPending),
            ]
        );
        assert!(reg.tool_schemas().is_empty());
    }

    #[test]
    fn registry_isolates_broken_server_from_healthy_ones_in_statuses() {
        let mut configs = BTreeMap::new();
        configs.insert(
            "broken".to_string(),
            stdio_cfg("definitely-not-a-real-binary-onyx-mcp-xyz"),
        );
        let reg = McpRegistry::discover_from_config(&configs, 1024);
        let statuses = reg.server_statuses();
        assert_eq!(statuses.len(), 1);
        match &statuses[0].1 {
            McpServerStatus::Failed(msg) => {
                assert!(!msg.is_empty(), "failure reason must be reported");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
