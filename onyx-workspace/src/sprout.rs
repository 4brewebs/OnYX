//! Orquestador local de procesos y contenedores (`onyx-sprout`, SDD-08).
//!
//! Implementación pura sobre `std::thread`, `std::process::Command` y `std::net::TcpStream`
//! (0 `tokio`, MSRV 1.85), reutilizando las primitivas de SDD-06 (`ProcessJob`,
//! `apply_sanitized_env`, `clean_process_path`, `configure_process_isolation`,
//! `detect_container_cli`) para garantizar limpieza sin huérfanos en Windows/Unix y
//! aislamiento de secretos (`GEMINI_API_KEY`, `OPENAI_API_KEY`, etc.).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use regex::RegexBuilder;

use crate::policy::{
    WorkspacePolicy, array_brackets_closed_pub, parse_toml_string_array_pub, strip_toml_comment_pub,
};
use crate::sandbox::{
    ProcessJob, SandboxMode, apply_sanitized_env, check_control_plane_shell_write,
    check_offline_network_command, clean_process_path, configure_process_isolation,
    detect_container_cli, ensure_sandbox_tmp_dir,
};
use crate::tools::scrub_secrets;

pub const DEFAULT_LOG_CAPACITY: usize = 2_000;
pub const DEFAULT_WAIT_READY_SECS: u64 = 30;
pub const MAX_WAIT_READY_SECS: u64 = 300;
pub const SPROUT_CONFIG_FILE: &str = "sprout.toml";

/// Tipo de servicio gestionado en el DAG de `sprout.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    Process,
    Container,
}

impl ServiceKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "process" | "proc" => Some(Self::Process),
            "container" | "docker" | "oci" => Some(Self::Container),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Container => "container",
        }
    }
}

/// Estado del ciclo de vida de un servicio supervisado (`REQ-08.2`, `TEST-08.2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStatus {
    Off,
    Pending,
    Starting,
    Ready,
    Failed(String),
}

impl ServiceStatus {
    pub fn as_badge(&self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Pending => "PENDING",
            Self::Starting => "STARTING",
            Self::Ready => "READY",
            Self::Failed(_) => "FAILED",
        }
    }

    pub fn as_toon_cell(&self) -> String {
        match self {
            Self::Failed(reason) if !reason.is_empty() => {
                let clean = reason.replace('|', "/").replace('\n', " ");
                let short = if clean.len() > 48 {
                    let mut cut = 48;
                    while cut > 0 && !clean.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    format!("{}...", &clean[..cut])
                } else {
                    clean
                };
                format!("FAILED({short})")
            }
            other => other.as_badge().to_string(),
        }
    }
}

/// Especificación declarativa de un servicio en `sprout.toml` (`REQ-08.1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SproutServiceSpec {
    pub id: String,
    pub name: String,
    pub kind: ServiceKind,
    pub cwd: PathBuf,
    pub raw_cwd: String,
    pub cmd: String,
    pub stop_cmd: Option<String>,
    pub logs_cmd: Option<String>,
    pub container: Option<String>,
    pub debug_port: Option<u16>,
    pub ready_port: Option<u16>,
    pub ready_log: Option<String>,
    pub ready_cmd: Option<String>,
    pub optional: bool,
    pub enabled: bool,
    pub depends_on: Vec<String>,
}

/// Configuración completa de `sprout.toml` con DAG validado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SproutConfig {
    pub container_cli: Option<String>,
    pub env_wrapper: Option<String>,
    pub services: BTreeMap<String, SproutServiceSpec>,
}

impl SproutConfig {
    /// Carga y valida `<path>` (`sprout.toml`) aplicando restricciones de rutas sobre `workspace_root`.
    pub fn load_from_file(
        path: &Path,
        workspace_root: &Path,
        policy: Option<&WorkspacePolicy>,
    ) -> Result<Self, String> {
        let raw = fs::read_to_string(path)
            .map_err(|e| format!("failed to read `{}`: {e}", path.display()))?;
        Self::parse_toml(&raw, workspace_root, policy)
    }

    /// Parsea y valida `sprout.toml` soportando tanto `[runtime]` + `[services.<id>]`
    /// como el formato clásico de Sprout `[global]` + `[[service]]` (`REQ-08.1`, `TEST-08.1`).
    pub fn parse_toml(
        input: &str,
        workspace_root: &Path,
        policy: Option<&WorkspacePolicy>,
    ) -> Result<Self, String> {
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);
        let lines: Vec<&str> = text.lines().collect();

        enum Section {
            Root,
            Runtime,
            NamedService(String),
            ArrayService(usize),
            Other,
        }

        let mut container_cli: Option<String> = None;
        let mut env_wrapper: Option<String> = None;
        let mut named_svcs: BTreeMap<String, RawSvcRecord> = BTreeMap::new();
        let mut array_svcs: Vec<RawSvcRecord> = Vec::new();
        let mut section = Section::Root;
        let mut i = 0usize;

        while i < lines.len() {
            let line_no = i + 1;
            let stripped = strip_toml_comment_pub(lines[i]).trim();
            if stripped.is_empty() {
                i += 1;
                continue;
            }

            if stripped.starts_with("[[") && stripped.ends_with("]]") {
                let header = stripped[2..stripped.len() - 2].trim();
                if header == "service" || header == "services" {
                    array_svcs.push(RawSvcRecord::default());
                    section = Section::ArrayService(array_svcs.len() - 1);
                } else {
                    section = Section::Other;
                }
                i += 1;
                continue;
            }

            if stripped.starts_with('[') && stripped.ends_with(']') {
                let header = stripped[1..stripped.len() - 1].trim();
                if header == "runtime" || header == "global" {
                    section = Section::Runtime;
                } else if let Some(rest) = header.strip_prefix("services.") {
                    let id = rest.trim().trim_matches('"').trim_matches('\'').to_string();
                    if id.is_empty() {
                        return Err(format!(
                            "sprout.toml line {line_no}: empty service id in `[{header}]`"
                        ));
                    }
                    named_svcs.entry(id.clone()).or_default();
                    section = Section::NamedService(id);
                } else {
                    section = Section::Other;
                }
                i += 1;
                continue;
            }

            let Some((raw_key, raw_val)) = stripped.split_once('=') else {
                return Err(format!(
                    "sprout.toml line {line_no}: expected `key = value`, got `{stripped}`"
                ));
            };
            let key = raw_key.trim();
            let mut val_buf = raw_val.trim().to_string();
            if val_buf.starts_with('[') {
                while !array_brackets_closed_pub(&val_buf) {
                    i += 1;
                    if i >= lines.len() {
                        return Err(format!(
                            "sprout.toml line {line_no}: unterminated array for `{key}`"
                        ));
                    }
                    let next_piece = strip_toml_comment_pub(lines[i]).trim();
                    if !next_piece.is_empty() {
                        val_buf.push(' ');
                        val_buf.push_str(next_piece);
                    }
                }
            }

            match &section {
                Section::Runtime => match key {
                    "container_cli" => {
                        let s = parse_toml_string(&val_buf)
                            .map_err(|e| format!("sprout.toml line {line_no} (`{key}`): {e}"))?;
                        if !s.trim().is_empty() {
                            container_cli = Some(s);
                        }
                    }
                    "env_wrapper" => {
                        let s = parse_toml_string(&val_buf)
                            .map_err(|e| format!("sprout.toml line {line_no} (`{key}`): {e}"))?;
                        if !s.trim().is_empty() {
                            env_wrapper = Some(s);
                        }
                    }
                    _ => {}
                },
                Section::NamedService(id) => {
                    let svc = named_svcs.get_mut(id).expect("service entry must exist");
                    apply_service_field(svc, key, &val_buf, line_no)?;
                }
                Section::ArrayService(idx) => {
                    let svc = &mut array_svcs[*idx];
                    apply_service_field(svc, key, &val_buf, line_no)?;
                }
                Section::Root | Section::Other => {}
            }

            i += 1;
        }

