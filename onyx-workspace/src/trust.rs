//! Workspace Trust (`~/.onyx/trusted_workspaces.toml`, hash `blake3`).
//!
//! Mitiga ataques de ejecución remota en 1 clic (RCE vía *Indirect Prompt Injection* +
//! `.onyx/config.toml` / `sprout.toml` maliciosos en repositorios clonados o PRs externos):
//! - Si un repositorio no tiene ni `.onyx/config.toml` ni `sprout.toml`, el estado es
//!   [`WorkspaceTrustStatus::NoProjectConfig`] y no añade fricción.
//! - Si existen y el workspace no ha sido confiado explícitamente por el usuario (o el hash
//!   `blake3` de cualquiera de los dos archivos cambió desde la última aprobación):
//!   - `.onyx/config.toml` del proyecto **solo puede restringir** (`deny`/`ask`, sandbox/impact
//!     más estrictos), **nunca ampliar** `allow`, `env_passthrough`, `allow_read_paths` ni relajar
//!     `[sandbox]` o `[impact]`.
//!   - `sprout.toml` bloquea la ejecución de servicios (`service_start`, `service_exec`, `stop_cmd`)
//!     hasta que el usuario apruebe el hash actual con `openonyx trust` o `/trust`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::policy::strip_toml_comment_pub;
use crate::sandbox::clean_process_path;
use crate::sprout::SPROUT_CONFIG_FILE;

/// Variable de entorno opcional para redirigir el almacén de confianza (tests herméticos / CI).
pub const ONYX_TRUST_FILE_ENV: &str = "ONYX_TRUST_FILE";

/// Registro persistido en `~/.onyx/trusted_workspaces.toml` para un workspace aprobado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTrustRecord {
    pub root: String,
    pub config_blake3: Option<String>,
    pub sprout_blake3: Option<String>,
    pub trusted_at: u64,
}

/// Estado de confianza de un workspace frente a `~/.onyx/trusted_workspaces.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceTrustStatus {
    /// El workspace no contiene `.onyx/config.toml` ni `sprout.toml`.
    NoProjectConfig,
    /// Los hashes `blake3` actuales coinciden con la aprobación persistida del usuario.
    Trusted,
    /// Existe configuración local pero el workspace nunca ha sido aprobado.
    Untrusted { has_config: bool, has_sprout: bool },
    /// El workspace fue aprobado antes, pero `.onyx/config.toml` o `sprout.toml` cambió.
    Modified {
        config_changed: bool,
        sprout_changed: bool,
    },
}

/// Informe completo de confianza evaluado para un workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTrustReport {
    pub status: WorkspaceTrustStatus,
    pub config_blake3: Option<String>,
    pub sprout_blake3: Option<String>,
    /// `true` si las reglas expansivas de `.onyx/config.toml` pueden aplicarse (o no existe).
    pub config_trusted: bool,
    /// `true` si `sprout.toml` está aprobado para ejecutar procesos/contenedores (o no existe).
    pub sprout_trusted: bool,
}

impl Default for WorkspaceTrustReport {
    fn default() -> Self {
        Self::no_project_config()
    }
}

impl WorkspaceTrustReport {
    pub fn no_project_config() -> Self {
        Self {
            status: WorkspaceTrustStatus::NoProjectConfig,
            config_blake3: None,
            sprout_blake3: None,
            config_trusted: true,
            sprout_trusted: true,
        }
    }

    /// Crea un informe explícitamente confiado en memoria (p. ej. `--trust-workspace` o tests).
    pub fn trusted_override(workspace_root: &Path) -> Self {
        let config_blake3 = hash_file_blake3(&workspace_root.join(".onyx").join("config.toml"))
            .ok()
            .flatten();
        let sprout_blake3 = hash_file_blake3(&workspace_root.join(SPROUT_CONFIG_FILE))
            .ok()
            .flatten();
        let status = if config_blake3.is_none() && sprout_blake3.is_none() {
            WorkspaceTrustStatus::NoProjectConfig
        } else {
            WorkspaceTrustStatus::Trusted
        };
        Self {
            status,
            config_blake3,
            sprout_blake3,
            config_trusted: true,
            sprout_trusted: true,
        }
    }

    /// `true` cuando no hay restricciones pendientes por falta de confianza.
    pub fn is_fully_trusted(&self) -> bool {
        self.config_trusted && self.sprout_trusted
    }

    /// Etiqueta corta legible del estado para `doctor` y banners TUI.
    pub fn status_label(&self) -> &'static str {
        match self.status {
            WorkspaceTrustStatus::NoProjectConfig => "no-project-config",
            WorkspaceTrustStatus::Trusted => "trusted",
            WorkspaceTrustStatus::Untrusted { .. } => "untrusted",
            WorkspaceTrustStatus::Modified { .. } => "modified",
        }
    }
}

/// Ruta por defecto del almacén global de confianza (`~/.onyx/trusted_workspaces.toml`),
/// con override opcional mediante `ONYX_TRUST_FILE`.
pub fn default_trust_store_path() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os(ONYX_TRUST_FILE_ENV) {
        let trimmed = custom.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| {
            PathBuf::from(h)
                .join(".onyx")
                .join("trusted_workspaces.toml")
        })
}

