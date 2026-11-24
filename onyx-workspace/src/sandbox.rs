//! Sandboxing y aislamiento a nivel de sistema operativo (SDD-06).
//!
//! Diseñado para ser compartido tanto por `WorkspaceTools::shell_exec` (SDD-06)
//! como por `SproutSupervisor` (SDD-08):
//! - Sanitización estricta de entorno (`env_clear()` + allowlist segura).
//! - RAII [`ProcessJob`] con Win32 Job Objects (`KILL_ON_JOB_CLOSE`, memoria y límite de PIDs)
//!   y grupos de procesos POSIX (`PGID`) para evitar procesos zombi/huérfanos.
//! - Drivers de aislamiento multiplataforma (`bwrap`, `landlock`, `sandbox-exec`, `win32-job`,
//!   contenedor OCI `docker`/`podman`/`nerdctl`) y detección de capacidades para `openonyx doctor`.
//! - Protección del *Control Plane* (`.onyx/config.toml`, `.git/hooks/**`, `.git/config`, `.env*`)
//!   y política de red configurable (`offline`, `localhost`, `unrestricted`).

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::policy::{is_control_plane_rel_path, strip_toml_comment_pub};

/// Nivel de aislamiento aplicado a comandos ejecutados por el agente.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Sin sandbox OS (solo `WorkspacePolicy` + timeout); emite aviso en `doctor`/TUI.
    Off,
    /// Recomendado por defecto: `env_clear()` + escritura limitada a workspace/tmp + red offline.
    #[default]
    WorkspaceWrite,
    /// Máximo aislamiento para `--auto-approve` / `bench`: contenedor OCI (`docker`/`podman`/`nerdctl`).
    StrictContainer,
}

impl SandboxMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => Some(Self::Off),
            "workspace-write" | "workspace" | "ws" => Some(Self::WorkspaceWrite),
            "strict-container" | "strict" | "container" => Some(Self::StrictContainer),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceWrite => "workspace-write",
            Self::StrictContainer => "strict-container",
        }
    }

    /// Etiqueta corta para el badge `[SANDBOX:<label>]` en la TUI (`REQ-06.6`).
    pub fn as_badge(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceWrite => "workspace",
            Self::StrictContainer => "strict",
        }
    }

    /// Etiqueta honesta del badge `[SANDBOX:<label>]` teniendo en cuenta el driver real del host:
    /// - En Linux (`bwrap`) y macOS (`sandbox-exec`), `WorkspaceWrite` ofrece aislamiento FS/red (`workspace`).
    /// - En Windows (`Win32Job`), `WorkspaceWrite` ofrece `env_clear` + Job Objects (`env+job`), sin
    ///   aislamiento de filesystem/red a nivel kernel.
    /// - Sin driver nativo de FS/red (`Landlock` sin `bwrap` o `None`), muestra `env+pgid`.
    pub fn effective_badge(self, driver: SandboxDriverKind) -> &'static str {
        match self {
            Self::Off => "off",
            Self::StrictContainer => "strict",
            Self::WorkspaceWrite => match driver {
                SandboxDriverKind::Bwrap | SandboxDriverKind::SandboxExec => "workspace",
                SandboxDriverKind::Win32Job => "env+job",
                SandboxDriverKind::Landlock | SandboxDriverKind::None => "env+pgid",
            },
        }
    }

    fn strictness_rank(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::WorkspaceWrite => 1,
            Self::StrictContainer => 2,
        }
    }
}

/// Política de acceso a red para subprocesos bajo sandbox (`REQ-06.3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkPolicy {
    /// Sin acceso a red saliente (default seguro bajo sandbox).
    #[default]
    Offline,
    /// Permite solo `127.0.0.1` / `localhost` (p. ej. probar APIs locales de Sprout).
    LocalhostOnly,
    /// Sin restricción de red saliente.
    Unrestricted,
}

impl NetworkPolicy {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "offline" | "none" => Some(Self::Offline),
            "localhost" | "localhost-only" | "local" => Some(Self::LocalhostOnly),
            "unrestricted" | "allow" | "full" => Some(Self::Unrestricted),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::LocalhostOnly => "localhost",
            Self::Unrestricted => "unrestricted",
        }
    }

    fn strictness_rank(self) -> u8 {
        match self {
            Self::Unrestricted => 0,
            Self::LocalhostOnly => 1,
            Self::Offline => 2,
        }
    }
}

/// Runtime de contenedores OCI cerrado (`docker`, `nerdctl` o `podman`).
/// Nunca acepta rutas arbitrarias ni argumentos desde `sprout.toml` (cierra R-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerCli {
    Docker,
    Nerdctl,
    Podman,
}

impl ContainerCli {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "docker" => Some(Self::Docker),
            "nerdctl" => Some(Self::Nerdctl),
            "podman" => Some(Self::Podman),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Nerdctl => "nerdctl",
            Self::Podman => "podman",
        }
    }
}

/// Valida una referencia de imagen OCI (`[registry[:port]/]name[:tag][@sha256:<64hex>]`)
/// rechazando inyección de flags (`--privileged`), espacios y caracteres shell (cierra R-4).
pub fn validate_oci_image_ref(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("container_image cannot be empty".to_string());
    }
    if trimmed.len() > 256 {
        return Err("container_image exceeds 256 characters".to_string());
    }
    if trimmed.starts_with('-') || trimmed.starts_with('/') || trimmed.ends_with('/') {
        return Err(format!(
            "invalid container_image `{trimmed}`: cannot start with `-` or start/end with `/`"
        ));
    }
    if trimmed.contains("..") || trimmed.contains("//") {
        return Err(format!(
            "invalid container_image `{trimmed}`: contains disallowed `..` or `//`"
        ));
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b':' | b'@' | b'-'))
    {
        return Err(format!(
            "invalid container_image `{trimmed}`: only ASCII `[A-Za-z0-9._/:@-]` allowed (no spaces or shell characters)"
        ));
    }
    let (name_and_tag, digest_opt) = match trimmed.split_once('@') {
        Some((left, right)) => (left, Some(right)),
        None => (trimmed, None),
    };
    if name_and_tag.is_empty() {
        return Err(format!(
            "invalid container_image `{trimmed}`: missing image name"
        ));
    }
    if let Some(digest) = digest_opt {
        let Some(hex) = digest.strip_prefix("sha256:") else {
            return Err(format!(
                "invalid container_image `{trimmed}`: digest must start with `sha256:`"
            ));
        };
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "invalid container_image `{trimmed}`: sha256 digest must be 64 hex chars"
            ));
        }
    }
    // El último segmento (repo[:tag]) no puede empezar ni terminar en `:` ni empezar por `.`/`-`.
    let last_seg = name_and_tag.rsplit('/').next().unwrap_or("");
    let (repo_leaf, tag_opt) = match last_seg.split_once(':') {
        Some((r, t)) => (r, Some(t)),
        None => (last_seg, None),
    };
    if repo_leaf.is_empty()
        || !repo_leaf
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(format!(
            "invalid container_image `{trimmed}`: repository segment must start with an alphanumeric character"
        ));
    }
    if let Some(tag) = tag_opt {
        if tag.is_empty()
            || tag.len() > 128
            || tag.starts_with('-')
            || tag.starts_with('.')
            || tag.contains(':')
        {
            return Err(format!(
                "invalid container_image `{trimmed}`: invalid tag `{tag}`"
            ));
        }
    }
    Ok(trimmed.to_string())
}

/// Driver de sandbox detectado en el sistema anfitrión (`TEST-06.5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxDriverKind {
    Bwrap,
    Landlock,
    SandboxExec,
    Win32Job,
    None,
}