        // Unificar `[[service]]` con `[services.<id>]`.
        for (idx, raw) in array_svcs.into_iter().enumerate() {
            let derived_id = raw
                .id
                .clone()
                .or_else(|| {
                    raw.name.as_ref().map(|n| {
                        n.trim()
                            .to_ascii_lowercase()
                            .chars()
                            .map(|c| {
                                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                                    c
                                } else {
                                    '-'
                                }
                            })
                            .collect::<String>()
                    })
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("service-{}", idx + 1));
            if named_svcs.contains_key(&derived_id) {
                return Err(format!(
                    "duplicate service id `{derived_id}` in sprout.toml"
                ));
            }
            named_svcs.insert(derived_id, raw);
        }

        let mut services = BTreeMap::new();
        for (id, raw) in named_svcs {
            let inferred_container = raw.cmd.is_none()
                && (raw.up.is_some()
                    || raw.down.is_some()
                    || raw.logs_cmd.is_some()
                    || raw.container.is_some());
            let kind = raw.kind.unwrap_or(if inferred_container {
                ServiceKind::Container
            } else {
                ServiceKind::Process
            });
            let cmd = raw.cmd.or(raw.up).unwrap_or_default();
            if cmd.trim().is_empty() {
                return Err(format!(
                    "service `{id}` must specify `cmd` (or `up` for container services)"
                ));
            }
            let stop_cmd = raw.stop_cmd.or(raw.down);
            let raw_cwd = raw.cwd.unwrap_or_else(|| ".".to_string());
            let cwd = resolve_service_cwd(&id, &raw_cwd, workspace_root, policy)?;
            let name = raw.name.unwrap_or_else(|| id.clone());
            services.insert(
                id.clone(),
                SproutServiceSpec {
                    id,
                    name,
                    kind,
                    cwd,
                    raw_cwd,
                    cmd,
                    stop_cmd,
                    logs_cmd: raw.logs_cmd,
                    container: raw.container,
                    debug_port: raw.debug_port,
                    ready_port: raw.ready_port,
                    ready_log: raw.ready_log,
                    ready_cmd: raw.ready_cmd,
                    optional: raw.optional,
                    enabled: raw.enabled.unwrap_or(true),
                    depends_on: raw.depends_on,
                },
            );
        }

        let cfg = Self {
            container_cli,
            env_wrapper,
            services,
        };
        cfg.validate_dag()?;
        Ok(cfg)
    }

    /// Valida que todas las dependencias existan y que el grafo `depends_on` sea acíclico (DAG).
    pub fn validate_dag(&self) -> Result<(), String> {
        let _ = self.topo_order()?;
        Ok(())
    }

    /// Devuelve todos los IDs de servicio en orden topológico determinista (dependencias primero).
    pub fn topo_order(&self) -> Result<Vec<String>, String> {
        let mut in_degree: BTreeMap<String, usize> = BTreeMap::new();
        let mut dependents: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for id in self.services.keys() {
            in_degree.insert(id.clone(), 0);
            dependents.insert(id.clone(), Vec::new());
        }

        for (id, svc) in &self.services {
            let mut seen_deps = BTreeSet::new();
            for dep in &svc.depends_on {
                if !self.services.contains_key(dep) {
                    return Err(format!("service `{id}` depends on unknown service `{dep}`"));
                }
                if dep == id {
                    return Err(format!("service `{id}` cannot depend on itself"));
                }
                if seen_deps.insert(dep.clone()) {
                    *in_degree.get_mut(id).expect("exists") += 1;
                    dependents.get_mut(dep).expect("exists").push(id.clone());
                }
            }
        }

        let mut ready_queue: BTreeSet<String> = in_degree
            .iter()
            .filter(|&(_, deg)| *deg == 0)
            .map(|(id, _)| id.clone())
            .collect();
        let mut order = Vec::with_capacity(self.services.len());

        while let Some(next) = ready_queue.pop_first() {
            order.push(next.clone());
            if let Some(children) = dependents.get(&next) {
                for child in children {
                    let deg = in_degree.get_mut(child).expect("exists");
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        ready_queue.insert(child.clone());
                    }
                }
            }
        }

        if order.len() != self.services.len() {
            let stuck: Vec<String> = in_degree
                .into_iter()
                .filter_map(|(id, deg)| (deg > 0).then_some(id))
                .collect();
            return Err(format!(
                "dependency cycle detected in sprout.toml involving: {}",
                stuck.join(", ")
            ));
        }

        Ok(order)
    }

    /// Calcula los ancestros transitivos de `target_id` más el propio `target_id` en orden topológico.
    pub fn transitive_start_order(&self, target_id: &str) -> Result<Vec<String>, String> {
        if !self.services.contains_key(target_id) {
            return Err(format!("unknown service `{target_id}` in sprout.toml"));
        }
        let mut needed = BTreeSet::new();
        let mut stack = vec![target_id.to_string()];
        while let Some(curr) = stack.pop() {
            if needed.insert(curr.clone()) {
                if let Some(svc) = self.services.get(&curr) {
                    for dep in &svc.depends_on {
                        stack.push(dep.clone());
                    }
                }
            }
        }
        let full_order = self.topo_order()?;
        Ok(full_order
            .into_iter()
            .filter(|id| needed.contains(id))
            .collect())
    }

    pub fn has_container_services(&self) -> bool {
        self.services
            .values()
            .any(|s| s.kind == ServiceKind::Container)
    }

    pub fn effective_container_cli(&self) -> Option<String> {
        self.container_cli
            .clone()
            .or_else(|| detect_container_cli().map(str::to_string))
    }
}

fn resolve_service_cwd(
    id: &str,
    raw_cwd: &str,
    workspace_root: &Path,
    policy: Option<&WorkspacePolicy>,
) -> Result<PathBuf, String> {
    let trimmed = raw_cwd.trim();
    if trimmed.is_empty() || trimmed == "." {
        return Ok(clean_process_path(workspace_root));
    }
    if let Some(pol) = policy {
        // Rechaza escapes fuera del workspace (incluso si el subdirectorio aún no existe).
        let checked = pol
            .resolve_existing(trimmed)
            .or_else(|_| pol.resolve_for_write(trimmed))
            .map_err(|e| format!("service `{id}` invalid cwd `{raw_cwd}`: {e}"))?;
        return Ok(clean_process_path(&checked));
    }
    let p = Path::new(trimmed);
    if p.is_absolute() || trimmed.replace('\\', "/").split('/').any(|seg| seg == "..") {
        return Err(format!(
            "service `{id}` cwd `{raw_cwd}` escapes workspace root"
        ));
    }
    Ok(clean_process_path(&workspace_root.join(p)))
}

