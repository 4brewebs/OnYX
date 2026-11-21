//! Gestión del almacenamiento centralizado de sesiones en `<workspace>/.onyx/vault/`.
//!
//! Reglas de resolución (100% retrocompatibles con rutas explícitas):
//! - Rutas explícitas (absolutas o con separador `/` / `\` o `.` / `..`) se respetan tal cual.
//! - Nombres cortos de `--snapshot-dir <nombre>` se resuelven en `<workspace>/.onyx/vault/<nombre>/`.
//! - Si hay directorio de snapshots activo y `--record <archivo>` es un nombre corto, el `.tr` se
//!   guarda dentro de ese mismo directorio (`<snapshot_dir>/<archivo>`).
//! - Si solo se pasa `--record <archivo>` (nombre corto sin `--snapshot-dir`), el `.tr` se guarda
//!   en `<workspace>/.onyx/vault/unassigned/<archivo>`.
//! - `--auto-save` (o `ONYX_AUTO_SAVE=1`) genera automáticamente un directorio con nombre
//!   descriptivo (`<YYYYMMDD-HHMMSS>_<modelo>[_<rama_o_goal>]`) bajo `.onyx/vault/` con
//!   `snapshots.onyxpack` y `<id>.tr` juntos, y crea `.onyx/vault/.gitignore` (`*`) para que
//!   Git y los checkpoints de `/undo` nunca absorban artefactos del vault.

use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub const VAULT_UNASSIGNED_DIR: &str = "unassigned";

/// Devuelve `<workspace>/.onyx/vault`.
pub fn vault_root(workspace: &Path) -> PathBuf {
    workspace.join(".onyx").join("vault")
}

/// Devuelve `<workspace>/.onyx/vault/unassigned`.
pub fn vault_unassigned_dir(workspace: &Path) -> PathBuf {
    vault_root(workspace).join(VAULT_UNASSIGNED_DIR)
}

/// Crea `<workspace>/.onyx/vault/.gitignore` con `*` si aún no existe,
/// evitando que `git status` o los checkpoints temporales de `/undo` incluyan el vault.
pub fn ensure_vault_gitignore(workspace: &Path) -> io::Result<()> {
    let root = vault_root(workspace);
    fs::create_dir_all(&root)?;
    let gi = root.join(".gitignore");
    if !gi.exists() {
        fs::write(&gi, "# Managed automatically by openOnyx vault\n*\n")?;
    }
    Ok(())
}

/// Comprueba si `ONYX_AUTO_SAVE` está habilitado por entorno (`1`, `true`, `yes`, `on`).
pub fn auto_save_from_env() -> bool {
    env::var("ONYX_AUTO_SAVE")
        .ok()
        .is_some_and(|v| parse_env_bool(&v))
}