impl SandboxDriverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bwrap => "bwrap",
            Self::Landlock => "landlock",
            Self::SandboxExec => "sandbox-exec",
            Self::Win32Job => "win32-job",
            Self::None => "none",
        }
    }
}

/// Capacidades de sandboxing y runtime OCI disponibles en el host actual.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCapabilities {
    pub native_driver: SandboxDriverKind,
    pub container_cli: Option<&'static str>,
    pub notes: Vec<String>,
}

impl SandboxCapabilities {
    /// Indica si `mode` proporciona aislamiento fuerte de filesystem y red a nivel de SO/contenedor.
    pub fn is_hard_fs_net_isolated(&self, mode: SandboxMode) -> bool {
        match mode {
            SandboxMode::Off => false,
            SandboxMode::StrictContainer => self.container_cli.is_some(),
            SandboxMode::WorkspaceWrite => matches!(
                self.native_driver,
                SandboxDriverKind::Bwrap | SandboxDriverKind::SandboxExec
            ),
        }
    }
}

/// Límites de recursos aplicables a un [`ProcessJob`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessLimits {
    pub max_memory_mb: Option<u64>,
    pub max_pids: Option<u32>,
}

/// Configuración de sandbox cargada desde `[sandbox]` en `.onyx/config.toml` y flags CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    pub network: NetworkPolicy,
    pub allow_read_paths: Vec<PathBuf>,
    pub env_passthrough: Vec<String>,
    pub max_memory_mb: Option<u64>,
    pub max_pids: Option<u32>,
    pub container_image: Option<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::WorkspaceWrite,
            network: NetworkPolicy::Offline,
            allow_read_paths: Vec::new(),
            env_passthrough: Vec::new(),
            max_memory_mb: Some(4096),
            max_pids: Some(128),
            container_image: None,
        }
    }
}

/// Parche parcial leído desde `[sandbox]` en un archivo `.onyx/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxPatch {
    pub mode: Option<SandboxMode>,
    pub network: Option<NetworkPolicy>,
    pub allow_read_paths: Vec<PathBuf>,
    pub env_passthrough: Vec<String>,
    pub max_memory_mb: Option<u64>,
    pub max_pids: Option<u32>,
    pub container_image: Option<String>,
}

impl SandboxConfig {
    pub fn limits(&self) -> ProcessLimits {
        ProcessLimits {
            max_memory_mb: self.max_memory_mb,
            max_pids: self.max_pids,
        }
    }

    /// Aplica un parche de configuración manteniendo los valores no especificados.
    pub fn apply_patch(&mut self, patch: SandboxPatch) {
        if let Some(m) = patch.mode {
            self.mode = m;
        }
        if let Some(n) = patch.network {
            self.network = n;
        }
        for p in patch.allow_read_paths {
            if !self.allow_read_paths.contains(&p) {
                self.allow_read_paths.push(p);
            }
        }
        for k in patch.env_passthrough {
            if !self.env_passthrough.contains(&k) {
                self.env_passthrough.push(k);
            }
        }
        if let Some(mem) = patch.max_memory_mb {
            self.max_memory_mb = Some(mem);
        }
        if let Some(pids) = patch.max_pids {
            self.max_pids = Some(pids);
        }
        if let Some(img) = patch.container_image {
            self.container_image = Some(img);
        }
    }