fn apply_service_field(
    svc: &mut impl ServiceFieldTarget,
    key: &str,
    val_buf: &str,
    line_no: usize,
) -> Result<(), String> {
    match key {
        "id" => svc.set_id(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?),
        "name" => svc.set_name(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?),
        "kind" => {
            let s = parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?;
            let parsed = ServiceKind::parse(&s).ok_or_else(|| {
                format!("sprout.toml line {line_no}: invalid kind `{s}` (expected `process` or `container`)")
            })?;
            svc.set_kind(parsed);
        }
        "cwd" | "dir" => {
            svc.set_cwd(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?)
        }
        "cmd" => svc.set_cmd(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?),
        "up" => svc.set_up(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?),
        "stop_cmd" => {
            svc.set_stop_cmd(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?)
        }
        "down" => svc.set_down(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?),
        "logs" | "logs_cmd" => {
            svc.set_logs_cmd(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?)
        }
        "container" => {
            svc.set_container(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?)
        }
        "debug_port" => {
            let port = val_buf
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("sprout.toml line {line_no}: `debug_port` must be a u16"))?;
            svc.set_debug_port(port);
        }
        "ready_port" => {
            let port = val_buf
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("sprout.toml line {line_no}: `ready_port` must be a u16"))?;
            svc.set_ready_port(port);
        }
        "ready_log" => {
            svc.set_ready_log(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?)
        }
        "ready_cmd" => {
            svc.set_ready_cmd(parse_toml_string(val_buf).map_err(|e| fmt_err(line_no, key, e))?)
        }
        "optional" => {
            let b = parse_toml_bool(val_buf).map_err(|e| fmt_err(line_no, key, e))?;
            svc.set_optional(b);
        }
        "enabled" => {
            let b = parse_toml_bool(val_buf).map_err(|e| fmt_err(line_no, key, e))?;
            svc.set_enabled(b);
        }
        "depends_on" => {
            let deps =
                parse_toml_string_array_pub(val_buf).map_err(|e| fmt_err(line_no, key, e))?;
            svc.extend_depends_on(deps);
        }
        _ => {}
    }
    Ok(())
}

trait ServiceFieldTarget {
    fn set_id(&mut self, v: String);
    fn set_name(&mut self, v: String);
    fn set_kind(&mut self, v: ServiceKind);
    fn set_cwd(&mut self, v: String);
    fn set_cmd(&mut self, v: String);
    fn set_up(&mut self, v: String);
    fn set_stop_cmd(&mut self, v: String);
    fn set_down(&mut self, v: String);
    fn set_logs_cmd(&mut self, v: String);
    fn set_container(&mut self, v: String);
    fn set_debug_port(&mut self, v: u16);
    fn set_ready_port(&mut self, v: u16);
    fn set_ready_log(&mut self, v: String);
    fn set_ready_cmd(&mut self, v: String);
    fn set_optional(&mut self, v: bool);
    fn set_enabled(&mut self, v: bool);
    fn extend_depends_on(&mut self, v: Vec<String>);
}

fn fmt_err(line_no: usize, key: &str, err: String) -> String {
    format!("sprout.toml line {line_no} (`{key}`): {err}")
}

fn parse_toml_bool(raw: &str) -> Result<bool, String> {
    match raw.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected boolean `true`/`false`, found `{other}`")),
    }
}

fn parse_toml_string(raw: &str) -> Result<String, String> {
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
                    None => return Err("unterminated escape sequence".to_string()),
                }
            } else {
                out.push(c);
            }
        }
        return Ok(out);
    }
    Err(format!("expected quoted string, found `{trimmed}`"))
}

// Implement `ServiceFieldTarget` for the local `RawSvc` via a macro-free helper struct.
// (Rust requires the struct at module scope for trait impl.)
#[derive(Default)]
struct RawSvcRecord {
    id: Option<String>,
    name: Option<String>,
    kind: Option<ServiceKind>,
    cwd: Option<String>,
    cmd: Option<String>,
    up: Option<String>,
    stop_cmd: Option<String>,
    down: Option<String>,
    logs_cmd: Option<String>,
    container: Option<String>,
    debug_port: Option<u16>,
    ready_port: Option<u16>,
    ready_log: Option<String>,
    ready_cmd: Option<String>,
    optional: bool,
    enabled: Option<bool>,
    depends_on: Vec<String>,
}

impl ServiceFieldTarget for RawSvcRecord {
    fn set_id(&mut self, v: String) {
        self.id = Some(v);
    }
    fn set_name(&mut self, v: String) {
        self.name = Some(v);
    }
    fn set_kind(&mut self, v: ServiceKind) {
        self.kind = Some(v);
    }
    fn set_cwd(&mut self, v: String) {
        self.cwd = Some(v);
    }
    fn set_cmd(&mut self, v: String) {
        self.cmd = Some(v);
    }
    fn set_up(&mut self, v: String) {
        self.up = Some(v);
    }
    fn set_stop_cmd(&mut self, v: String) {
        self.stop_cmd = Some(v);
    }
    fn set_down(&mut self, v: String) {
        self.down = Some(v);
    }
    fn set_logs_cmd(&mut self, v: String) {
        self.logs_cmd = Some(v);
    }
    fn set_container(&mut self, v: String) {
        self.container = Some(v);
    }
    fn set_debug_port(&mut self, v: u16) {
        self.debug_port = Some(v);
    }
    fn set_ready_port(&mut self, v: u16) {
        self.ready_port = Some(v);
    }
    fn set_ready_log(&mut self, v: String) {
        self.ready_log = Some(v);
    }
    fn set_ready_cmd(&mut self, v: String) {
        self.ready_cmd = Some(v);
    }
    fn set_optional(&mut self, v: bool) {
        self.optional = v;
    }
    fn set_enabled(&mut self, v: bool) {
        self.enabled = Some(v);
    }
    fn extend_depends_on(&mut self, v: Vec<String>) {
        self.depends_on.extend(v);
    }
}

// ---------------------------------------------------------------------------
// Runtime Supervisor (std::thread + ProcessJob + ring-buffer)
// ---------------------------------------------------------------------------

struct ManagedService {
    spec: SproutServiceSpec,
    status: ServiceStatus,
    pid: Option<u32>,
    generation: u64,
    logs: VecDeque<String>,
    job: Option<ProcessJob>,
    logs_job: Option<ProcessJob>,
    log_matched: bool,
    port_open: bool,
    cmd_ok: bool,
}

impl ManagedService {
    fn new(spec: SproutServiceSpec) -> Self {
        Self {
            spec,
            status: ServiceStatus::Off,
            pid: None,
            generation: 0,
            logs: VecDeque::with_capacity(256),
            job: None,
            logs_job: None,
            log_matched: false,
            port_open: false,
            cmd_ok: false,
        }
    }