/// Normaliza la ruta raíz de un workspace como clave estable cross-platform.
pub fn normalize_workspace_key(root: &Path) -> String {
    let resolved = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let cleaned = clean_process_path(&resolved);
    let mut s = cleaned.to_string_lossy().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        // Preservar raíces tipo "C:/" en Windows.
        if s.len() == 3 && s.as_bytes()[1] == b':' {
            break;
        }
        s.pop();
    }
    #[cfg(windows)]
    {
        s.make_ascii_lowercase();
    }
    s
}

/// Calcula el hash `blake3` hexadecimal (64 chars) de un archivo si existe en disco.
pub fn hash_file_blake3(path: &Path) -> Result<Option<String>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|e| format!("failed to read `{}`: {e}", path.display()))?;
    Ok(Some(blake3::hash(&bytes).to_hex().to_string()))
}

/// Evalúa el estado de confianza del workspace frente al almacén `store_path` (o el global).
pub fn evaluate_workspace_trust(
    workspace_root: &Path,
    store_path: Option<&Path>,
) -> WorkspaceTrustReport {
    let config_path = workspace_root.join(".onyx").join("config.toml");
    let sprout_path = workspace_root.join(SPROUT_CONFIG_FILE);

    let config_blake3 = hash_file_blake3(&config_path).ok().flatten();
    let sprout_blake3 = hash_file_blake3(&sprout_path).ok().flatten();

    if config_blake3.is_none() && sprout_blake3.is_none() {
        return WorkspaceTrustReport::no_project_config();
    }

    let key = normalize_workspace_key(workspace_root);
    let store_buf = store_path
        .map(Path::to_path_buf)
        .or_else(default_trust_store_path);
    let records = store_buf
        .as_deref()
        .and_then(|p| load_trust_records(p).ok())
        .unwrap_or_default();

    let Some(existing) = records.iter().find(|r| r.root == key) else {
        return WorkspaceTrustReport {
            status: WorkspaceTrustStatus::Untrusted {
                has_config: config_blake3.is_some(),
                has_sprout: sprout_blake3.is_some(),
            },
            config_trusted: config_blake3.is_none(),
            sprout_trusted: sprout_blake3.is_none(),
            config_blake3,
            sprout_blake3,
        };
    };

    let config_matches = existing.config_blake3 == config_blake3;
    let sprout_matches = existing.sprout_blake3 == sprout_blake3;
    if config_matches && sprout_matches {
        WorkspaceTrustReport {
            status: WorkspaceTrustStatus::Trusted,
            config_blake3,
            sprout_blake3,
            config_trusted: true,
            sprout_trusted: true,
        }
    } else {
        WorkspaceTrustReport {
            status: WorkspaceTrustStatus::Modified {
                config_changed: !config_matches,
                sprout_changed: !sprout_matches,
            },
            config_trusted: config_blake3.is_none() || config_matches,
            sprout_trusted: sprout_blake3.is_none() || sprout_matches,
            config_blake3,
            sprout_blake3,
        }
    }
}

/// Aprueba explícitamente el workspace actual grabando los hashes `blake3` de
/// `.onyx/config.toml` y `sprout.toml` en el almacén global (escritura atómica).
pub fn trust_workspace(
    workspace_root: &Path,
    store_path: Option<&Path>,
) -> Result<WorkspaceTrustReport, String> {
    let store = store_path
        .map(Path::to_path_buf)
        .or_else(default_trust_store_path)
        .ok_or_else(|| {
            "cannot determine user home directory for trusted_workspaces.toml".to_string()
        })?;

    let config_blake3 = hash_file_blake3(&workspace_root.join(".onyx").join("config.toml"))?;
    let sprout_blake3 = hash_file_blake3(&workspace_root.join(SPROUT_CONFIG_FILE))?;
    let key = normalize_workspace_key(workspace_root);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut records = load_trust_records(&store)?;
    records.retain(|r| r.root != key);
    records.push(WorkspaceTrustRecord {
        root: key,
        config_blake3: config_blake3.clone(),
        sprout_blake3: sprout_blake3.clone(),
        trusted_at: now,
    });
    records.sort_by(|a, b| a.root.cmp(&b.root));
    write_trust_records(&store, &records)?;

    let status = if config_blake3.is_none() && sprout_blake3.is_none() {
        WorkspaceTrustStatus::NoProjectConfig
    } else {
        WorkspaceTrustStatus::Trusted
    };
    Ok(WorkspaceTrustReport {
        status,
        config_blake3,
        sprout_blake3,
        config_trusted: true,
        sprout_trusted: true,
    })
}