    /// Aplica un parche proveniente de un repositorio NO confiado: solo permite endurecer
    /// (`mode` y `network` más restrictivos, límites de memoria/PID menores); ignora con aviso
    /// `env_passthrough`, `allow_read_paths`, `container_image` y cualquier relajación.
    pub fn apply_patch_restrictive(&mut self, patch: SandboxPatch) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(m) = patch.mode {
            match m.strictness_rank().cmp(&self.mode.strictness_rank()) {
                std::cmp::Ordering::Greater => self.mode = m,
                std::cmp::Ordering::Less => warnings.push(format!(
                    "untrusted project `.onyx/config.toml` cannot relax `[sandbox].mode` from `{}` to `{}`",
                    self.mode.as_str(),
                    m.as_str()
                )),
                std::cmp::Ordering::Equal => {}
            }
        }
        if let Some(n) = patch.network {
            match n.strictness_rank().cmp(&self.network.strictness_rank()) {
                std::cmp::Ordering::Greater => self.network = n,
                std::cmp::Ordering::Less => warnings.push(format!(
                    "untrusted project `.onyx/config.toml` cannot relax `[sandbox].network` from `{}` to `{}`",
                    self.network.as_str(),
                    n.as_str()
                )),
                std::cmp::Ordering::Equal => {}
            }
        }
        if !patch.allow_read_paths.is_empty() {
            warnings.push(
                "untrusted project `.onyx/config.toml` cannot add `[sandbox].allow_read_paths`"
                    .to_string(),
            );
        }
        if !patch.env_passthrough.is_empty() {
            warnings.push(
                "untrusted project `.onyx/config.toml` cannot add `[sandbox].env_passthrough`"
                    .to_string(),
            );
        }
        if let Some(mem) = patch.max_memory_mb {
            match self.max_memory_mb {
                Some(cur) if mem < cur => self.max_memory_mb = Some(mem),
                None => self.max_memory_mb = Some(mem),
                _ => warnings.push(
                    "untrusted project `.onyx/config.toml` cannot raise `[sandbox].max_memory_mb`"
                        .to_string(),
                ),
            }
        }
        if let Some(pids) = patch.max_pids {
            match self.max_pids {
                Some(cur) if pids < cur => self.max_pids = Some(pids),
                None => self.max_pids = Some(pids),
                _ => warnings.push(
                    "untrusted project `.onyx/config.toml` cannot raise `[sandbox].max_pids`"
                        .to_string(),
                ),
            }
        }
        if patch.container_image.is_some() {
            warnings.push(
                "untrusted project `.onyx/config.toml` cannot override `[sandbox].container_image`"
                    .to_string(),
            );
        }
        warnings
    }

    /// Elevación automática de seguridad para `--auto-approve` (`REQ-06.5`):
    /// si el usuario no pasó `--sandbox` explícitamente por CLI y el modo estaba en `Off`,
    /// eleva a `WorkspaceWrite` + `Offline`.
    pub fn elevate_for_auto_approve(&mut self, explicit_cli_mode: bool) {
        if !explicit_cli_mode && self.mode == SandboxMode::Off {
            self.mode = SandboxMode::WorkspaceWrite;
            self.network = NetworkPolicy::Offline;
        }
    }

    /// Parsea la sección `[sandbox]` de un archivo TOML (`.onyx/config.toml`).
    pub fn parse_toml_patch(input: &str) -> Result<SandboxPatch, String> {
        let mut patch = SandboxPatch::default();
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);
        let lines: Vec<&str> = text.lines().collect();
        let mut in_sandbox = false;
        let mut i = 0usize;

        while i < lines.len() {
            let line_no = i + 1;
            let stripped = strip_toml_comment_pub(lines[i]).trim();
            if stripped.is_empty() {
                i += 1;
                continue;
            }
            if stripped.starts_with('[') && stripped.ends_with(']') {
                let header = stripped[1..stripped.len() - 1].trim();
                in_sandbox = header == "sandbox";
                i += 1;
                continue;
            }
            if !in_sandbox {
                i += 1;
                continue;
            }

            let Some((raw_key, raw_val)) = stripped.split_once('=') else {
                return Err(format!(
                    "config.toml line {line_no}: expected `key = value` inside [sandbox]"
                ));
            };
            let key = raw_key.trim();
            let mut val_buf = raw_val.trim().to_string();

            if val_buf.starts_with('[') {
                while !crate::policy::array_brackets_closed_pub(&val_buf) {
                    i += 1;
                    if i >= lines.len() {
                        return Err(format!(
                            "config.toml line {line_no}: unterminated array for `[sandbox].{key}`"
                        ));
                    }
                    let next_piece = strip_toml_comment_pub(lines[i]).trim();
                    if !next_piece.is_empty() {
                        val_buf.push(' ');
                        val_buf.push_str(next_piece);
                    }
                }
            }

            match key {
                "mode" => {
                    let s = parse_toml_scalar_string(&val_buf)
                        .map_err(|e| format!("config.toml line {line_no} (`mode`): {e}"))?;
                    let parsed = SandboxMode::parse(&s).ok_or_else(|| {
                        format!(
                            "config.toml line {line_no}: invalid sandbox mode `{s}` (expected `off`, `workspace-write`, or `strict-container`)"
                        )
                    })?;
                    patch.mode = Some(parsed);
                }
                "network" => {
                    let s = parse_toml_scalar_string(&val_buf)
                        .map_err(|e| format!("config.toml line {line_no} (`network`): {e}"))?;
                    let parsed = NetworkPolicy::parse(&s).ok_or_else(|| {
                        format!(
                            "config.toml line {line_no}: invalid sandbox network `{s}` (expected `offline`, `localhost`, or `unrestricted`)"
                        )
                    })?;
                    patch.network = Some(parsed);
                }
                "env_passthrough" => {
                    let items =
                        crate::policy::parse_toml_string_array_pub(&val_buf).map_err(|e| {
                            format!("config.toml line {line_no} (`env_passthrough`): {e}")
                        })?;
                    patch.env_passthrough.extend(items);
                }
                "allow_read_paths" => {
                    let items =
                        crate::policy::parse_toml_string_array_pub(&val_buf).map_err(|e| {
                            format!("config.toml line {line_no} (`allow_read_paths`): {e}")
                        })?;
                    patch
                        .allow_read_paths
                        .extend(items.into_iter().map(PathBuf::from));
                }
                "max_memory_mb" => {
                    let parsed = val_buf.replace('_', "").parse::<u64>().map_err(|_| {
                        format!("config.toml line {line_no}: `max_memory_mb` must be an integer")
                    })?;
                    patch.max_memory_mb = Some(parsed);
                }
                "max_pids" => {
                    let parsed = val_buf.replace('_', "").parse::<u32>().map_err(|_| {
                        format!("config.toml line {line_no}: `max_pids` must be an integer")
                    })?;
                    patch.max_pids = Some(parsed);
                }
                "container_image" => {
                    let s = parse_toml_scalar_string(&val_buf).map_err(|e| {
                        format!("config.toml line {line_no} (`container_image`): {e}")
                    })?;
                    if !s.trim().is_empty() {
                        let validated = validate_oci_image_ref(&s).map_err(|e| {
                            format!("config.toml line {line_no} (`container_image`): {e}")
                        })?;
                        patch.container_image = Some(validated);
                    }
                }
                other => {
                    return Err(format!(
                        "config.toml line {line_no}: unknown key `{other}` in [sandbox]"
                    ));
                }
            }
            i += 1;
        }

        Ok(patch)
    }

    /// Carga y fusiona `[sandbox]` desde `~/.onyx/config.toml` y `<workspace>/.onyx/config.toml`.
    pub fn load_for_workspace(workspace_root: &Path) -> Self {
        let global_path = user_home_dir().map(|h| h.join(".onyx").join("config.toml"));
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
                cfg.apply_patch(Self::parse_toml_patch(&raw)?);
            }
        }
        if let Some(pp) = project_path {
            if pp.is_file() {
                let raw = fs::read_to_string(pp)
                    .map_err(|e| format!("failed to read `{}`: {e}", pp.display()))?;
                let patch = Self::parse_toml_patch(&raw)?;
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

/// Detecta el driver nativo de sandbox y runtime de contenedores disponibles en el host (`TEST-06.5`).
pub fn detect_sandbox_capabilities() -> SandboxCapabilities {
    let mut notes = Vec::new();
    let container_cli = detect_container_cli();

    #[cfg(target_os = "linux")]
    let native_driver = {
        if bwrap_is_usable() {
            notes
                .push("bubblewrap (`bwrap`) available for filesystem and network isolation".into());
            SandboxDriverKind::Bwrap
        } else if linux_landlock_supported() {
            notes.push(
                "Linux Landlock LSM active in kernel (install/enable `bwrap` userns for full --unshare-net)"
                    .into(),
            );
            SandboxDriverKind::Landlock
        } else {
            notes.push(
                "neither usable `bwrap` nor Landlock LSM detected; only env_clear and PGID limits apply"
                    .into(),
            );
            SandboxDriverKind::None
        }
    };

    #[cfg(target_os = "macos")]
    let native_driver = {
        if Path::new("/usr/bin/sandbox-exec").is_file() {
            notes.push(
                "macOS `sandbox-exec` (Seatbelt) available for FS and network isolation".into(),
            );
            SandboxDriverKind::SandboxExec
        } else {
            notes.push(
                "`/usr/bin/sandbox-exec` not found; only env_clear and PGID limits apply".into(),
            );
            SandboxDriverKind::None
        }
    };

    #[cfg(windows)]
    let native_driver = {
        if win::probe_job_object_available() {
            notes.push(
                "Win32 Job Objects (`KILL_ON_JOB_CLOSE`, memory/PID limits) + env_clear active (no kernel FS/network namespace isolation; use `--sandbox strict-container` for hard isolation)"
                    .into(),
            );
            SandboxDriverKind::Win32Job
        } else {
            notes.push("Win32 Job Object creation unavailable".into());
            SandboxDriverKind::None
        }
    };

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let native_driver = {
        notes.push("no native OS sandbox driver for this target; env_clear applies".into());
        SandboxDriverKind::None
    };

    if let Some(cli) = container_cli {
        notes.push(format!("OCI container runtime `{cli}` detected on PATH"));
    }

    SandboxCapabilities {
        native_driver,
        container_cli,
        notes,
    }
}

/// Detecta el primer CLI de contenedores disponible (`docker`, `nerdctl` o `podman`).
pub fn detect_container_cli() -> Option<&'static str> {
    ["docker", "nerdctl", "podman"]
        .into_iter()
        .find(|&bin| command_succeeds(bin, &["--version"]))
}

/// Indica si el nombre de una variable de entorno corresponde a un secreto o credencial
/// que nunca debe heredarse por defecto en subprocesos (`REQ-06.1`).
pub fn is_sensitive_env_key(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.contains("API_KEY")
        || upper.contains("SECRET")
        || upper.contains("TOKEN")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.contains("CREDENTIAL")
        || upper.contains("PRIVATE_KEY")
        || upper.starts_with("AWS_")
        || upper.starts_with("AZURE_")
        || upper.starts_with("GCP_")
        || upper.starts_with("GOOGLE_")
        || upper.starts_with("GEMINI_")
        || upper.starts_with("OPENAI_")
        || upper.starts_with("ANTHROPIC_")
        || upper.starts_with("ONYX_API")
        || upper.starts_with("GITHUB_")
        || upper.starts_with("GH_")
        || upper.starts_with("GITLAB_")
        || upper.starts_with("NPM_TOKEN")
        || upper.starts_with("CARGO_REGISTRY_")
}

/// Aplica sanitización estricta de variables de entorno sobre `cmd` (`REQ-06.1`).
///
/// Reutilizable por `WorkspaceTools::shell_exec` y por `SproutSupervisor` (SDD-08):
/// - Arranca con `cmd.env_clear()`.
/// - Solo copia una allowlist mínima de variables de runtime del SO y toolchains.
/// - **Nunca** hereda `GEMINI_API_KEY`, `OPENAI_API_KEY`, `AWS_*`, `*_TOKEN`, etc.
///   salvo opt-in explícito en `config.env_passthrough`.
/// - Redirige `TMP`/`TEMP`/`TMPDIR` a `sandbox_tmp` y añade hints offline cuando
///   `config.network == NetworkPolicy::Offline`.
pub fn apply_sanitized_env(
    cmd: &mut Command,
    config: &SandboxConfig,
    workspace_root: &Path,
    sandbox_tmp: &Path,
) {
    cmd.env_clear();

    const SAFE_BASE_KEYS: &[&str] = &[
        "PATH",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "COLORTERM",
        "NO_COLOR",
        "CLICOLOR",
        "TZ",
        "RUSTUP_HOME",
        "CARGO_HOME",
        "RUSTUP_TOOLCHAIN",
        "CARGO_TARGET_DIR",
        "RUSTFLAGS",
        "JAVA_HOME",
        "MAVEN_HOME",
        "M2_HOME",
        "GRADLE_HOME",
        "GOPATH",
        "GOROOT",
        "VIRTUAL_ENV",
        "CONDA_PREFIX",
        "NODE_OPTIONS",
    ];

    #[cfg(windows)]
    const SAFE_OS_KEYS: &[&str] = &[
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "OS",
        "PROCESSOR_ARCHITECTURE",
        "NUMBER_OF_PROCESSORS",
        "SYSTEMDRIVE",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "PROGRAMDATA",
        "COMMONPROGRAMFILES",
        "COMMONPROGRAMFILES(X86)",
        "LOCALAPPDATA",
        "APPDATA",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "PSModulePath",
    ];

    #[cfg(not(windows))]
    const SAFE_OS_KEYS: &[&str] = &["USER", "LOGNAME", "SHELL", "XDG_CACHE_HOME"];

    let mut allowed: BTreeSet<String> = BTreeSet::new();
    for k in SAFE_BASE_KEYS.iter().chain(SAFE_OS_KEYS.iter()) {
        allowed.insert((*k).to_string());
    }

    // Preservar valores reales del entorno actual solo para la allowlist segura (nunca secretos).
    for (k, v) in std::env::vars() {
        let in_default_allow = allowed.iter().any(|a| a.eq_ignore_ascii_case(&k));
        let in_passthrough = config
            .env_passthrough
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&k));
        if (in_default_allow && !is_sensitive_env_key(&k)) || in_passthrough {
            cmd.env(&k, v);
        }
    }

    // En Unix, redirigimos HOME al directorio temporal aislado preservando CARGO_HOME / RUSTUP_HOME.
    #[cfg(not(windows))]
    {
        if let Some(real_home) = user_home_dir() {
            if std::env::var_os("CARGO_HOME").is_none() {
                let cargo_home = real_home.join(".cargo");
                if cargo_home.exists() {
                    cmd.env("CARGO_HOME", cargo_home);
                }
            }
            if std::env::var_os("RUSTUP_HOME").is_none() {
                let rustup_home = real_home.join(".rustup");
                if rustup_home.exists() {
                    cmd.env("RUSTUP_HOME", rustup_home);
                }
            }
        }
        cmd.env("HOME", sandbox_tmp);
    }

    // En Windows, aseguramos CARGO_HOME/RUSTUP_HOME si existen bajo USERPROFILE.
    #[cfg(windows)]
    {
        if let Some(real_home) = user_home_dir() {
            if std::env::var_os("CARGO_HOME").is_none() {
                let cargo_home = real_home.join(".cargo");
                if cargo_home.exists() {
                    cmd.env("CARGO_HOME", cargo_home);
                }
            }
            if std::env::var_os("RUSTUP_HOME").is_none() {
                let rustup_home = real_home.join(".rustup");
                if rustup_home.exists() {
                    cmd.env("RUSTUP_HOME", rustup_home);
                }
            }
        }
    }

    // Directorio temporal acotado a la sesión.
    let clean_tmp = clean_process_path(sandbox_tmp);
    let clean_ws = clean_process_path(workspace_root);
    cmd.env("TMPDIR", &clean_tmp);
    cmd.env("TMP", &clean_tmp);
    cmd.env("TEMP", &clean_tmp);
    cmd.env("ONYX_SANDBOX_MODE", config.mode.as_str());
    cmd.env("ONYX_SANDBOX_NET", config.network.as_str());
    cmd.env("ONYX_WORKSPACE_ROOT", &clean_ws);

    if config.network == NetworkPolicy::Offline {
        cmd.env("CARGO_NET_OFFLINE", "true");
        cmd.env("PIP_NO_INDEX", "1");
        cmd.env("NPM_CONFIG_OFFLINE", "true");
        cmd.env("NO_PROXY", "127.0.0.1,localhost");
    }
}