    fn push_log(&mut self, line: String) {
        if self.logs.len() >= DEFAULT_LOG_CAPACITY {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }

    fn has_any_probe(&self) -> bool {
        self.spec.ready_log.is_some()
            || self.spec.ready_port.is_some()
            || self.spec.ready_cmd.is_some()
    }

    fn check_all_probes_satisfied(&self) -> bool {
        let log_ok = self.spec.ready_log.is_none() || self.log_matched;
        let port_ok = self.spec.ready_port.is_none() || self.port_open;
        let cmd_ok = self.spec.ready_cmd.is_none() || self.cmd_ok;
        self.has_any_probe() && log_ok && port_ok && cmd_ok
    }

    fn reset_probes(&mut self) {
        self.log_matched = false;
        self.port_open = false;
        self.cmd_ok = false;
    }
}

struct SproutState {
    config: Option<SproutConfig>,
    services: BTreeMap<String, ManagedService>,
}

struct SproutInner {
    policy: WorkspacePolicy,
    state: Mutex<SproutState>,
}

impl Drop for SproutInner {
    fn drop(&mut self) {
        stop_all_locked(&self.policy, &self.state);
    }
}

/// Supervisor thread-safe de servicios locales y contenedores (`onyx-sprout`, `REQ-08.2`).
///
/// Al destruirse el último clon (`Drop` del `Arc` interior), ejecuta `stop_cmd`/`down` y
/// suelta todos los [`ProcessJob`] garantizando cero procesos huérfanos.
#[derive(Clone)]
pub struct SproutSupervisor {
    inner: Arc<SproutInner>,
}

impl std::fmt::Debug for SproutSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SproutSupervisor")
            .field("root", &self.inner.policy.root)
            .finish()
    }
}

impl SproutSupervisor {
    pub fn new(policy: WorkspacePolicy) -> Self {
        Self {
            inner: Arc::new(SproutInner {
                policy,
                state: Mutex::new(SproutState {
                    config: None,
                    services: BTreeMap::new(),
                }),
            }),
        }
    }

    /// Crea un supervisor precargado con un `SproutConfig` ya validado (útil en tests).
    pub fn from_config(policy: WorkspacePolicy, config: SproutConfig) -> Self {
        let mut services = BTreeMap::new();
        for (id, spec) in &config.services {
            services.insert(id.clone(), ManagedService::new(spec.clone()));
        }
        Self {
            inner: Arc::new(SproutInner {
                policy,
                state: Mutex::new(SproutState {
                    config: Some(config),
                    services,
                }),
            }),
        }
    }

    /// Garantiza que `sprout.toml` está cargado en memoria si existe en el workspace.
    pub fn ensure_loaded(&self) -> Result<(), String> {
        let mut guard = self
            .inner
            .state
            .lock()
            .map_err(|_| "sprout state lock poisoned".to_string())?;
        if guard.config.is_some() {
            return Ok(());
        }
        let path = self.inner.policy.root.join(SPROUT_CONFIG_FILE);
        if !path.is_file() {
            return Err(format!(
                "no `{SPROUT_CONFIG_FILE}` found in `{}`",
                clean_process_path(&self.inner.policy.root).display()
            ));
        }
        let cfg =
            SproutConfig::load_from_file(&path, &self.inner.policy.root, Some(&self.inner.policy))?;
        for (id, spec) in &cfg.services {
            guard
                .services
                .insert(id.clone(), ManagedService::new(spec.clone()));
        }
        guard.config = Some(cfg);
        Ok(())
    }

    /// Devuelve la tabla `@TOON v1` del estado actual de todos los servicios (`REQ-08.3`).
    pub fn status_toon(&self) -> Result<String, String> {
        self.ensure_loaded()?;
        let guard = self
            .inner
            .state
            .lock()
            .map_err(|_| "sprout state lock poisoned".to_string())?;
        Ok(format_status_toon_locked(&guard))
    }

    /// Devuelve texto compacto para el panel lateral `Services (Sprout)` de la TUI si hay `sprout.toml`.
    pub fn render_panel_text(&self) -> Option<String> {
        let _ = self.ensure_loaded();
        let guard = self.inner.state.lock().ok()?;
        if guard.services.is_empty() {
            return None;
        }
        let toon = format_status_toon_locked(&guard);
        let panel = format_services_panel_from_toon(&toon);
        (!panel.is_empty()).then_some(panel)
    }

    /// Arranca `id` (o todos los servicios habilitados si `id == "all"`) y sus dependencias
    /// transitivas en orden topológico esperando las sondas (`REQ-08.2`, `REQ-08.3`).
    pub fn start(&self, id: &str, wait_ready_secs: Option<u64>) -> Result<String, String> {
        if id.trim().eq_ignore_ascii_case("all") {
            return self.start_all(wait_ready_secs);
        }
        self.ensure_loaded()?;
        let wait_secs = wait_ready_secs
            .unwrap_or(DEFAULT_WAIT_READY_SECS)
            .clamp(1, MAX_WAIT_READY_SECS);
        let plan = {
            let guard = self
                .inner
                .state
                .lock()
                .map_err(|_| "sprout state lock poisoned".to_string())?;
            let cfg = guard
                .config
                .as_ref()
                .ok_or_else(|| "sprout config not loaded".to_string())?;
            cfg.transitive_start_order(id)?
        };
        let actions = self.start_plan(&plan, Some(id), wait_secs)?;
        let toon = self.status_toon()?;
        Ok(format!("started `{id}` ({})\n{toon}", actions.join(" -> ")))
    }

    /// Arranca todos los servicios habilitados de `sprout.toml` en orden topológico DAG.
    pub fn start_all(&self, wait_ready_secs: Option<u64>) -> Result<String, String> {
        self.ensure_loaded()?;
        let wait_secs = wait_ready_secs
            .unwrap_or(DEFAULT_WAIT_READY_SECS)
            .clamp(1, MAX_WAIT_READY_SECS);
        let plan = {
            let guard = self
                .inner
                .state
                .lock()
                .map_err(|_| "sprout state lock poisoned".to_string())?;
            let cfg = guard
                .config
                .as_ref()
                .ok_or_else(|| "sprout config not loaded".to_string())?;
            cfg.topo_order()?
                .into_iter()
                .filter(|svc_id| cfg.services.get(svc_id).is_some_and(|s| s.enabled))
                .collect::<Vec<_>>()
        };
        if plan.is_empty() {
            let toon = self.status_toon()?;
            return Ok(format!("no enabled services to start\n{toon}"));
        }
        let actions = self.start_plan(&plan, None, wait_secs)?;
        let toon = self.status_toon()?;
        Ok(format!("started `all` ({})\n{toon}", actions.join(" -> ")))
    }