/// Revoca la confianza previamente concedida a `workspace_root`. Devuelve `true` si existía.
pub fn revoke_workspace_trust(
    workspace_root: &Path,
    store_path: Option<&Path>,
) -> Result<bool, String> {
    let store = store_path
        .map(Path::to_path_buf)
        .or_else(default_trust_store_path)
        .ok_or_else(|| {
            "cannot determine user home directory for trusted_workspaces.toml".to_string()
        })?;
    if !store.is_file() {
        return Ok(false);
    }
    let key = normalize_workspace_key(workspace_root);
    let mut records = load_trust_records(&store)?;
    let before = records.len();
    records.retain(|r| r.root != key);
    if records.len() == before {
        return Ok(false);
    }
    write_trust_records(&store, &records)?;
    Ok(true)
}

/// Lista todos los workspaces confiados actualmente en el almacén.
pub fn list_trusted_workspaces(
    store_path: Option<&Path>,
) -> Result<Vec<WorkspaceTrustRecord>, String> {
    let Some(store) = store_path
        .map(Path::to_path_buf)
        .or_else(default_trust_store_path)
    else {
        return Ok(Vec::new());
    };
    load_trust_records(&store)
}

fn load_trust_records(path: &Path) -> Result<Vec<WorkspaceTrustRecord>, String> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("failed to read `{}`: {e}", path.display()))?;
    parse_trust_toml(&raw)
}

fn parse_trust_toml(input: &str) -> Result<Vec<WorkspaceTrustRecord>, String> {
    let text = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut records: Vec<WorkspaceTrustRecord> = Vec::new();
    let mut current: Option<WorkspaceTrustRecord> = None;

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let stripped = strip_toml_comment_pub(raw_line).trim();
        if stripped.is_empty() {
            continue;
        }
        if stripped == "[[workspace]]" {
            if let Some(rec) = current.take() {
                if !rec.root.is_empty() {
                    records.push(rec);
                }
            }
            current = Some(WorkspaceTrustRecord {
                root: String::new(),
                config_blake3: None,
                sprout_blake3: None,
                trusted_at: 0,
            });
            continue;
        }
        if stripped.starts_with('[') && stripped.ends_with(']') {
            if let Some(rec) = current.take() {
                if !rec.root.is_empty() {
                    records.push(rec);
                }
            }
            continue;
        }
        let Some(rec) = current.as_mut() else {
            continue;
        };
        let Some((raw_key, raw_val)) = stripped.split_once('=') else {
            return Err(format!(
                "trusted_workspaces.toml line {line_no}: expected `key = value`"
            ));
        };
        let key = raw_key.trim();
        let val = raw_val.trim();
        match key {
            "root" => {
                rec.root = parse_quoted_toml_string(val)
                    .map_err(|e| format!("trusted_workspaces.toml line {line_no} (`root`): {e}"))?;
            }
            "config_blake3" => {
                let s = parse_quoted_toml_string(val).map_err(|e| {
                    format!("trusted_workspaces.toml line {line_no} (`config_blake3`): {e}")
                })?;
                rec.config_blake3 = normalize_hash_field(&s);
            }
            "sprout_blake3" => {
                let s = parse_quoted_toml_string(val).map_err(|e| {
                    format!("trusted_workspaces.toml line {line_no} (`sprout_blake3`): {e}")
                })?;
                rec.sprout_blake3 = normalize_hash_field(&s);
            }
            "trusted_at" => {
                rec.trusted_at = val.parse::<u64>().map_err(|_| {
                    format!("trusted_workspaces.toml line {line_no}: `trusted_at` must be u64")
                })?;
            }
            _ => {}
        }
    }

    if let Some(rec) = current.take() {
        if !rec.root.is_empty() {
            records.push(rec);
        }
    }
    Ok(records)
}

fn normalize_hash_field(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(trimmed.to_ascii_lowercase())
    }
}

fn write_trust_records(path: &Path, records: &[WorkspaceTrustRecord]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create trust directory `{}`: {e}",
                parent.display()
            )
        })?;
    }
    let mut out = String::from(
        "# Managed by openOnyx Workspace Trust (`openonyx trust`)\n\
         # Each entry pins blake3 hashes of `.onyx/config.toml` and `sprout.toml`.\n",
    );
    for rec in records {
        out.push_str("\n[[workspace]]\n");
        out.push_str(&format!("root = \"{}\"\n", escape_toml_string(&rec.root)));
        out.push_str(&format!(
            "config_blake3 = \"{}\"\n",
            rec.config_blake3.as_deref().unwrap_or("none")
        ));
        out.push_str(&format!(
            "sprout_blake3 = \"{}\"\n",
            rec.sprout_blake3.as_deref().unwrap_or("none")
        ));
        out.push_str(&format!("trusted_at = {}\n", rec.trusted_at));
    }

    let tmp_path = path.with_extension("toml.tmp");
    fs::write(&tmp_path, out.as_bytes()).map_err(|e| {
        format!(
            "failed to write temporary trust file `{}`: {e}",
            tmp_path.display()
        )
    })?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp_path, path)
        .map_err(|e| format!("failed to persist trust file `{}`: {e}", path.display()))
}

fn escape_toml_string(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_quoted_toml_string(raw: &str) -> Result<String, String> {
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