/// Aplica sanitización estricta de variables de entorno al subproceso de un **harness CLI
/// externo** (`SDD-10` Fase 2). Mismo espíritu que [`apply_sanitized_env`], con las
/// diferencias del contrato del harness:
/// - **Nunca** hereda `NODE_OPTIONS` (vector de inyección `--require=...`) salvo opt-in
///   explícito en `passthrough` (el `allowed_env` del driver).
/// - El home del usuario (`HOME` / `USERPROFILE` / `APPDATA` / `LOCALAPPDATA`) solo se hereda
///   con `inherit_home = true`; en caso contrario se redirige a `sandbox_tmp` (sin `~/.ssh`,
///   `~/.aws`, configs del harness, etc.).
/// - No inyecta hints `*_OFFLINE`/`NO_PROXY` salvo `NetworkPolicy::Offline`: un harness con
///   `network_allowlist` deriva una política no-Offline (p. ej. `Unrestricted`) y necesita
///   llegar a su propia API.
pub fn apply_sanitized_env_for_external_harness(
    cmd: &mut Command,
    network: NetworkPolicy,
    workspace_root: &Path,
    sandbox_tmp: &Path,
    passthrough: &[String],
    inherit_home: bool,
) {
    cmd.env_clear();

    // Allowlist base de runtime/toolchains, **sin** `NODE_OPTIONS` (ver doc del función).
    const SAFE_BASE_KEYS: &[&str] = &[
        "PATH",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "COLORTERM",
        "NO_COLOR",
        "CLICOLOR",
        "TZ",
        "RUSTUP_HOME",
        "CARGO_HOME",
        "RUSTUP_TOOLCHAIN",
        "CARGO_TARGET_DIR",
        "RUSTFLAGS",
        "JAVA_HOME",
        "MAVEN_HOME",
        "M2_HOME",
        "GRADLE_HOME",
        "GOPATH",
        "GOROOT",
        "VIRTUAL_ENV",
        "CONDA_PREFIX",
    ];

    // Claves de SO **sin** las de home (`USERPROFILE`, `APPDATA`, ...): se tratan aparte
    // según `inherit_home`.
    #[cfg(windows)]
    const SAFE_OS_KEYS: &[&str] = &[
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "OS",
        "PROCESSOR_ARCHITECTURE",
        "NUMBER_OF_PROCESSORS",
        "SYSTEMDRIVE",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "PROGRAMDATA",
        "COMMONPROGRAMFILES",
        "COMMONPROGRAMFILES(X86)",
    ];

    #[cfg(not(windows))]
    const SAFE_OS_KEYS: &[&str] = &["SHELL", "XDG_CACHE_HOME"];

    for (k, v) in std::env::vars() {
        let in_default_allow = SAFE_BASE_KEYS
            .iter()
            .chain(SAFE_OS_KEYS.iter())
            .any(|a| a.eq_ignore_ascii_case(&k));
        let in_passthrough = passthrough.iter().any(|p| p.eq_ignore_ascii_case(&k));
        if in_passthrough || (in_default_allow && !is_sensitive_env_key(&k)) {
            cmd.env(&k, v);
        }
    }

    // Toolchains bajo el home real siguen funcionando con home aislado.
    if let Some(real_home) = user_home_dir() {
        if std::env::var_os("CARGO_HOME").is_none() {
            let cargo_home = real_home.join(".cargo");
            if cargo_home.exists() {
                cmd.env("CARGO_HOME", cargo_home);
            }
        }
        if std::env::var_os("RUSTUP_HOME").is_none() {
            let rustup_home = real_home.join(".rustup");
            if rustup_home.exists() {
                cmd.env("RUSTUP_HOME", rustup_home);
            }
        }
    }

    // Home: aislado en `sandbox_tmp` salvo opt-in explícito del driver.
    // `clean_process_path` (sin prefijo verbatim `\\?\`) ya que estos valores pasan a
    // interpretes que no lo entienden, igual que `TMP`/`TEMP` más abajo.
    let clean_tmp = clean_process_path(sandbox_tmp);
    #[cfg(not(windows))]
    {
        if inherit_home {
            if let Some(real_home) = user_home_dir() {
                cmd.env("HOME", real_home);
            }
        } else {
            cmd.env("HOME", &clean_tmp);
        }
    }
    #[cfg(windows)]
    {
        if inherit_home {
            for key in [
                "USERPROFILE",
                "APPDATA",
                "LOCALAPPDATA",
                "HOMEDRIVE",
                "HOMEPATH",
            ] {
                if let Ok(v) = std::env::var(key) {
                    cmd.env(key, v);
                }
            }
        } else {
            cmd.env("USERPROFILE", &clean_tmp);
            cmd.env("APPDATA", clean_tmp.join("appdata"));
            cmd.env("LOCALAPPDATA", clean_tmp.join("localappdata"));
        }
    }

    // Directorio temporal acotado a la sesión + marcadores del workspace (el worktree).
    let clean_ws = clean_process_path(workspace_root);
    cmd.env("TMPDIR", &clean_tmp);
    cmd.env("TMP", &clean_tmp);
    cmd.env("TEMP", &clean_tmp);
    cmd.env("ONYX_SANDBOX_NET", network.as_str());
    cmd.env("ONYX_WORKSPACE_ROOT", &clean_ws);

    if network == NetworkPolicy::Offline {
        cmd.env("CARGO_NET_OFFLINE", "true");
        cmd.env("PIP_NO_INDEX", "1");
        cmd.env("NPM_CONFIG_OFFLINE", "true");
        cmd.env("NO_PROXY", "127.0.0.1,localhost");
    }
}