    fn start_plan(
        &self,
        plan: &[String],
        focus_id: Option<&str>,
        wait_secs: u64,
    ) -> Result<Vec<String>, String> {
        let mut actions = Vec::new();
        for svc_id in plan {
            let already_ready = {
                let guard = self
                    .inner
                    .state
                    .lock()
                    .map_err(|_| "sprout state lock poisoned".to_string())?;
                guard
                    .services
                    .get(svc_id)
                    .is_some_and(|s| s.status == ServiceStatus::Ready)
            };
            if already_ready {
                if focus_id.is_none() || focus_id == Some(svc_id.as_str()) {
                    actions.push(format!("{svc_id}: already READY"));
                }
                continue;
            }

            // Si es un servicio `container`, validamos daemon antes de lanzar `up`.
            let (kind, container_cli) = {
                let guard = self
                    .inner
                    .state
                    .lock()
                    .map_err(|_| "sprout state lock poisoned".to_string())?;
                let svc = guard
                    .services
                    .get(svc_id)
                    .ok_or_else(|| format!("unknown service `{svc_id}`"))?;
                let cli = guard
                    .config
                    .as_ref()
                    .and_then(|c| c.effective_container_cli());
                (svc.spec.kind, cli)
            };

            if kind == ServiceKind::Container {
                let cli = container_cli.ok_or_else(|| {
                    format!(
                        "service `{svc_id}` requires a container runtime (`docker`, `nerdctl`, or `podman`), but none was found on PATH"
                    )
                })?;
                verify_container_daemon(&cli, &self.inner.policy)?;
            }

            self.spawn_single(svc_id)?;
            let outcome = self.wait_until_ready(svc_id, Duration::from_secs(wait_secs))?;
            match outcome {
                ServiceStatus::Ready => {
                    actions.push(format!("{svc_id}: READY"));
                }
                ServiceStatus::Failed(reason) => {
                    let tail = self.tail_logs_raw(svc_id, 15);
                    let toon = self.status_toon().unwrap_or_default();
                    return Err(format!(
                        "service `{svc_id}` failed to start ({reason}).\nRecent logs:\n{}\n\n{toon}",
                        scrub_secrets(&tail)
                    ));
                }
                other => {
                    let tail = self.tail_logs_raw(svc_id, 15);
                    let toon = self.status_toon().unwrap_or_default();
                    return Err(format!(
                        "service `{svc_id}` did not reach READY within {wait_secs}s (current: {}).\nRecent logs:\n{}\n\n{toon}",
                        other.as_badge(),
                        scrub_secrets(&tail)
                    ));
                }
            }
        }
        Ok(actions)
    }

    /// Detiene un servicio concreto (`id`) o todos los activos (`all = true`) en orden topológico inverso.
    pub fn stop(&self, id: Option<&str>, all: bool) -> Result<String, String> {
        self.ensure_loaded()?;
        let targets = if all || id.is_none() {
            let guard = self
                .inner
                .state
                .lock()
                .map_err(|_| "sprout state lock poisoned".to_string())?;
            let mut order = guard
                .config
                .as_ref()
                .map(|c| c.topo_order().unwrap_or_default())
                .unwrap_or_default();
            order.reverse();
            order
        } else {
            let target = id.unwrap_or_default().trim().to_string();
            let guard = self
                .inner
                .state
                .lock()
                .map_err(|_| "sprout state lock poisoned".to_string())?;
            if !guard.services.contains_key(&target) {
                return Err(format!("unknown service `{target}` in sprout.toml"));
            }
            vec![target]
        };

        let mut stopped = Vec::new();
        for svc_id in targets {
            if stop_single_service(&self.inner.policy, &self.inner.state, &svc_id) {
                stopped.push(svc_id);
            }
        }
        let toon = self.status_toon()?;
        if stopped.is_empty() {
            Ok(format!("no running services to stop\n{toon}"))
        } else {
            Ok(format!("stopped: {}\n{toon}", stopped.join(", ")))
        }
    }

    /// Consulta el ring-buffer de logs de un servicio con filtro opcional (texto o regex) y `scrub_secrets`.
    pub fn logs(
        &self,
        id: &str,
        tail: Option<usize>,
        pattern: Option<&str>,
        is_regex: bool,
    ) -> Result<String, String> {
        self.ensure_loaded()?;
        let limit = tail.unwrap_or(80).clamp(1, 500);
        let matcher = match pattern.map(str::trim).filter(|s| !s.is_empty()) {
            Some(pat) => {
                let src = if is_regex {
                    pat.to_string()
                } else {
                    regex::escape(pat)
                };
                Some(
                    RegexBuilder::new(&src)
                        .case_insensitive(false)
                        .build()
                        .map_err(|e| format!("invalid regex pattern `{pat}`: {e}"))?,
                )
            }
            None => None,
        };

        let guard = self
            .inner
            .state
            .lock()
            .map_err(|_| "sprout state lock poisoned".to_string())?;
        let svc = guard
            .services
            .get(id)
            .ok_or_else(|| format!("unknown service `{id}` in sprout.toml"))?;

        let filtered: Vec<&String> = svc
            .logs
            .iter()
            .filter(|line| matcher.as_ref().is_none_or(|re| re.is_match(line)))
            .collect();
        if filtered.is_empty() {
            return Ok(format!(
                "(no logs for `{id}`, status={})",
                svc.status.as_badge()
            ));
        }
        let start = filtered.len().saturating_sub(limit);
        let joined = filtered[start..]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let cleaned = scrub_secrets(&joined);
        Ok(truncate_utf8(&cleaned, self.inner.policy.max_output_bytes))
    }