fn parse_env_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Determina si el usuario pasó una ruta explícita (absoluta, con separadores `/` o `\`,
/// o relativa `.`/`..`) en lugar de un nombre corto de sesión/archivo.
pub fn is_explicit_path(path: &Path) -> bool {
    if path.is_absolute() {
        return true;
    }
    let raw = path.to_string_lossy();
    if raw.contains('/') || raw.contains('\\') {
        return true;
    }
    matches!(
        path.components().next(),
        Some(Component::CurDir | Component::ParentDir | Component::Prefix(_) | Component::RootDir)
    )
}

/// Resultado de resolver rutas de escritura de snapshots y transcripción para una sesión.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedWriteStorage {
    pub snapshot_dir: Option<PathBuf>,
    pub save_transcript: Option<PathBuf>,
    pub session_label: Option<String>,
    pub uses_vault: bool,
}

/// Resuelve `snapshot_dir` y `save_transcript` aplicando las reglas de `.onyx/vault/` y `--auto-save`.
#[allow(clippy::too_many_arguments)]
pub fn resolve_write_storage(
    workspace: &Path,
    raw_snapshot_dir: Option<&Path>,
    raw_save_transcript: Option<&Path>,
    resumed_snapshot_dir: Option<&Path>,
    auto_save: bool,
    model: Option<&str>,
    goal_hint: Option<&str>,
    now: SystemTime,
) -> ResolvedWriteStorage {
    let vault = vault_root(workspace);

    // 1. Resolver snapshot_dir efectivo.
    let resolved_snapshot_dir = if let Some(raw_dir) = raw_snapshot_dir {
        if is_explicit_path(raw_dir) {
            Some(raw_dir.to_path_buf())
        } else {
            Some(vault.join(raw_dir))
        }
    } else if let Some(resumed) = resumed_snapshot_dir {
        Some(resumed.to_path_buf())
    } else if auto_save {
        let id = generate_auto_session_id(workspace, model, goal_hint, now);
        Some(vault.join(id))
    } else {
        None
    };

    // 2. Resolver save_transcript efectivo.
    let resolved_transcript = if let Some(raw_tr) = raw_save_transcript {
        if is_explicit_path(raw_tr) {
            Some(raw_tr.to_path_buf())
        } else if let Some(ref snap_dir) = resolved_snapshot_dir {
            // Si hay directorio de snapshots, el .tr corto entra en ese mismo directorio.
            Some(snap_dir.join(raw_tr))
        } else {
            // Sin directorio de snapshots -> .onyx/vault/unassigned/<archivo>.
            Some(vault_unassigned_dir(workspace).join(raw_tr))
        }
    } else if auto_save {
        resolved_snapshot_dir.as_ref().map(|snap_dir| {
            let base_name = snap_dir
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("session");
            let candidate = snap_dir.join(format!("{base_name}.tr"));
            if candidate.exists() {
                // Al continuar/reanudar con --auto-save, evita sobrescribir una transcripción previa.
                let ts = utc_timestamp_compact(now);
                unique_transcript_path(snap_dir, &format!("{base_name}_{ts}"))
            } else {
                candidate
            }
        })
    } else {
        None
    };

    let uses_vault = resolved_snapshot_dir
        .as_deref()
        .is_some_and(|p| p.starts_with(&vault))
        || resolved_transcript
            .as_deref()
            .is_some_and(|p| p.starts_with(&vault));

    let session_label = resolved_snapshot_dir
        .as_deref()
        .and_then(|p| session_display_label(workspace, p));

    ResolvedWriteStorage {
        snapshot_dir: resolved_snapshot_dir,
        save_transcript: resolved_transcript,
        session_label,
        uses_vault,
    }
}

/// Resuelve `--fork-dir <dir>` colocando nombres cortos dentro de `.onyx/vault/`.
pub fn resolve_fork_dir(workspace: &Path, raw: &Path) -> PathBuf {
    if is_explicit_path(raw) {
        raw.to_path_buf()
    } else {
        vault_root(workspace).join(raw)
    }
}

/// Resuelve un directorio de snapshots para lectura (`--resume`, `inspect`, `rewind`, `doctor`).
/// Si `raw` ya existe en disco o es ruta explícita se usa tal cual; si es nombre corto se busca
/// en `<workspace>/.onyx/vault/<raw>`.
pub fn resolve_read_snapshot_dir(workspace: &Path, raw: &Path) -> PathBuf {
    if raw.exists() || is_explicit_path(raw) {
        return raw.to_path_buf();
    }
    let in_vault = vault_root(workspace).join(raw);
    if in_vault.exists() {
        return in_vault;
    }
    raw.to_path_buf()
}

/// Resuelve una ruta de transcripción para lectura (`replay`, `rewind --transcript`, `doctor`).
/// Si `raw` ya existe o es ruta explícita se devuelve tal cual; si es nombre corto se busca en
/// `.onyx/vault/unassigned/<raw>` y después en las carpetas de sesión de `.onyx/vault/*/<raw>`.
pub fn resolve_read_transcript(workspace: &Path, raw: &Path) -> PathBuf {
    if raw.exists() || is_explicit_path(raw) {
        return raw.to_path_buf();
    }
    let in_unassigned = vault_unassigned_dir(workspace).join(raw);
    if in_unassigned.is_file() {
        return in_unassigned;
    }
    let vault = vault_root(workspace);
    if let Ok(entries) = fs::read_dir(&vault) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let candidate = dir.join(raw);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    raw.to_path_buf()
}

/// Extrae una etiqueta compacta legible de un directorio de snapshots para mostrar en TUI/CLI.
pub fn session_display_label(workspace: &Path, snapshot_dir: &Path) -> Option<String> {
    let vault = vault_root(workspace);
    if let Ok(rel) = snapshot_dir.strip_prefix(&vault) {
        let s = rel.to_string_lossy().replace('\\', "/");
        if !s.is_empty() {
            return Some(s);
        }
    }
    snapshot_dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Genera un identificador único de sesión legible para `--auto-save`:
/// `<YYYYMMDD-HHMMSS>_<modelo>[_<slug_goal_o_rama>]`.
pub fn generate_auto_session_id(
    workspace: &Path,
    model: Option<&str>,
    goal_hint: Option<&str>,
    now: SystemTime,
) -> String {
    let ts = utc_timestamp_compact(now);
    let model_slug = model
        .map(|m| {
            let tail = m.rsplit('/').next().unwrap_or(m);
            sanitize_slug(tail, 24)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string());

    let context_slug = goal_hint
        .map(|g| sanitize_slug(g, 20))
        .filter(|s| !s.is_empty())
        .or_else(|| detect_git_branch(workspace).map(|b| sanitize_slug(&b, 20)))
        .filter(|s| !s.is_empty());

    let base = match context_slug {
        Some(ctx) => format!("{ts}_{model_slug}_{ctx}"),
        None => format!("{ts}_{model_slug}"),
    };

    let vault = vault_root(workspace);
    if !vault.join(&base).exists() {
        return base;
    }
    for n in 2..=10_000u32 {
        let cand = format!("{base}-{n}");
        if !vault.join(&cand).exists() {
            return cand;
        }
    }
    format!("{base}-overflow")
}

fn unique_transcript_path(dir: &Path, stem: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.tr"));
    if !first.exists() {
        return first;
    }
    for n in 2..=10_000u32 {
        let cand = dir.join(format!("{stem}-{n}.tr"));
        if !cand.exists() {
            return cand;
        }
    }
    dir.join(format!("{stem}-overflow.tr"))
}

/// Formatea un `SystemTime` en UTC compacto `YYYYMMDD-HHMMSS` sin dependencias externas
/// (algoritmo civil-from-days de Howard Hinnant).
pub fn utc_timestamp_compact(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = y + i64::from(month <= 2);

    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Convierte texto arbitrario en un slug seguro para nombres de directorio/archivo en Windows y Unix.
pub fn sanitize_slug(raw: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in raw.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= max_len {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

fn detect_git_branch(workspace: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Resumen de auditoría del vault para `openonyx doctor`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VaultAudit {
    pub exists: bool,
    pub gitignore_present: bool,
    pub session_dirs: usize,
    pub unassigned_transcripts: usize,
}

pub fn audit_vault(workspace: &Path) -> VaultAudit {
    let root = vault_root(workspace);
    if !root.is_dir() {
        return VaultAudit::default();
    }
    let gitignore_present = root.join(".gitignore").is_file();
    let mut session_dirs = 0usize;
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                if name != VAULT_UNASSIGNED_DIR {
                    session_dirs += 1;
                }
            }
        }
    }
    let mut unassigned_transcripts = 0usize;
    let unassigned = vault_unassigned_dir(workspace);
    if let Ok(entries) = fs::read_dir(&unassigned) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("tr") {
                unassigned_transcripts += 1;
            }
        }
    }
    VaultAudit {
        exists: true,
        gitignore_present,
        session_dirs,
        unassigned_transcripts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_ws(tag: &str) -> PathBuf {
        let mut dir = env::temp_dir();
        dir.push(format!("openonyx-vault-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn utc_timestamp_matches_known_epoch_dates() {
        assert_eq!(utc_timestamp_compact(UNIX_EPOCH), "19700101-000000");
        // 2026-11-21 10:30:45 UTC = 1_795_257_045? Let's verify 2024-02-29 12:34:56 UTC (leap day!)
        // 2024-02-29 00:00:00 UTC = 19_782 days * 86_400 = 1_709_164_800
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_164_800 + 12 * 3600 + 34 * 60 + 56);
        assert_eq!(utc_timestamp_compact(leap), "20240229-123456");
    }

    #[test]
    fn write_storage_routes_short_names_into_vault_and_preserves_explicit_paths() {
        let ws = temp_ws("routing");
        let fixed_now = UNIX_EPOCH + Duration::from_secs(1_709_164_800);

        // 1. Snapshot dir + transcript cortos -> ambos en .onyx/vault/<dir>/
        let r1 = resolve_write_storage(
            &ws,
            Some(Path::new("feat-auth")),
            Some(Path::new("chat.tr")),
            None,
            false,
            Some("gpt-4o"),
            None,
            fixed_now,
        );
        assert_eq!(
            r1.snapshot_dir.as_deref(),
            Some(ws.join(".onyx").join("vault").join("feat-auth").as_path())
        );
        assert_eq!(
            r1.save_transcript.as_deref(),
            Some(
                ws.join(".onyx")
                    .join("vault")
                    .join("feat-auth")
                    .join("chat.tr")
                    .as_path()
            )
        );
        assert!(r1.uses_vault);

        // 2. Solo transcript corto (sin snapshot-dir) -> .onyx/vault/unassigned/<file>
        let r2 = resolve_write_storage(
            &ws,
            None,
            Some(Path::new("solo.tr")),
            None,
            false,
            Some("gpt-4o"),
            None,
            fixed_now,
        );
        assert_eq!(r2.snapshot_dir, None);
        assert_eq!(
            r2.save_transcript.as_deref(),
            Some(
                ws.join(".onyx")
                    .join("vault")
                    .join("unassigned")
                    .join("solo.tr")
                    .as_path()
            )
        );
        assert!(r2.uses_vault);

        // 3. Rutas explícitas se respetan tal cual (retrocompatibilidad)
        let explicit_dir = ws.join("custom_snaps");
        let explicit_tr = ws.join("custom.tr");
        let r3 = resolve_write_storage(
            &ws,
            Some(&explicit_dir),
            Some(&explicit_tr),
            None,
            false,
            None,
            None,
            fixed_now,
        );
        assert_eq!(r3.snapshot_dir.as_deref(), Some(explicit_dir.as_path()));
        assert_eq!(r3.save_transcript.as_deref(), Some(explicit_tr.as_path()));
        assert!(!r3.uses_vault);

        // 4. --auto-save genera directorio y .tr descriptivos juntos dentro de .onyx/vault/
        let r4 = resolve_write_storage(
            &ws,
            None,
            None,
            None,
            true,
            Some("Qwen/Qwen3-Coder:30b"),
            Some("Fix parser UTF-8 bugs"),
            fixed_now,
        );
        let snap_dir = r4.snapshot_dir.expect("auto-save creates snapshot_dir");
        let tr_file = r4.save_transcript.expect("auto-save creates transcript");
        assert!(snap_dir.starts_with(vault_root(&ws)));
        assert_eq!(tr_file.parent(), Some(snap_dir.as_path()));
        let label = r4.session_label.expect("session label");
        assert!(label.starts_with("20240229-000000_qwen3-coder-30b_fix-parser"));
        assert!(tr_file.to_string_lossy().ends_with(&format!("{label}.tr")));
        assert!(r4.uses_vault);

        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn read_resolution_finds_vault_sessions_and_unassigned_transcripts() {
        let ws = temp_ws("read");
        ensure_vault_gitignore(&ws).unwrap();
        assert!(vault_root(&ws).join(".gitignore").is_file());

        let sess = vault_root(&ws).join("mi-sesion");
        fs::create_dir_all(&sess).unwrap();
        let sess_tr = sess.join("mi-sesion.tr");
        fs::write(&sess_tr, b"ONYXTR2").unwrap();

        let unassigned = vault_unassigned_dir(&ws);
        fs::create_dir_all(&unassigned).unwrap();
        let loose_tr = unassigned.join("suelta.tr");
        fs::write(&loose_tr, b"ONYXTR2").unwrap();

        assert_eq!(resolve_read_snapshot_dir(&ws, Path::new("mi-sesion")), sess);
        assert_eq!(
            resolve_read_transcript(&ws, Path::new("suelta.tr")),
            loose_tr
        );
        assert_eq!(
            resolve_read_transcript(&ws, Path::new("mi-sesion.tr")),
            sess_tr
        );

        let audit = audit_vault(&ws);
        assert!(audit.exists);
        assert!(audit.gitignore_present);
        assert_eq!(audit.session_dirs, 1);
        assert_eq!(audit.unassigned_transcripts, 1);

        let _ = fs::remove_dir_all(&ws);
    }
}