/// Normaliza rutas canónicas para pasarlas como `current_dir` / variables de entorno a procesos
/// hijos: en Windows elimina el prefijo verbatim `\\?\` (p. ej. `\\?\C:\...` -> `C:\...`) para
/// que `pwsh`, `cmd.exe`, `mvn`, `java` y `node` operen sin errores de proveedor de rutas.
pub fn clean_process_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let raw = path.to_string_lossy();
        if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(stripped) = raw.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
        path.to_path_buf()
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// Binario resuelto explícitamente contra `PATH`/`PATHEXT` antes de `Command::new`
/// (`SDD-10` Fase 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProcessBinary {
    /// Ruta del ejecutable encontrado (sin re-resolución implícita de `Command`).
    pub path: PathBuf,
    /// `true` si resuelve a un wrapper de shell (`.cmd`/`.bat`): su argv pasa por `cmd.exe`
    /// y reinterpreta metacaracteres, por lo que `PromptTransport::Arg` se rechaza para ellos.
    pub is_shell_wrapper: bool,
}

/// Resuelve `bin` explícitamente contra `PATH` (`PATHEXT` en Windows) **antes** de
/// `Command::new` (`SDD-10` Fase 2, criterio de inyección): si `bin` contiene separador de
/// ruta se usa como camino directo (absoluto o relativo al CWD); si no, se busca por
/// entradas de `PATH` (en Windows probando además cada extensión de `PATHEXT`).
///
/// En Windows, un nombre **sin extensión** (p. ej. `opencode` de un install de npm) solo se
/// resuelve vía `PATHEXT`: junto al wrapper real `opencode.cmd`, npm instala un script POSIX
/// `opencode` (`#!/bin/sh`) que no es un Win32 ejecutable (el spawn fallaría con os error 193)
/// y además daría un falso negativo de `is_shell_wrapper` para el bloqueo de `PromptTransport::Arg`.
pub fn resolve_process_binary(bin: &str) -> Option<ResolvedProcessBinary> {
    resolve_process_binary_in(bin, std::env::var_os("PATH").as_deref())
}

/// Variante con `PATH` explícito (tests herméticos sin mutar el entorno del proceso).
pub fn resolve_process_binary_in(
    bin: &str,
    path_value: Option<&std::ffi::OsStr>,
) -> Option<ResolvedProcessBinary> {
    let trimmed = bin.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Con separador es una ruta directa, no un nombre de PATH.
    if trimmed.contains('/') || trimmed.contains('\\') {
        let p = PathBuf::from(trimmed);
        let p = if p.is_absolute() {
            p
        } else {
            std::env::current_dir().ok()?.join(p)
        };
        return classify_candidate(&p);
    }

    let path = path_value?;
    for dir in env::split_paths(path) {
        let dir = if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir().ok()?.join(dir)
        };
        if let Some(r) = classify_candidate(&dir.join(trimmed)) {
            return Some(r);
        }
    }
    None
}

/// Clasifica un candidato concreto a binario según la semántica de extensión de la plataforma
/// (ver [`resolve_process_binary`]):
/// - **Con** extensión: se clasifica tal cual (p. ej. `node.exe`, `cli.cmd`).
/// - **Sin** extensión en Windows: solo se prueban las variantes `PATHEXT`
///   (`.COM`/`.EXE`/`.BAT`/`.CMD`/...); nunca al fichero sin extensión (script POSIX de npm).
/// - **Sin** extensión en Unix: se clasifica tal cual (lo que cuenta es el bit de ejecución).
fn classify_candidate(name: &Path) -> Option<ResolvedProcessBinary> {
    #[cfg(not(windows))]
    {
        classify_resolved_binary(name)
    }
    #[cfg(windows)]
    {
        if name.extension().is_some() {
            return classify_resolved_binary(name);
        }
        for ext in pathext_list() {
            let candidate = name.with_extension(ext.trim_start_matches('.'));
            if let Some(r) = classify_resolved_binary(&candidate) {
                return Some(r);
            }
        }
        None
    }
}

/// Lista de extensiones de `PATHEXT` (con su punto), con default de Windows si no está definida.
#[cfg(windows)]
fn pathext_list() -> Vec<String> {
    let raw = std::env::var_os("PATHEXT").map(|p| p.to_string_lossy().into_owned());
    raw.map(|p| {
        p.split(';')
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.starts_with('.') {
                    s.to_string()
                } else {
                    format!(".{s}")
                }
            })
            .collect()
    })
    .filter(|v: &Vec<String>| !v.is_empty())
    .unwrap_or_else(|| vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()])
}

fn classify_resolved_binary(p: &Path) -> Option<ResolvedProcessBinary> {
    let meta = fs::metadata(p).ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    Some(ResolvedProcessBinary {
        path: p.to_path_buf(),
        is_shell_wrapper: matches!(ext.as_str(), "cmd" | "bat"),
    })
}