    /// Ejecuta un comando aislado dentro de un servicio `kind = "container"` (`docker exec` / `nerdctl exec`)
    /// aplicando `env_clear()`, `ProcessJob`, timeout acotado y `scrub_secrets` (`REQ-08.4`, `TEST-08.3`).
    pub fn exec_in_container(
        &self,
        id: &str,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<String, String> {
        if command.trim().is_empty() {
            return Err("command cannot be empty".to_string());
        }
        self.ensure_loaded()?;

        let (container_name, container_cli) = {
            let guard = self
                .inner
                .state
                .lock()
                .map_err(|_| "sprout state lock poisoned".to_string())?;
            let svc = guard
                .services
                .get(id)
                .ok_or_else(|| format!("unknown service `{id}` in sprout.toml"))?;
            if svc.spec.kind != ServiceKind::Container {
                return Err(format!(
                    "service `{id}` has kind `{}`: `service_exec` only supports `kind = \"container\"` services",
                    svc.spec.kind.as_str()
                ));
            }
            let cname = svc.spec.container.clone().ok_or_else(|| {
                format!("service `{id}` does not declare `container = \"<name>\"` in sprout.toml")
            })?;
            let cli = guard
                .config
                .as_ref()
                .and_then(|c| c.effective_container_cli())
                .ok_or_else(|| {
                    "no container CLI (`docker`, `nerdctl`, or `podman`) available on PATH"
                        .to_string()
                })?;
            (cname, cli)
        };

        if self.inner.policy.sandbox.mode != SandboxMode::Off {
            check_control_plane_shell_write(command)?;
            check_offline_network_command(command, self.inner.policy.sandbox.network)?;
        }

        let sandbox_tmp = ensure_sandbox_tmp_dir(&self.inner.policy.root)?;
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(30).clamp(1, 300));

        let mut cmd = Command::new(&container_cli);
        cmd.args(["exec", &container_name, "sh", "-c", command])
            .current_dir(clean_process_path(&self.inner.policy.root))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_sanitized_env(
            &mut cmd,
            &self.inner.policy.sandbox,
            &self.inner.policy.root,
            &sandbox_tmp,
        );
        configure_process_isolation(&mut cmd, true);

        let (code, stdout, stderr) = run_command_bounded(
            cmd,
            timeout,
            self.inner.policy.max_output_bytes,
            self.inner.policy.sandbox.limits(),
            &format!("service_exec({id})"),
        )?;

        let combined = format!(
            "service: {id} (container={container_name})\nexit_code: {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        Ok(truncate_utf8(
            &scrub_secrets(&combined),
            self.inner.policy.max_output_bytes,
        ))
    }

    fn spawn_single(&self, id: &str) -> Result<(), String> {
        let (spec, env_wrapper, generation) = {
            let mut guard = self
                .inner
                .state
                .lock()
                .map_err(|_| "sprout state lock poisoned".to_string())?;
            let wrapper = guard.config.as_ref().and_then(|c| c.env_wrapper.clone());
            let svc = guard
                .services
                .get_mut(id)
                .ok_or_else(|| format!("unknown service `{id}`"))?;
            // Limpiar cualquier instancia previa.
            if let Some(mut j) = svc.job.take() {
                j.terminate();
            }
            if let Some(mut lj) = svc.logs_job.take() {
                lj.terminate();
            }
            svc.pid = None;
            svc.reset_probes();
            svc.status = ServiceStatus::Pending;
            svc.generation = svc.generation.saturating_add(1);
            svc.status = ServiceStatus::Starting;
            svc.push_log(format!("[ORCHESTRATOR] Starting {}...", svc.spec.name));
            (svc.spec.clone(), wrapper, svc.generation)
        };

        let mut final_cmd = spec.cmd.clone();
        if spec.kind == ServiceKind::Process {
            if let Some(port) = spec.debug_port {
                final_cmd = format!(
                    "{final_cmd} -Dspring-boot.run.jvmArguments=\"-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:{port}\""
                );
            }
            if let Some(wrapper) = env_wrapper {
                final_cmd = format!("{wrapper} {final_cmd}");
            }
        }

        let sandbox_tmp = ensure_sandbox_tmp_dir(&self.inner.policy.root)?;
        let mut cmd_proc = build_shell_command(&final_cmd, &spec.cwd);
        apply_sanitized_env(
            &mut cmd_proc,
            &self.inner.policy.sandbox,
            &self.inner.policy.root,
            &sandbox_tmp,
        );
        configure_process_isolation(&mut cmd_proc, true);

        let mut child = match cmd_proc.spawn() {
            Ok(c) => c,
            Err(e) => {
                if let Ok(mut guard) = self.inner.state.lock() {
                    if let Some(svc) = guard.services.get_mut(id) {
                        if svc.generation == generation {
                            svc.status = ServiceStatus::Failed(e.to_string());
                            svc.push_log(format!("[ERROR] Failed to spawn: {e}"));
                        }
                    }
                }
                return Err(format!("failed to spawn service `{id}`: {e}"));
            }
        };

        let job = ProcessJob::assign(&child, self.inner.policy.sandbox.limits());
        let pid = child.id();

        {
            let mut guard = self
                .inner
                .state
                .lock()
                .map_err(|_| "sprout state lock poisoned".to_string())?;
            if let Some(svc) = guard.services.get_mut(id) {
                if svc.generation != generation || svc.status != ServiceStatus::Starting {
                    drop(job);
                    let _ = child.kill();
                    return Ok(());
                }
                svc.pid = Some(pid);
                svc.job = job;
            }
        }

        // 1. Lector stdout (vigila `ready_log`, UTF-8 lossy para CP850/Windows-1252).
        if let Some(stdout) = child.stdout.take() {
            let inner = Arc::clone(&self.inner);
            let svc_id = id.to_string();
            let marker = spec.ready_log.clone();
            thread::spawn(move || {
                stream_pipe_to_logs(stdout, inner, &svc_id, generation, marker.as_deref(), false);
            });
        }

        // 2. Lector stderr (vigila también `ready_log` ya que algunos frameworks loguean en stderr).
        if let Some(stderr) = child.stderr.take() {
            let inner = Arc::clone(&self.inner);
            let svc_id = id.to_string();
            let marker = spec.ready_log.clone();
            thread::spawn(move || {
                stream_pipe_to_logs(stderr, inner, &svc_id, generation, marker.as_deref(), true);
            });
        }

        // 3. Si `logs_cmd` está definido (p. ej. `docker compose logs -f ...`), lanzar lector bajo ProcessJob.
        if let Some(logs_cmd) = spec.logs_cmd.clone() {
            let mut lcmd = build_shell_command(&logs_cmd, &spec.cwd);
            apply_sanitized_env(
                &mut lcmd,
                &self.inner.policy.sandbox,
                &self.inner.policy.root,
                &sandbox_tmp,
            );
            configure_process_isolation(&mut lcmd, true);
            if let Ok(mut lchild) = lcmd.spawn() {
                let ljob = ProcessJob::assign(&lchild, self.inner.policy.sandbox.limits());
                if let Ok(mut guard) = self.inner.state.lock() {
                    if let Some(svc) = guard.services.get_mut(id) {
                        if svc.generation == generation {
                            svc.logs_job = ljob;
                        }
                    }
                }
                if let Some(out) = lchild.stdout.take() {
                    let inner = Arc::clone(&self.inner);
                    let svc_id = id.to_string();
                    let marker = spec.ready_log.clone();
                    thread::spawn(move || {
                        stream_pipe_to_logs(
                            out,
                            inner,
                            &svc_id,
                            generation,
                            marker.as_deref(),
                            false,
                        );
                    });
                }
                if let Some(err) = lchild.stderr.take() {
                    let inner = Arc::clone(&self.inner);
                    let svc_id = id.to_string();
                    let marker = spec.ready_log.clone();
                    thread::spawn(move || {
                        stream_pipe_to_logs(
                            err,
                            inner,
                            &svc_id,
                            generation,
                            marker.as_deref(),
                            true,
                        );
                    });
                }
            }
        }

        // 4. Hilo de sondas (`ready_port` y `ready_cmd`).
        if spec.ready_port.is_some() || spec.ready_cmd.is_some() {
            let inner = Arc::clone(&self.inner);
            let svc_id = id.to_string();
            let port_opt = spec.ready_port;
            let cmd_opt = spec.ready_cmd.clone();
            let cwd = spec.cwd.clone();
            thread::spawn(move || {
                let mut last_cmd_probe = Instant::now() - Duration::from_secs(5);
                loop {
                    let active = inner
                        .state
                        .lock()
                        .ok()
                        .and_then(|g| {
                            g.services.get(&svc_id).map(|s| {
                                s.generation == generation && s.status == ServiceStatus::Starting
                            })
                        })
                        .unwrap_or(false);
                    if !active {
                        break;
                    }

                    if let Some(port) = port_opt {
                        if probe_local_tcp_port(port, Duration::from_millis(250)) {
                            if let Ok(mut guard) = inner.state.lock() {
                                if let Some(svc) = guard.services.get_mut(&svc_id) {
                                    if svc.generation == generation
                                        && svc.status == ServiceStatus::Starting
                                    {
                                        if !svc.port_open {
                                            svc.port_open = true;
                                            svc.push_log(format!(
                                                "[ORCHESTRATOR] Port {port} is open."
                                            ));
                                        }
                                        if svc.check_all_probes_satisfied() {
                                            svc.status = ServiceStatus::Ready;
                                            svc.push_log(
                                                "[ORCHESTRATOR] Service Ready.".to_string(),
                                            );
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if let Some(ref check_cmd) = cmd_opt {
                        if last_cmd_probe.elapsed() >= Duration::from_secs(1) {
                            last_cmd_probe = Instant::now();
                            if run_silent_probe_cmd(check_cmd, &cwd, &inner.policy) {
                                if let Ok(mut guard) = inner.state.lock() {
                                    if let Some(svc) = guard.services.get_mut(&svc_id) {
                                        if svc.generation == generation
                                            && svc.status == ServiceStatus::Starting
                                        {
                                            if !svc.cmd_ok {
                                                svc.cmd_ok = true;
                                                svc.push_log(format!(
                                                    "[ORCHESTRATOR] Check '{check_cmd}' succeeded."
                                                ));
                                            }
                                            if svc.check_all_probes_satisfied() {
                                                svc.status = ServiceStatus::Ready;
                                                svc.push_log(
                                                    "[ORCHESTRATOR] Service Ready.".to_string(),
                                                );
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    thread::sleep(Duration::from_millis(150));
                }
            });
        }

        // 5. Monitor de salida del proceso hijo.
        {
            let inner = Arc::clone(&self.inner);
            let svc_id = id.to_string();
            thread::spawn(move || {
                if let Ok(status) = child.wait() {
                    if let Ok(mut guard) = inner.state.lock() {
                        if let Some(svc) = guard.services.get_mut(&svc_id) {
                            if svc.generation != generation {
                                return;
                            }
                            svc.pid = None;
                            if status.success() {
                                if !svc.has_any_probe() && svc.status == ServiceStatus::Starting {
                                    svc.status = ServiceStatus::Ready;
                                    svc.push_log(
                                        "[ORCHESTRATOR] Command completed; Service Ready."
                                            .to_string(),
                                    );
                                }
                            } else if svc.status != ServiceStatus::Off {
                                let code = status.code().unwrap_or(-1);
                                svc.status = ServiceStatus::Failed(format!("exit code {code}"));
                                svc.push_log(format!(
                                    "[ERROR] Process exited unexpectedly with code {code}"
                                ));
                            }
                        }
                    }
                }
            });
        }

        Ok(())
    }

    fn wait_until_ready(&self, id: &str, timeout: Duration) -> Result<ServiceStatus, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let status = {
                let guard = self
                    .inner
                    .state
                    .lock()
                    .map_err(|_| "sprout state lock poisoned".to_string())?;
                guard
                    .services
                    .get(id)
                    .map(|s| s.status.clone())
                    .ok_or_else(|| format!("unknown service `{id}`"))?
            };
            match status {
                ServiceStatus::Ready | ServiceStatus::Failed(_) | ServiceStatus::Off => {
                    return Ok(status);
                }
                ServiceStatus::Pending | ServiceStatus::Starting => {
                    if Instant::now() >= deadline {
                        return Ok(status);
                    }
                    thread::sleep(Duration::from_millis(80));
                }
            }
        }
    }

    fn tail_logs_raw(&self, id: &str, n: usize) -> String {
        let Ok(guard) = self.inner.state.lock() else {
            return String::new();
        };
        let Some(svc) = guard.services.get(id) else {
            return String::new();
        };
        let start = svc.logs.len().saturating_sub(n);
        svc.logs
            .iter()
            .skip(start)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Resultado de auditar `sprout.toml` desde `openonyx doctor` (`REQ-08.6`, `TEST-08.5`).
#[derive(Debug, Clone, Default)]
pub struct SproutAudit {
    pub checks: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// Audita `<workspace>/sprout.toml` si existe: sintaxis, DAG acíclico, CLI de contenedores
/// y puertos locales ocupados (`REQ-08.6`).
pub fn audit_sprout_workspace(workspace: &Path) -> Option<SproutAudit> {
    let path = workspace.join(SPROUT_CONFIG_FILE);
    if !path.is_file() {
        return None;
    }

    let mut audit = SproutAudit::default();
    let policy = WorkspacePolicy::new(workspace).ok();
    match SproutConfig::load_from_file(&path, workspace, policy.as_ref()) {
        Ok(cfg) => {
            let cli_label = cfg.effective_container_cli();
            audit.checks.push(format!(
                "sprout.toml valid ({} service(s), DAG acyclic, container_cli={})",
                cfg.services.len(),
                cli_label.as_deref().unwrap_or("none")
            ));

            if cfg.has_container_services() {
                match cli_label.as_deref() {
                    Some(cli) => {
                        let available = Command::new(cli)
                            .arg("--version")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false);
                        if !available {
                            audit.warnings.push(format!(
                                "sprout.toml declares container services, but `{cli}` was not found on PATH"
                            ));
                        }
                    }
                    None => {
                        audit.warnings.push(
                            "sprout.toml declares container services, but neither `docker`, `nerdctl`, nor `podman` was found on PATH"
                                .to_string(),
                        );
                    }
                }
            }

            for (id, svc) in &cfg.services {
                if let Some(port) = svc.ready_port {
                    if probe_local_tcp_port(port, Duration::from_millis(100)) {
                        audit.warnings.push(format!(
                            "sprout.toml: port {port} for service `{id}` is already in use on 127.0.0.1"
                        ));
                    }
                }
            }
        }
        Err(err) => {
            audit
                .errors
                .push(format!("invalid `{}`: {err}", path.display()));
        }
    }

    Some(audit)
}

/// Convierte una salida `@TOON v1` de `service_status`/`service_start`/`service_stop`
/// en líneas limpias para el panel `Services (Sprout)` de la TUI (`ws.services.text`).
pub fn format_services_panel_from_toon(text: &str) -> String {
    let mut in_toon = false;
    let mut lines = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed == "@TOON v1" {
            in_toon = true;
            continue;
        }
        if !in_toon || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = trimmed.split('|').map(str::trim).collect();
        if cols.len() < 4 {
            continue;
        }
        let id = cols[0];
        let kind = cols[1];
        let status = cols[2];
        let port = cols[3];
        let port_suffix = if port != "-" && !port.is_empty() {
            format!(" :{port}")
        } else {
            String::new()
        };
        let badge = status.split('(').next().unwrap_or(status);
        lines.push(format!("[{badge}] {id} ({kind}{port_suffix})"));
    }
    lines.join("\n")
}

fn format_status_toon_locked(guard: &SproutState) -> String {
    let mut out = vec![
        "@TOON v1".to_string(),
        "# cols: id | kind | status | port | pid | deps".to_string(),
    ];
    let order = guard
        .config
        .as_ref()
        .and_then(|c| c.topo_order().ok())
        .unwrap_or_else(|| guard.services.keys().cloned().collect());

    for id in &order {
        if let Some(svc) = guard.services.get(id) {
            let port = svc
                .spec
                .ready_port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            let pid = svc
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            let deps = if svc.spec.depends_on.is_empty() {
                "-".to_string()
            } else {
                svc.spec.depends_on.join(",")
            };
            out.push(format!(
                "{} | {} | {} | {} | {} | {}",
                svc.spec.id,
                svc.spec.kind.as_str(),
                svc.status.as_toon_cell(),
                port,
                pid,
                deps
            ));
        }
    }
    out.push(format!("# total: {}", order.len()));
    out.join("\n")
}

fn stop_all_locked(policy: &WorkspacePolicy, state: &Mutex<SproutState>) {
    let ids = {
        let Ok(guard) = state.lock() else { return };
        let mut order = guard
            .config
            .as_ref()
            .and_then(|c| c.topo_order().ok())
            .unwrap_or_else(|| guard.services.keys().cloned().collect());
        order.reverse();
        order
    };
    for id in ids {
        let _ = stop_single_service(policy, state, &id);
    }
}

fn stop_single_service(policy: &WorkspacePolicy, state: &Mutex<SproutState>, id: &str) -> bool {
    let (was_active, stop_cmd_opt, cwd, job, logs_job) = {
        let Ok(mut guard) = state.lock() else {
            return false;
        };
        let Some(svc) = guard.services.get_mut(id) else {
            return false;
        };
        let active =
            svc.status != ServiceStatus::Off || svc.job.is_some() || svc.logs_job.is_some();
        svc.generation = svc.generation.saturating_add(1);
        svc.status = ServiceStatus::Off;
        svc.pid = None;
        svc.reset_probes();
        if active {
            svc.push_log(format!("[ORCHESTRATOR] Stopping {}...", svc.spec.name));
        }
        (
            active,
            svc.spec.stop_cmd.clone(),
            svc.spec.cwd.clone(),
            svc.job.take(),
            svc.logs_job.take(),
        )
    };

    if let Some(mut j) = job {
        j.terminate();
    }
    if let Some(mut lj) = logs_job {
        lj.terminate();
    }
    if was_active {
        if let Some(stop_cmd) = stop_cmd_opt {
            if let Ok(sandbox_tmp) = ensure_sandbox_tmp_dir(&policy.root) {
                let mut cmd = build_shell_command(&stop_cmd, &cwd);
                apply_sanitized_env(&mut cmd, &policy.sandbox, &policy.root, &sandbox_tmp);
                configure_process_isolation(&mut cmd, true);
                let _ = run_command_bounded(
                    cmd,
                    Duration::from_secs(15),
                    8 * 1024,
                    policy.sandbox.limits(),
                    &format!("stop({id})"),
                );
            }
        }
    }
    was_active
}

fn verify_container_daemon(cli: &str, policy: &WorkspacePolicy) -> Result<(), String> {
    let sandbox_tmp = ensure_sandbox_tmp_dir(&policy.root)?;
    let mut cmd = Command::new(cli);
    cmd.arg("info")
        .current_dir(clean_process_path(&policy.root))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_sanitized_env(&mut cmd, &policy.sandbox, &policy.root, &sandbox_tmp);
    configure_process_isolation(&mut cmd, true);

    let (code, _out, err) = run_command_bounded(
        cmd,
        Duration::from_secs(10),
        4096,
        policy.sandbox.limits(),
        &format!("{cli} info"),
    )?;
    if code != 0 {
        return Err(format!(
            "container daemon check `{cli} info` failed (exit {code}): {}",
            scrub_secrets(err.trim())
        ));
    }
    Ok(())
}

fn probe_local_tcp_port(port: u16, timeout: Duration) -> bool {
    let v4 = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&v4, timeout) {
        Ok(_) => return true,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            // In restricted sandboxes where loopback connect() is denied by OS policy
            // (e.g., Windows AppContainer/WFP `WSAEACCES`), detect an active listener safely
            // by verifying whether binding `127.0.0.1:port` fails with `AddrInUse`.
            return matches!(
                TcpListener::bind(v4),
                Err(bind_err) if bind_err.kind() == ErrorKind::AddrInUse
            );
        }
        Err(_) => {}
    }
    let v6 = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port));
    TcpStream::connect_timeout(&v6, timeout).is_ok()
}

fn run_silent_probe_cmd(check_cmd: &str, cwd: &Path, policy: &WorkspacePolicy) -> bool {
    let Ok(sandbox_tmp) = ensure_sandbox_tmp_dir(&policy.root) else {
        return false;
    };
    let mut cmd = build_shell_command(check_cmd, cwd);
    apply_sanitized_env(&mut cmd, &policy.sandbox, &policy.root, &sandbox_tmp);
    configure_process_isolation(&mut cmd, true);
    run_command_bounded(
        cmd,
        Duration::from_secs(5),
        1024,
        policy.sandbox.limits(),
        "ready_cmd",
    )
    .map(|(code, _, _)| code == 0)
    .unwrap_or(false)
}

fn build_shell_command(raw_cmd: &str, cwd: &Path) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.current_dir(clean_process_path(cwd))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c.raw_arg(format!("/C \"{raw_cmd}\""));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.args(["-c", raw_cmd])
            .current_dir(clean_process_path(cwd))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }
}

fn stream_pipe_to_logs<R: Read>(
    pipe: R,
    inner: Arc<SproutInner>,
    svc_id: &str,
    generation: u64,
    ready_marker: Option<&str>,
    is_stderr: bool,
) {
    let mut reader = BufReader::new(pipe);
    let mut buf = Vec::new();
    while let Ok(n) = reader.read_until(b'\n', &mut buf) {
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_string();
        buf.clear();

        let Ok(mut guard) = inner.state.lock() else {
            break;
        };
        let Some(svc) = guard.services.get_mut(svc_id) else {
            break;
        };
        if svc.generation != generation {
            break;
        }
        if let Some(marker) = ready_marker {
            if line.contains(marker) && svc.status == ServiceStatus::Starting {
                svc.log_matched = true;
                if svc.check_all_probes_satisfied() {
                    svc.status = ServiceStatus::Ready;
                }
            }
        }
        if is_stderr {
            svc.push_log(format!("[ERR] {line}"));
        } else {
            svc.push_log(line);
        }
    }
}

fn run_command_bounded(
    mut cmd: Command,
    timeout: Duration,
    max_output_bytes: usize,
    limits: crate::sandbox::ProcessLimits,
    label: &str,
) -> Result<(i32, String, String), String> {
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {label}: {e}"))?;
    let mut job = ProcessJob::assign(&child, limits);

    let out_handle = spawn_pipe_reader(child.stdout.take(), max_output_bytes);
    let err_handle = spawn_pipe_reader(child.stderr.take(), max_output_bytes);
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    if let Some(ref mut j) = job {
                        j.terminate();
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err(format!("{label} timed out after {timeout:?}"));
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("{label} wait failed: {e}")),
        }
    };

    drop(job);
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

fn spawn_pipe_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
    cap: usize,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let Some(mut r) = pipe else {
            return String::new();
        };
        let mut collected = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if collected.len() < cap {
                        let take = (cap - collected.len()).min(n);
                        collected.extend_from_slice(&buf[..take]);
                    }
                }
            }
        }
        String::from_utf8_lossy(&collected).into_owned()
    })
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[truncated]", &s[..end])
}