/// Crea (si no existe) un directorio temporal seguro específico para el sandbox del workspace.
pub fn ensure_sandbox_tmp_dir(workspace_root: &Path) -> Result<PathBuf, String> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in workspace_root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = std::env::temp_dir().join(format!("onyx-sandbox-{hash:016x}"));
    fs::create_dir_all(&dir).map_err(|e| {
        format!(
            "failed to prepare sandbox temp dir `{}`: {e}",
            dir.display()
        )
    })?;
    Ok(dir)
}

/// Verifica si un comando de shell intenta escribir o alterar rutas del Control Plane
/// (`.onyx/config.toml`, `.git/hooks`, `.git/config`, `.env*`) (`REQ-06.4`, `TEST-06.4`).
pub fn check_control_plane_shell_write(command: &str) -> Result<(), String> {
    let lower = command.to_ascii_lowercase();
    let mentions_cp = lower.contains(".onyx/config")
        || lower.contains(".onyx\\config")
        || lower.contains(".git/hooks")
        || lower.contains(".git\\hooks")
        || lower.contains(".git/config")
        || lower.contains(".git\\config")
        || mentions_dotenv_token(&lower);
    if !mentions_cp {
        return Ok(());
    }

    let is_write_intent = lower.contains('>')
        || lower.contains("set-content")
        || lower.contains("add-content")
        || lower.contains("out-file")
        || lower.contains("new-item")
        || lower.contains("remove-item")
        || lower.contains("move-item")
        || lower.contains("copy-item")
        || lower.contains("touch ")
        || lower.contains("tee ")
        || lower.contains("sed -i")
        || lower.contains("chmod ")
        || lower.contains("rm ")
        || lower.contains("del ")
        || lower.contains("mv ")
        || lower.contains("cp ")
        || lower.contains("truncate ")
        || lower.contains("git config");

    if is_write_intent {
        return Err(
            "blocked by control-plane protection (read-only): shell command cannot modify `.onyx/config.toml`, `.git/hooks/**`, `.git/config`, or `.env*`"
                .to_string(),
        );
    }
    Ok(())
}

/// Verifica en hosts sin aislamiento de namespace de red (p. ej. Windows sin `StrictContainer`)
/// que un comando bajo `NetworkPolicy::Offline` no intente conexiones salientes directas (`REQ-06.3`, `TEST-06.3`).
pub fn check_offline_network_command(command: &str, network: NetworkPolicy) -> Result<(), String> {
    if network == NetworkPolicy::Unrestricted {
        return Ok(());
    }
    let lower = command.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '\'' | '"'))
        .filter(|t| !t.is_empty())
        .collect();

    let is_network_tool = tokens.iter().any(|t| {
        matches!(
            *t,
            "curl"
                | "curl.exe"
                | "wget"
                | "wget.exe"
                | "invoke-webrequest"
                | "invoke-restmethod"
                | "iwr"
                | "irm"
                | "nc"
                | "ncat"
                | "netcat"
                | "telnet"
                | "ssh"
                | "scp"
                | "sftp"
                | "rsync"
                | "ftp"
                | "nslookup"
                | "dig"
        )
    }) || lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("tcpclient")
        || lower.contains("webclient")
        || (tokens.contains(&"git")
            && tokens
                .iter()
                .any(|t| matches!(*t, "fetch" | "pull" | "push" | "clone" | "ls-remote")));

    if !is_network_tool {
        return Ok(());
    }

    if network == NetworkPolicy::LocalhostOnly {
        let targets_only_local =
            lower.contains("127.0.0.1") || lower.contains("localhost") || lower.contains("[::1]");
        if targets_only_local && !contains_non_local_url(&lower) {
            return Ok(());
        }
        return Err(
            "blocked by sandbox network policy (localhost): outbound non-localhost network access requires `--sandbox-net unrestricted` or approval"
                .to_string(),
        );
    }

    Err(
        "blocked by sandbox network policy (offline): outbound network access is disabled; pass `--sandbox-net unrestricted` (or configure `[sandbox].network` in `.onyx/config.toml`) to allow"
            .to_string(),
    )
}

/// Construye un `std::process::Command` listo para ejecutar `raw_command` bajo `config` (`REQ-06.1`..`REQ-06.5`).
pub fn prepare_sandboxed_shell_command(
    config: &SandboxConfig,
    workspace_root: &Path,
    sandbox_tmp: &Path,
    raw_command: &str,
) -> Result<Command, String> {
    if raw_command.trim().is_empty() {
        return Err("command cannot be empty".to_string());
    }

    if config.mode == SandboxMode::Off {
        let mut cmd = plain_shell_command(raw_command);
        cmd.current_dir(clean_process_path(workspace_root));
        return Ok(cmd);
    }

    // Capa 1 (transversal): protección del Control Plane en cualquier plataforma.
    check_control_plane_shell_write(raw_command)?;

    // Capa 2: modo StrictContainer (contenedor OCI `docker`/`podman`/`nerdctl`).
    if config.mode == SandboxMode::StrictContainer {
        return build_container_command(config, workspace_root, raw_command);
    }

    // Capa 3: modo WorkspaceWrite según SO anfitrión.
    check_offline_network_command(raw_command, config.network)?;

    #[cfg(target_os = "linux")]
    {
        if bwrap_is_usable() {
            return build_bwrap_command(config, workspace_root, sandbox_tmp, raw_command);
        }
        let mut cmd = plain_shell_command(raw_command);
        cmd.current_dir(clean_process_path(workspace_root));
        apply_sanitized_env(&mut cmd, config, workspace_root, sandbox_tmp);
        Ok(cmd)
    }

    #[cfg(target_os = "macos")]
    {
        if Path::new("/usr/bin/sandbox-exec").is_file() {
            return build_seatbelt_command(config, workspace_root, sandbox_tmp, raw_command);
        }
        let mut cmd = plain_shell_command(raw_command);
        cmd.current_dir(clean_process_path(workspace_root));
        apply_sanitized_env(&mut cmd, config, workspace_root, sandbox_tmp);
        Ok(cmd)
    }

    #[cfg(windows)]
    {
        let mut cmd = plain_shell_command(raw_command);
        cmd.current_dir(clean_process_path(workspace_root));
        apply_sanitized_env(&mut cmd, config, workspace_root, sandbox_tmp);
        Ok(cmd)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let mut cmd = plain_shell_command(raw_command);
        cmd.current_dir(clean_process_path(workspace_root));
        apply_sanitized_env(&mut cmd, config, workspace_root, sandbox_tmp);
        Ok(cmd)
    }
}

/// Configura flags de grupo de proceso / ventana oculta en un `Command` antes de `spawn`.
/// Útil tanto para `shell_exec` como para procesos en segundo plano de `SproutSupervisor` (SDD-08).
pub fn configure_process_isolation(cmd: &mut Command, hide_window: bool) {
    #[cfg(unix)]
    {
        let _ = hide_window;
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        if hide_window {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }
}

fn plain_shell_command(raw_command: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("pwsh");
        c.args(["-NoProfile", "-Command", raw_command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", raw_command]);
        c
    }
}

fn build_container_command(
    config: &SandboxConfig,
    workspace_root: &Path,
    raw_command: &str,
) -> Result<Command, String> {
    let cli = detect_container_cli().ok_or_else(|| {
        "strict-container sandbox requested, but neither `docker`, `nerdctl`, nor `podman` was found on PATH"
            .to_string()
    })?;
    let image = match config.container_image.as_deref() {
        Some(raw_img) => validate_oci_image_ref(raw_img)?,
        None => "alpine:latest".to_string(),
    };
    let mut cmd = Command::new(cli);
    cmd.args(["run", "--rm", "-i", "--workdir", "/workspace"]);

    match config.network {
        NetworkPolicy::Offline => {
            cmd.arg("--network=none");
        }
        NetworkPolicy::LocalhostOnly => {
            cmd.arg("--network=host");
        }
        NetworkPolicy::Unrestricted => {}
    }
    if let Some(mem) = config.max_memory_mb {
        cmd.arg(format!("--memory={mem}m"));
    }
    if let Some(pids) = config.max_pids {
        cmd.arg(format!("--pids-limit={pids}"));
    }

    let ws_mount = format!("{}:/workspace", workspace_root.display());
    cmd.args(["-v", &ws_mount]);

    // Montaje read-only de ficheros existentes del Control Plane dentro del contenedor.
    for rel in [".onyx/config.toml", ".git/hooks", ".git/config", ".env"] {
        let host_path = workspace_root.join(rel);
        if host_path.exists() {
            cmd.args([
                "-v",
                &format!("{}:/workspace/{rel}:ro", host_path.display()),
            ]);
        }
    }

    for key in &config.env_passthrough {
        if let Ok(val) = std::env::var(key) {
            cmd.args(["-e", &format!("{key}={val}")]);
        }
    }
    cmd.args([image.as_str(), "sh", "-c", raw_command]);
    cmd.current_dir(workspace_root);
    Ok(cmd)
}

#[cfg(target_os = "linux")]
fn bwrap_is_usable() -> bool {
    use std::sync::OnceLock;
    static USABLE: OnceLock<bool> = OnceLock::new();
    *USABLE.get_or_init(|| {
        // En Ubuntu 24.04 (GitHub Actions) AppArmor puede restringir user namespaces
        // haciendo que `bwrap --version` funcione pero `bwrap --ro-bind / / ...` falle.
        command_succeeds(
            "bwrap",
            &[
                "--die-with-parent",
                "--unshare-pid",
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--",
                "true",
            ],
        )
    })
}

#[cfg(target_os = "linux")]
fn build_bwrap_command(
    config: &SandboxConfig,
    workspace_root: &Path,
    sandbox_tmp: &Path,
    raw_command: &str,
) -> Result<Command, String> {
    let mut cmd = Command::new("bwrap");
    cmd.args([
        "--die-with-parent",
        "--unshare-pid",
        "--new-session",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
    ]);
    if config.network == NetworkPolicy::Offline {
        cmd.arg("--unshare-net");
    }
    let ws = workspace_root.to_string_lossy().to_string();
    let tmp = sandbox_tmp.to_string_lossy().to_string();
    // Montar `/tmp` ANTES de `ws` para no ocultar workspaces que vivan bajo `/tmp` (p. ej. TempDir en tests).
    cmd.args(["--bind", tmp.as_str(), "/tmp"]);
    if tmp != "/tmp" {
        cmd.args(["--bind", tmp.as_str(), tmp.as_str()]);
    }
    cmd.args(["--bind", ws.as_str(), ws.as_str()]);

    // Ocultar credenciales sensibles del usuario en $HOME real.
    if let Some(real_home) = user_home_dir() {
        for sensitive in [".ssh", ".gnupg", ".aws", ".kube"] {
            let p = real_home.join(sensitive);
            if p.exists() {
                let s = p.to_string_lossy();
                cmd.args(["--tmpfs", s.as_ref()]);
            }
        }
    }

    // Rutas de solo lectura adicionales explícitas.
    for extra in &config.allow_read_paths {
        if extra.exists() {
            let s = extra.to_string_lossy();
            cmd.args(["--ro-bind", s.as_ref(), s.as_ref()]);
        }
    }

    // Re-montar Control Plane como read-only dentro del workspace.
    for rel in [".onyx/config.toml", ".git/hooks", ".git/config", ".env"] {
        let p = workspace_root.join(rel);
        if p.exists() {
            let s = p.to_string_lossy();
            cmd.args(["--ro-bind", s.as_ref(), s.as_ref()]);
        }
    }

    cmd.args(["--chdir", ws.as_str(), "sh", "-c", raw_command]);
    cmd.current_dir(workspace_root);
    apply_sanitized_env(&mut cmd, config, workspace_root, sandbox_tmp);
    Ok(cmd)
}

#[cfg(target_os = "macos")]
fn build_seatbelt_command(
    config: &SandboxConfig,
    workspace_root: &Path,
    sandbox_tmp: &Path,
    raw_command: &str,
) -> Result<Command, String> {
    let ws = escape_seatbelt_path(workspace_root);
    let tmp = escape_seatbelt_path(sandbox_tmp);
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    profile.push_str(&format!(
        "(allow file-write* (subpath \"{ws}\") (subpath \"{tmp}\") (subpath \"/dev\"))\n"
    ));

    // Control Plane read-only.
    for rel in [".onyx/config.toml", ".git/hooks", ".git/config", ".env"] {
        let p = escape_seatbelt_path(&workspace_root.join(rel));
        profile.push_str(&format!("(deny file-write* (subpath \"{p}\"))\n"));
    }

    // Ocultar lectura de credenciales en $HOME.
    if let Some(real_home) = user_home_dir() {
        for sensitive in [".ssh", ".gnupg", ".aws", ".kube"] {
            let p = escape_seatbelt_path(&real_home.join(sensitive));
            profile.push_str(&format!("(deny file-read* (subpath \"{p}\"))\n"));
        }
    }

    match config.network {
        NetworkPolicy::Offline => {
            profile.push_str("(deny network*)\n");
        }
        NetworkPolicy::LocalhostOnly => {
            profile.push_str("(deny network*)\n(allow network* (local ip \"localhost:*\") (remote ip \"localhost:*\"))\n");
        }
        NetworkPolicy::Unrestricted => {}
    }

    let mut cmd = Command::new("/usr/bin/sandbox-exec");
    cmd.args(["-p", &profile, "sh", "-c", raw_command]);
    cmd.current_dir(workspace_root);
    apply_sanitized_env(&mut cmd, config, workspace_root, sandbox_tmp);
    Ok(cmd)
}

#[cfg(target_os = "macos")]
fn escape_seatbelt_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn linux_landlock_supported() -> bool {
    fs::read_to_string("/sys/kernel/security/lsm")
        .map(|s| s.split(',').any(|m| m.trim() == "landlock"))
        .unwrap_or(false)
}

fn command_succeeds(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn parse_toml_scalar_string(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Ok(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        return Ok(inner.to_string());
    }
    Err(format!("expected quoted string, found `{trimmed}`"))
}

fn mentions_dotenv_token(lower: &str) -> bool {
    lower
        .split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | ';' | '|' | '&' | '>' | '<' | '(' | ')')
        })
        .any(|tok| !tok.is_empty() && is_control_plane_rel_path(tok))
}

fn contains_non_local_url(lower: &str) -> bool {
    for scheme in ["http://", "https://"] {
        let mut rest = lower;
        while let Some(idx) = rest.find(scheme) {
            let after = &rest[idx + scheme.len()..];
            let host = after
                .split(|c: char| c == '/' || c == ':' || c.is_whitespace() || c == '"' || c == '\'')
                .next()
                .unwrap_or("");
            if !host.is_empty() && host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
                return true;
            }
            rest = after;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// RAII ProcessJob (Win32 Job Objects / POSIX PGID) — compartido con SDD-08
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use super::ProcessLimits;

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
    const JOB_OBJECT_LIMIT_ACTIVE_PROCESS: u32 = 0x0000_0008;
    const JOB_OBJECT_LIMIT_JOB_MEMORY: u32 = 0x0000_0200;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(lp_job_attributes: *const c_void, lp_name: *const u16) -> isize;
        fn SetInformationJobObject(
            h_job: isize,
            job_object_information_class: u32,
            lp_job_object_information: *const c_void,
            cb_job_object_information_length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(h_job: isize, h_process: isize) -> i32;
        fn TerminateJobObject(h_job: isize, u_exit_code: u32) -> i32;
        fn CloseHandle(h_object: isize) -> i32;
    }

    pub fn probe_job_object_available() -> bool {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 {
                return false;
            }
            CloseHandle(job);
            true
        }
    }

    /// Handle RAII a un Win32 Job Object con `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    /// Al destruirse (`Drop`) o llamar a [`Self::terminate`], mata todo el árbol de procesos hijo.
    pub struct ProcessJob {
        handle: isize,
    }

    // Los handles de Win32 Job Object son válidos a nivel de proceso y seguros de mover entre hilos.
    unsafe impl Send for ProcessJob {}

    impl ProcessJob {
        pub fn assign(child: &Child, limits: ProcessLimits) -> Option<Self> {
            let raw_proc = child.as_raw_handle() as isize;
            if raw_proc == 0 {
                return None;
            }
            // Intenta primero con límites completos (KILL_ON_JOB_CLOSE + memoria + PIDs);
            // si un sandbox anidado rechaza límites extra, cae limpiamente a KILL_ON_JOB_CLOSE.
            if let Some(job) = Self::try_assign(raw_proc, limits, true) {
                return Some(job);
            }
            Self::try_assign(raw_proc, ProcessLimits::default(), false)
        }

        fn try_assign(raw_proc: isize, limits: ProcessLimits, apply_extras: bool) -> Option<Self> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job == 0 {
                    return None;
                }

                let mut info: JobObjectExtendedLimitInformation = zeroed();
                let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if apply_extras {
                    if let Some(mb) = limits.max_memory_mb {
                        if let Some(bytes) = usize::try_from(mb)
                            .ok()
                            .and_then(|m| m.checked_mul(1024 * 1024))
                        {
                            if bytes > 0 {
                                flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                                info.job_memory_limit = bytes;
                            }
                        }
                    }
                    if let Some(pids) = limits.max_pids {
                        if pids > 0 {
                            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                            info.basic_limit_information.active_process_limit = pids;
                        }
                    }
                }
                info.basic_limit_information.limit_flags = flags;

                let ok = SetInformationJobObject(
                    job,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                    &info as *const _ as *const c_void,
                    size_of::<JobObjectExtendedLimitInformation>() as u32,
                );
                if ok == 0 || AssignProcessToJobObject(job, raw_proc) == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(Self { handle: job })
            }
        }

        /// Termina inmediatamente todo el árbol de procesos asociado a este Job Object.
        pub fn terminate(&mut self) {
            if self.handle != 0 {
                unsafe {
                    TerminateJobObject(self.handle, 1);
                    CloseHandle(self.handle);
                }
                self.handle = 0;
            }
        }
    }

    impl Drop for ProcessJob {
        fn drop(&mut self) {
            if self.handle != 0 {
                unsafe {
                    CloseHandle(self.handle);
                }
                self.handle = 0;
            }
        }
    }
}

#[cfg(not(windows))]
mod unix_job {
    use std::process::Child;

    use super::ProcessLimits;

    const SIGKILL: i32 = 9;

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    /// Handle RAII a un grupo de procesos POSIX (`PGID`); al soltar o terminar mata `-pgid`.
    pub struct ProcessJob {
        pgid: u32,
        terminated: bool,
    }

    impl ProcessJob {
        pub fn assign(child: &Child, _limits: ProcessLimits) -> Option<Self> {
            let pid = child.id();
            if pid == 0 {
                return None;
            }
            Some(Self {
                pgid: pid,
                terminated: false,
            })
        }

        pub fn terminate(&mut self) {
            if !self.terminated {
                self.terminated = true;
                if let Ok(pgid) = i32::try_from(self.pgid) {
                    if pgid > 0 {
                        unsafe {
                            kill(-pgid, SIGKILL);
                        }
                    }
                }
            }
        }
    }

    impl Drop for ProcessJob {
        fn drop(&mut self) {
            self.terminate();
        }
    }
}

#[cfg(windows)]
pub use win::ProcessJob;

#[cfg(not(windows))]
pub use unix_job::ProcessJob;

#[cfg(test)]
mod tests {
    use super::resolve_process_binary_in;

    /// `SDD-10` Fase 2: layout de npm, donde `mycli` (script POSIX `#!/bin/sh`, sin
    /// extensión) y `mycli.cmd` (wrapper real) conviven en el mismo directorio. El nombre
    /// debe resolverse:
    /// - **Windows**: a `mycli.cmd` vía `PATHEXT`, con `is_shell_wrapper == true`. El
    ///   fichero sin extensión no es un Win32 ejecutable (spawn con os error 193) y daría
    ///   un falso negativo para el bloqueo de `PromptTransport::Arg`.
    /// - **Unix**: a `mycli` tal cual (lo decide el bit de ejecución; `.cmd` no aplica).
    #[test]
    fn resolve_process_binary_in_resolves_npm_style_extensionless_via_pathext_on_windows() {
        let dir = std::env::temp_dir().join(format!(
            "onyx_resolve_pathext_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let posix = dir.join("mycli");
        let wrapper = dir.join("mycli.cmd");
        std::fs::write(&posix, "#!/bin/sh\necho posix\n").unwrap();
        std::fs::write(&wrapper, "@echo off\r\necho wrapper\r\n").unwrap();
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&posix, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // 1) Nombre en `PATH` explícito (hermético, sin mutar el entorno del proceso).
        let r = resolve_process_binary_in("mycli", Some(dir.as_os_str()))
            .expect("debe resolver `mycli`");
        #[cfg(windows)]
        {
            assert_eq!(
                r.path.canonicalize().unwrap(),
                wrapper.canonicalize().unwrap(),
                "sin extensión debe resolver al wrapper `mycli.cmd` vía PATHEXT, no al script POSIX"
            );
            assert!(
                r.is_shell_wrapper,
                "el wrapper `.cmd` debe marcar `is_shell_wrapper`"
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                r.path.canonicalize().unwrap(),
                posix.canonicalize().unwrap(),
                "en Unix el nombre exacto (bit de ejecución) gana"
            );
            assert!(!r.is_shell_wrapper);
        }

        // 2) Ruta directa con separador y sin extensión (p. ej. `C:\...\npm\mycli`): misma regla.
        let direct = dir.join("mycli").to_string_lossy().into_owned();
        let r2 = resolve_process_binary_in(&direct, None).expect("ruta directa debe resolver");
        #[cfg(windows)]
        {
            assert!(
                r2.path
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&wrapper.to_string_lossy()),
                "la ruta directa sin extensión debe resolverse vía PATHEXT: {:?}",
                r2.path
            );
            assert!(r2.is_shell_wrapper);
        }
        #[cfg(not(windows))]
        {
            assert!(!r2.is_shell_wrapper);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn unix_process_job_terminate_kills_shell_and_grandchild_promptly() {
        use std::io::Read;
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        use super::{ProcessJob, ProcessLimits, configure_process_isolation};

        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 20"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_isolation(&mut cmd, false);

        let mut child = cmd.spawn().expect("sh -c sleep 20 must spawn");
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");
        let mut job = ProcessJob::assign(&child, ProcessLimits::default())
            .expect("ProcessJob::assign must succeed on Unix");

        thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        job.terminate();
        let _ = child.wait();
        let mut out = String::new();
        let mut err = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = stderr.read_to_string(&mut err);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(3),
            "ProcessJob::terminate must kill shell and grandchild promptly instead of waiting 20s (took {elapsed:?})"
        );
    }
}
