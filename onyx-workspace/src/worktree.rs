//! Aislamiento en disco mediante Git worktrees y reconciliación 3-way sin mutar `HEAD` (`SDD-09`).
//!
//! Implementación pura sobre `std::process::Command` (**0 `tokio`**, **0 crates externos nuevos**):
//! - Captura el *working tree* real de `root` (incluyendo cambios sin commitear) mediante
//!   [`capture_git_tree_checkpoint`] + `git commit-tree` efímero sin tocar `HEAD` ni `.git/index`.
//! - Resuelve rutas hermanas cortas (aptas para Windows) con override `ONYX_WT_DIR` y fallback a `temp_dir`.
//! - Reconcilia cambios concurrentes en `merge_tick` con `git merge-tree --write-tree --merge-base`
//!   (requiere Git `>= 2.38.0`), aplicando únicamente el delta `<root_now_commit>..<merged_tree>`
//!   sin mover `HEAD` y bloqueando cualquier escritura al Control Plane (`.onyx/config.toml`, `.env*`, `.git/**`).

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::policy::is_control_plane_rel_path;
use crate::sandbox::NetworkPolicy;
use crate::state::{capture_git_tree_checkpoint, capture_git_tree_checkpoint_excluding};

/// Variable de entorno opcional para sobreescribir el directorio base de *worktrees* temporales.
pub const ONYX_WT_DIR_ENV: &str = "ONYX_WT_DIR";

/// Versión mínima de Git requerida para `git merge-tree --write-tree --merge-base`.
pub const MIN_GIT_VERSION: (u32, u32, u32) = (2, 38, 0);

const SUBAGENT_BRANCH_PREFIX: &str = "onyx/sub/";

/// Política de red aplicable al entorno de ejecución de un subagente (`SDD-09` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentNetworkPolicy {
    /// Subagente nativo: los subprocesos de herramientas corren offline (el host hace las llamadas LLM).
    NativeOffline,
    /// Harness CLI externo (`opencode`, `claude`, etc.) con lista explícita de hosts permitidos (`SDD-10` §2.7).
    ExternalHarnessAllowlist(Vec<String>),
}

impl SubagentNetworkPolicy {
    /// `NetworkPolicy` de sandbox derivada para los subprocesos del subagente (`SDD-10` Fase 2):
    /// - `NativeOffline` → `Offline` (los subprocesos de herramientas corren sin red; el host hace las LLM).
    /// - `ExternalHarnessAllowlist` → **no-`Offline`** (el harness necesita llegar a su API):
    ///   `Unrestricted` con allowlist declarada, `LocalhostOnly` con allowlist vacía.
    ///   En ambos casos `apply_sanitized_env*` no inyecta hints `*_OFFLINE`/`NO_PROXY`;
    ///   la allowlist de hosts es declarativa (el enforce de SO solo existe en contenedor).
    pub fn derived_network_policy(&self) -> NetworkPolicy {
        match self {
            Self::NativeOffline => NetworkPolicy::Offline,
            Self::ExternalHarnessAllowlist(allowlist) => {
                if allowlist.is_empty() {
                    NetworkPolicy::LocalhostOnly
                } else {
                    NetworkPolicy::Unrestricted
                }
            }
        }
    }
}

/// Cómo se entrega el prompt al CLI externo sin inyección de shell (`SDD-10` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTransport {
    /// Recomendado y default: bytes del prompt al stdin del hijo (seguro en wrappers `.cmd`).
    Stdin,
    /// Un único elemento argv (rechazado en Windows si el binario resuelve a `.cmd`/`.bat`).
    Arg,
    /// Archivo temporal fuera del índice Git; pasa su ruta por `prompt_flag` o `{prompt_file}`.
    TempFile,
}

impl PromptTransport {
    /// Parsea el valor TOML (`"stdin" | "arg" | "temp_file"`).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "stdin" => Some(Self::Stdin),
            "arg" => Some(Self::Arg),
            "temp_file" => Some(Self::TempFile),
            _ => None,
        }
    }
}

/// Driver de harness CLI cargado desde `[[external_drivers]]` de `.onyx/config.toml`
/// (parser hand-rolled en [`crate::external_driver`], junto al de `[permissions]`; `SDD-10` §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalDriverConfig {
    /// Identificador único con charset `[a-z0-9_-]`, p. ej. `"opencode"`.
    pub id: String,
    /// Binario a resolver en `PATH`/`PATHEXT`, p. ej. `"opencode"`.
    pub bin: String,
    /// Argumentos fijos del CLI, p. ej. `["run", "--model", "qwen38-27b"]`.
    pub args: Vec<String>,
    /// Transporte del prompt (default `Stdin`).
    pub prompt_transport: PromptTransport,
    /// Flag opcional para `TempFile` (o placeholder `{prompt_file}` dentro de `args`).
    pub prompt_flag: Option<String>,
    /// Tope de ejecución del proceso externo.
    pub timeout: Duration,
    /// Whitelist de variables de entorno visibles para el hijo (incluye opt-in a `NODE_OPTIONS`).
    pub allowed_env: Vec<String>,
    /// `true` si el CLI necesita config/OAuth del `HOME` real (default `false`).
    pub inherit_home: bool,
    /// Gobernanza de red declarada por el driver (allowlist de hosts, `SDD-10` §2.7).
    pub network_policy: SubagentNetworkPolicy,
}

/// Versión semántica detectada del binario `git` instalado en el host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl GitVersion {
    pub const fn as_tuple(self) -> (u32, u32, u32) {
        (self.major, self.minor, self.patch)
    }

    pub const fn supports_merge_tree_write_tree(self) -> bool {
        let (maj, min, pat) = self.as_tuple();
        let (req_maj, req_min, req_pat) = MIN_GIT_VERSION;
        maj > req_maj || (maj == req_maj && (min > req_min || (min == req_min && pat >= req_pat)))
    }
}

impl std::fmt::Display for GitVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Contexto aislado en disco creado por [`WorktreeManager::create_worktree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHandle {
    pub job_id: String,
    pub spawn_tick: u64,
    pub worktree_path: PathBuf,
    pub branch_name: String,
    /// Commit efímero (`git commit-tree`) que captura el working tree exacto de `root` en `spawn_tick`.
    pub base_commit: String,
    /// Árbol Git (`tree_sha`) correspondiente a `base_commit`.
    pub base_tree: String,
}

/// Propuesta estructurada emitida por un subagente al terminar (`SDD-09` §3).
///
/// Reconcilia conocimiento por claves semánticas (`touched_files`), nunca por `ConceptId` numérico.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentProposal {
    pub job_id: String,
    /// Backend que ejecutó el job: `"native"` (motor en proceso) o el `id` de un harness CLI
    /// externo configurado (`SDD-10`).
    pub driver: String,
    pub spawn_tick: u64,
    pub branch_name: String,
    pub base_commit: String,
    /// Commit efímero capturando el working tree final del subagente (vacío si fue cancelado/fallido antes de capturar).
    pub sub_commit: String,
    pub ok: bool,
    pub summary: String,
    /// Rutas relativas tocadas por el subagente respecto a `base_commit`.
    pub touched_files: Vec<String>,
}

/// Entrada reportada por [`WorktreeManager::list_worktrees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head_sha: Option<String>,
    pub branch: Option<String>,
    pub is_onyx_subagent: bool,
}

/// Resultado de la reconciliación 3-way en disco entre `base_commit`, `root_now_commit` y `sub_commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeMergeOutcome {
    /// Sin cambios efectivos respecto a `base_commit` / `root_now_commit`.
    NoOp,
    /// Fusión 3-way limpia lista para aprobación + *Scent-Heated Approval* y aplicación sin mover `HEAD`.
    Clean {
        root_now_commit: String,
        root_now_tree: String,
        merged_tree: String,
        changed_files: Vec<String>,
        unified_diff: String,
    },
    /// Colisión real entre los cambios de `root` y los del subagente (disco de `root` intacto).
    Conflict {
        root_now_commit: String,
        conflicting_files: Vec<String>,
        details: String,
    },
}

/// Gestor autocontenido del ciclo de vida de *worktrees* Git aislados y merges 3-way (`SDD-09` Fase 1).
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    workspace_root: PathBuf,
    base_dir: PathBuf,
}

impl WorktreeManager {
    /// Crea un gestor para `workspace_root`, validando que sea un repositorio Git con Git `>= 2.38.0`.
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, String> {
        Self::with_base_dir_override(workspace_root, None)
    }

    /// Variante con override explícito de directorio base (útil en tests y configuraciones personalizadas).
    pub fn with_base_dir_override(
        workspace_root: impl AsRef<Path>,
        base_dir_override: Option<&Path>,
    ) -> Result<Self, String> {
        check_git_version()?;
        let canonical = fs::canonicalize(workspace_root.as_ref())
            .map_err(|e| format!("failed to resolve workspace root: {e}"))?;
        let clean_root = strip_verbatim_prefix(&canonical);

        let in_repo = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&clean_root)
            .output()
            .map_err(|e| format!("failed to run git rev-parse: {e}"))?;
        if !in_repo.status.success() || String::from_utf8_lossy(&in_repo.stdout).trim() != "true" {
            return Err(format!(
                "`{}` is not inside a Git work tree",
                clean_root.display()
            ));
        }

        let base_dir = resolve_worktree_base_dir(&clean_root, base_dir_override);
        Ok(Self {
            workspace_root: clean_root,
            base_dir,
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Ejecuta `git worktree prune` para limpiar metadatos de *worktrees* huérfanos tras cierres abruptos.
    pub fn prune_worktrees(&self) -> Result<(), String> {
        let out = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.workspace_root)
            .output()
            .map_err(|e| format!("failed to run git worktree prune: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git worktree prune failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Lista todos los *worktrees* registrados en el repositorio (`git worktree list --porcelain`).
    pub fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, String> {
        let out = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.workspace_root)
            .output()
            .map_err(|e| format!("failed to run git worktree list: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git worktree list failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(parse_worktree_porcelain(&String::from_utf8_lossy(
            &out.stdout,
        )))
    }

    /// Crea un *worktree* aislado capturando el estado real actual del *working tree* de `root`
    /// (incluyendo cambios sin commitear) en un commit efímero sin mutar `HEAD` ni `.git/index`.
    pub fn create_worktree(&self, job_id: &str, spawn_tick: u64) -> Result<WorktreeHandle, String> {
        validate_job_id(job_id)?;
        fs::create_dir_all(&self.base_dir).map_err(|e| {
            format!(
                "failed to create worktree base directory `{}`: {e}",
                self.base_dir.display()
            )
        })?;

        let worktree_path = strip_verbatim_prefix(&self.base_dir.join(job_id));
        if worktree_path.exists() {
            return Err(format!(
                "worktree target `{}` already exists",
                worktree_path.display()
            ));
        }

        let (base_commit, base_tree) = create_ephemeral_commit(
            &self.workspace_root,
            None,
            &format!("onyx-wt-base:{job_id}@t{spawn_tick}"),
        )?;

        let branch_name = format!("{SUBAGENT_BRANCH_PREFIX}{job_id}");
        let out = Command::new("git")
            .args(["worktree", "add", "-b", &branch_name])
            .arg(&worktree_path)
            .arg(&base_commit)
            .current_dir(&self.workspace_root)
            .output()
            .map_err(|e| format!("failed to run git worktree add: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        Ok(WorktreeHandle {
            job_id: job_id.to_string(),
            spawn_tick,
            worktree_path,
            branch_name,
            base_commit,
            base_tree,
        })
    }

    /// Captura el estado final del *working tree* del subagente en un commit efímero descendiente
    /// de `handle.base_commit`, listo para reconciliación 3-way.
    ///
    /// Excluye los artefactos operativos que OnYX mismo crea en el *worktree* (caché scent
    /// `.onyx/scent.idx*` y su `.gitignore` de higiene, al evaluar el impacto de una mutación
    /// con la feature `scent`): no son trabajo del hijo. El pathspec `scent*` es glob (no una
    /// ruta explícita): `git add` rechaza pathspecs que nombran rutas ya ignoradas por
    /// `.gitignore` (el propio `.onyx/.gitignore` ignora `scent.idx`). El resto de `.onyx/`
    /// **no** se excluye: p. ej. un `config.toml` tocado directamente en el *worktree* sigue
    /// capturándose y bloqueándose en `apply` por Control Plane.
    pub fn capture_subagent_commit(
        &self,
        handle: &WorktreeHandle,
    ) -> Result<(String, String), String> {
        create_ephemeral_commit_excluding(
            &handle.worktree_path,
            Some(&handle.base_commit),
            &format!("onyx-wt-sub:{}", handle.job_id),
            &[":(exclude).onyx/scent*", ":(exclude).onyx/.gitignore"],
        )
    }

    /// Devuelve la lista ordenada de rutas relativas modificadas entre `from_commit` y `to_commit`.
    pub fn changed_files_between(
        &self,
        from_commit: &str,
        to_commit: &str,
    ) -> Result<Vec<String>, String> {
        diff_name_only(&self.workspace_root, from_commit, to_commit)
    }

    /// Evalúa una fusión 3-way en memoria/objetos Git entre el estado actual (posiblemente sucio)
    /// de `root` (`root_now_commit`) y `sub_commit`, usando `handle.base_commit` como ancestro común.
    /// Nunca modifica el *working tree*, `HEAD` ni `.git/index` de `root`.
    pub fn preview_three_way_merge(
        &self,
        handle: &WorktreeHandle,
        sub_commit: &str,
    ) -> Result<WorktreeMergeOutcome, String> {
        let sub_tree = rev_parse_tree(&self.workspace_root, sub_commit)?;
        if sub_tree == handle.base_tree {
            return Ok(WorktreeMergeOutcome::NoOp);
        }

        let (root_now_commit, root_now_tree) = create_ephemeral_commit(
            &self.workspace_root,
            None,
            &format!("onyx-wt-root-now:{}", handle.job_id),
        )?;

        let out = Command::new("git")
            .args([
                "-c",
                "core.quotepath=false",
                "merge-tree",
                "--write-tree",
                &format!("--merge-base={}", handle.base_commit),
                &root_now_commit,
                sub_commit,
            ])
            .current_dir(&self.workspace_root)
            .output()
            .map_err(|e| format!("failed to run git merge-tree: {e}"))?;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        match out.status.code() {
            Some(0) => {
                let merged_tree = stdout
                    .lines()
                    .next()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "git merge-tree returned empty tree SHA".to_string())?
                    .to_string();
                if merged_tree == root_now_tree {
                    return Ok(WorktreeMergeOutcome::NoOp);
                }

                let changed_files =
                    diff_name_only(&self.workspace_root, &root_now_tree, &merged_tree)?;
                let unified_diff =
                    diff_unified(&self.workspace_root, &root_now_tree, &merged_tree)?;
                Ok(WorktreeMergeOutcome::Clean {
                    root_now_commit,
                    root_now_tree,
                    merged_tree,
                    changed_files,
                    unified_diff,
                })
            }
            Some(1) => {
                let conflicting_files = parse_merge_tree_conflicts(&stdout);
                let details = if stderr.trim().is_empty() {
                    stdout.trim().to_string()
                } else {
                    format!("{}\n{}", stdout.trim(), stderr.trim())
                };
                Ok(WorktreeMergeOutcome::Conflict {
                    root_now_commit,
                    conflicting_files,
                    details,
                })
            }
            code => Err(format!(
                "git merge-tree failed (exit {:?}): {}",
                code,
                stderr.trim()
            )),
        }
    }

    /// Materializa sobre el *working tree* de `root` **exclusivamente** el delta entre
    /// `root_now_commit` y `merged_tree`, sin mover `HEAD`, sin alterar `.git/index` y
    /// rechazando cualquier ruta del Control Plane (`.onyx/config.toml`, `.env*`, `.git/**`).
    pub fn apply_clean_merge(
        &self,
        root_now_commit: &str,
        merged_tree: &str,
    ) -> Result<Vec<String>, String> {
        let entries = diff_name_status(&self.workspace_root, root_now_commit, merged_tree)?;
        for (_, rel_path) in &entries {
            validate_merge_target_path(rel_path)?;
        }

        let mut applied = Vec::with_capacity(entries.len());
        for (status, rel_path) in entries {
            let abs = self.workspace_root.join(&rel_path);
            match status.as_str() {
                "D" => {
                    if abs.is_file() || abs.is_symlink() {
                        fs::remove_file(&abs).map_err(|e| {
                            format!("failed to remove merged deletion `{rel_path}`: {e}")
                        })?;
                    }
                    prune_empty_parents(&self.workspace_root, abs.parent());
                }
                "A" | "M" | "T" => {
                    let blob = read_tree_blob(&self.workspace_root, merged_tree, &rel_path)?;
                    if let Some(parent) = abs.parent() {
                        fs::create_dir_all(parent).map_err(|e| {
                            format!("failed to create parent directory for `{rel_path}`: {e}")
                        })?;
                    }
                    fs::write(&abs, blob)
                        .map_err(|e| format!("failed to write merged file `{rel_path}`: {e}"))?;
                }
                other => {
                    return Err(format!(
                        "unsupported git diff status `{other}` for `{rel_path}` during merge apply"
                    ));
                }
            }
            applied.push(rel_path);
        }

        Ok(applied)
    }

    /// Captura el `tree_sha` actual del *working tree* de `root` sin mutar `HEAD` ni `.git/index`.
    pub fn current_working_tree_sha(&self) -> Result<String, String> {
        let (_, tree_sha) = capture_git_tree_checkpoint(&self.workspace_root).ok_or_else(|| {
            format!(
                "failed to capture current git working tree checkpoint for `{}`",
                self.workspace_root.display()
            )
        })?;
        Ok(tree_sha)
    }

    /// Versión resistente a **TOCTOU** (*Time-of-Check to Time-of-Use*) de [`Self::apply_clean_merge`]:
    /// antes de escribir un solo byte, verifica que el *working tree* actual de `root` siga teniendo
    /// exactamente `expected_root_now_tree` (el árbol evaluado durante el preview/aprobación humana).
    /// Si `root` (u otro proceso) modificó el disco mientras tanto, aborta limpio sin mutar archivos.
    pub fn apply_clean_merge_checked(
        &self,
        root_now_commit: &str,
        expected_root_now_tree: &str,
        merged_tree: &str,
    ) -> Result<Vec<String>, String> {
        let live_tree = self.current_working_tree_sha()?;
        if live_tree != expected_root_now_tree {
            return Err(format!(
                "TOCTOU guard aborted merge: root working tree changed since preview/approval (expected tree `{expected_root_now_tree}`, current tree `{live_tree}`); re-run `subagent_merge` to refresh preview and impact assessment"
            ));
        }
        self.apply_clean_merge(root_now_commit, merged_tree)
    }

    /// Desmonta el *worktree* y elimina la rama temporal `onyx/sub/<job_id>`.
    /// El llamador debe haber cerrado antes cualquier `ProcessJob` asociado para evitar
    /// bloqueos de archivos en Windows.
    pub fn remove_worktree(&self, handle: &WorktreeHandle) -> Result<(), String> {
        let rm = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&handle.worktree_path)
            .current_dir(&self.workspace_root)
            .output()
            .map_err(|e| format!("failed to run git worktree remove: {e}"))?;

        if !rm.status.success() {
            if handle.worktree_path.exists() {
                fs::remove_dir_all(&handle.worktree_path).map_err(|e| {
                    format!(
                        "git worktree remove failed ({}) and fallback directory cleanup failed: {e}",
                        String::from_utf8_lossy(&rm.stderr).trim()
                    )
                })?;
            }
            let _ = self.prune_worktrees();
        }

        let br_del = Command::new("git")
            .args(["branch", "-D", &handle.branch_name])
            .current_dir(&self.workspace_root)
            .output()
            .map_err(|e| format!("failed to run git branch -D `{}`: {e}", handle.branch_name))?;
        if !br_del.status.success() {
            let full_ref = format!("refs/heads/{}", handle.branch_name);
            let still_exists = Command::new("git")
                .args(["show-ref", "--verify", "--quiet", &full_ref])
                .current_dir(&self.workspace_root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if still_exists {
                return Err(format!(
                    "failed to delete temporary branch `{}`: {}",
                    handle.branch_name,
                    String::from_utf8_lossy(&br_del.stderr).trim()
                ));
            }
        }
        Ok(())
    }
}

/// Comprueba que `git` esté disponible y cumpla la versión mínima `>= 2.38.0` (`merge-tree --write-tree`).
pub fn check_git_version() -> Result<GitVersion, String> {
    let out = Command::new("git")
        .arg("--version")
        .output()
        .map_err(|e| format!("git is not available on PATH: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git --version failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let ver = parse_git_version(&raw)
        .ok_or_else(|| format!("could not parse git version from `{}`", raw.trim()))?;
    if !ver.supports_merge_tree_write_tree() {
        return Err(format!(
            "Git {}.{}.{} or newer is required for 3-way worktree reconciliation (`git merge-tree --write-tree`), found {ver}",
            MIN_GIT_VERSION.0, MIN_GIT_VERSION.1, MIN_GIT_VERSION.2
        ));
    }
    Ok(ver)
}

/// Parsea salidas estándar de `git --version` en Linux, macOS y Windows (`2.47.1.windows.2`).
pub fn parse_git_version(raw: &str) -> Option<GitVersion> {
    for token in raw.split_whitespace() {
        let mut nums = token.split('.').take(3).map(|seg| {
            let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u32>().ok()
        });
        let (Some(Some(major)), Some(Some(minor))) = (nums.next(), nums.next()) else {
            continue;
        };
        let patch = nums.next().flatten().unwrap_or(0);
        return Some(GitVersion {
            major,
            minor,
            patch,
        });
    }
    None
}

/// Valida que `job_id` sea seguro para nombres de directorio y ramas Git (`onyx/sub/<job_id>`).
pub fn validate_job_id(job_id: &str) -> Result<(), String> {
    if job_id.is_empty() || job_id.len() > 64 {
        return Err("worktree job_id must be 1..=64 ASCII characters".to_string());
    }
    if job_id.starts_with('-') {
        return Err(format!(
            "invalid worktree job_id `{job_id}`: cannot start with `-`"
        ));
    }
    if !job_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "invalid worktree job_id `{job_id}`: only `[A-Za-z0-9_-]` characters are allowed"
        ));
    }
    Ok(())
}

/// Resuelve el directorio base donde se ubicarán los *worktrees* del workspace:
/// 1. Override explícito o variable `ONYX_WT_DIR`.
/// 2. Carpeta hermana `<workspace_parent>/.onyx-wt/<workspace_name>` (misma partición, ruta corta).
/// 3. Fallback a `std::env::temp_dir().join("onyx-wt").join(<workspace_name>)`.
pub fn resolve_worktree_base_dir(
    workspace_root: &Path,
    explicit_override: Option<&Path>,
) -> PathBuf {
    let ws_name = workspace_root
        .file_name()
        .and_then(|n| n.to_str())
        .map(sanitize_path_segment)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "workspace".to_string());

    if let Some(dir) = explicit_override {
        return strip_verbatim_prefix(dir).join(&ws_name);
    }
    if let Ok(raw) = env::var(ONYX_WT_DIR_ENV) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return strip_verbatim_prefix(Path::new(trimmed)).join(&ws_name);
        }
    }
    if let Some(parent) = workspace_root.parent() {
        let sibling = strip_verbatim_prefix(parent)
            .join(".onyx-wt")
            .join(&ws_name);
        if fs::create_dir_all(&sibling).is_ok() {
            return sibling;
        }
    }
    strip_verbatim_prefix(&env::temp_dir())
        .join("onyx-wt")
        .join(&ws_name)
}

fn sanitize_path_segment(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect()
}

fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{stripped}"));
        }
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    path.to_path_buf()
}

fn create_ephemeral_commit(
    repo_dir: &Path,
    explicit_parent: Option<&str>,
    message: &str,
) -> Result<(String, String), String> {
    create_ephemeral_commit_excluding(repo_dir, explicit_parent, message, &[])
}

fn create_ephemeral_commit_excluding(
    repo_dir: &Path,
    explicit_parent: Option<&str>,
    message: &str,
    extra_excludes: &[&str],
) -> Result<(String, String), String> {
    let (head_sha, tree_sha) = capture_git_tree_checkpoint_excluding(repo_dir, extra_excludes)
        .ok_or_else(|| {
            format!(
                "failed to capture git tree checkpoint for `{}`",
                repo_dir.display()
            )
        })?;

    let mut cmd = Command::new("git");
    cmd.arg("commit-tree").arg(&tree_sha);
    let parent = explicit_parent
        .filter(|p| !p.is_empty())
        .or_else(|| (!head_sha.is_empty()).then_some(head_sha.as_str()));
    if let Some(p) = parent {
        cmd.arg("-p").arg(p);
    }
    cmd.arg("-m")
        .arg(message)
        .env("GIT_AUTHOR_NAME", "openOnyx")
        .env("GIT_AUTHOR_EMAIL", "openonyx@local")
        .env("GIT_COMMITTER_NAME", "openOnyx")
        .env("GIT_COMMITTER_EMAIL", "openonyx@local")
        .current_dir(repo_dir);

    let out = cmd
        .output()
        .map_err(|e| format!("failed to run git commit-tree: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let commit_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if commit_sha.is_empty() {
        return Err("git commit-tree returned empty commit SHA".to_string());
    }
    Ok((commit_sha, tree_sha))
}

fn rev_parse_tree(repo_dir: &Path, commit_or_tree: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(["rev-parse", &format!("{commit_or_tree}^{{tree}}")])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("failed to resolve git tree for `{commit_or_tree}`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git rev-parse {commit_or_tree}^{{tree}} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn diff_name_only(repo_dir: &Path, from: &str, to: &str) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .args([
            "-c",
            "core.quotepath=false",
            "diff",
            "--name-only",
            from,
            to,
        ])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("failed to run git diff --name-only: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git diff --name-only failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn diff_unified(repo_dir: &Path, from: &str, to: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(["-c", "core.quotepath=false", "diff", "--no-color", from, to])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("failed to run git diff: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn diff_name_status(
    repo_dir: &Path,
    from: &str,
    to: &str,
) -> Result<Vec<(String, String)>, String> {
    let out = Command::new("git")
        .args([
            "-c",
            "core.quotepath=false",
            "diff",
            "--name-status",
            "--no-renames",
            "-z",
            from,
            to,
        ])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("failed to run git diff --name-status: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git diff --name-status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let parts: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|slice| !slice.is_empty())
        .map(|slice| String::from_utf8_lossy(slice).into_owned())
        .collect();
    let mut entries = Vec::with_capacity(parts.len() / 2);
    let mut i = 0;
    while i + 1 < parts.len() {
        entries.push((parts[i].clone(), parts[i + 1].clone()));
        i += 2;
    }
    Ok(entries)
}

fn read_tree_blob(repo_dir: &Path, tree_sha: &str, rel_path: &str) -> Result<Vec<u8>, String> {
    let spec = format!("{tree_sha}:{rel_path}");
    let out = Command::new("git")
        .args(["cat-file", "blob", &spec])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("failed to read merged blob `{spec}`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git cat-file blob `{spec}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn validate_merge_target_path(rel_path: &str) -> Result<(), String> {
    let norm = rel_path.replace('\\', "/");
    if norm.is_empty()
        || norm.starts_with('/')
        || norm.split('/').any(|seg| seg == ".." || seg == ".git")
    {
        return Err(format!(
            "refusing to apply merge path outside workspace: `{rel_path}`"
        ));
    }
    if is_control_plane_rel_path(&norm) {
        return Err(format!(
            "refusing to merge subagent edit into protected Control Plane path `{rel_path}`"
        ));
    }
    Ok(())
}

fn prune_empty_parents(stop_at: &Path, start: Option<&Path>) {
    let mut cur = start;
    while let Some(dir) = cur {
        if dir == stop_at || !dir.starts_with(stop_at) {
            break;
        }
        let is_empty = fs::read_dir(dir)
            .ok()
            .and_then(|mut it| it.next())
            .is_none();
        if !is_empty || fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn parse_merge_tree_conflicts(stdout: &str) -> Vec<String> {
    let mut files = BTreeSet::new();
    for line in stdout.lines().skip(1) {
        if let Some((_, path)) = line.split_once('\t') {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                files.insert(trimmed.to_string());
                continue;
            }
        }
        if let Some(idx) = line.find("Merge conflict in ") {
            let path = line[idx + "Merge conflict in ".len()..].trim();
            if !path.is_empty() {
                files.insert(path.to_string());
            }
        }
    }
    files.into_iter().collect()
}

fn parse_worktree_porcelain(stdout: &str) -> Vec<WorktreeInfo> {
    let mut out = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_head: Option<String> = None;
    let mut cur_branch: Option<String> = None;

    let flush = |out: &mut Vec<WorktreeInfo>,
                 path: &mut Option<PathBuf>,
                 head: &mut Option<String>,
                 branch: &mut Option<String>| {
        if let Some(p) = path.take() {
            let b = branch.take();
            let is_sub = b
                .as_deref()
                .is_some_and(|r| r.contains(SUBAGENT_BRANCH_PREFIX));
            out.push(WorktreeInfo {
                path: p,
                head_sha: head.take(),
                branch: b,
                is_onyx_subagent: is_sub,
            });
        } else {
            *head = None;
            *branch = None;
        }
    };

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut out, &mut cur_path, &mut cur_head, &mut cur_branch);
        } else if let Some(p) = trimmed.strip_prefix("worktree ") {
            flush(&mut out, &mut cur_path, &mut cur_head, &mut cur_branch);
            cur_path = Some(PathBuf::from(p.trim()));
        } else if let Some(h) = trimmed.strip_prefix("HEAD ") {
            cur_head = Some(h.trim().to_string());
        } else if let Some(b) = trimmed.strip_prefix("branch ") {
            cur_branch = Some(b.trim().to_string());
        }
    }
    flush(&mut out, &mut cur_path, &mut cur_head, &mut cur_branch);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_temp_git_repo(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "onyx_wt_test_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let run_git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@local")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@local")
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        run_git(&["init", "-q"]);
        fs::write(dir.join("a.txt"), "line1\nline2\n").unwrap();
        fs::write(dir.join("b.txt"), "orig-b\n").unwrap();
        fs::write(dir.join("delete_me.txt"), "bye\n").unwrap();
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "initial"]);
        dir
    }

    fn git_head_sha(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn parse_git_version_handles_windows_macos_and_linux_strings() {
        let win = parse_git_version("git version 2.47.1.windows.2").unwrap();
        assert_eq!(win.as_tuple(), (2, 47, 1));
        assert!(win.supports_merge_tree_write_tree());

        let mac = parse_git_version("git version 2.39.3 (Apple Git-146)").unwrap();
        assert_eq!(mac.as_tuple(), (2, 39, 3));
        assert!(mac.supports_merge_tree_write_tree());

        let old = parse_git_version("git version 2.37.9").unwrap();
        assert!(!old.supports_merge_tree_write_tree());
    }

    #[test]
    fn validate_job_id_rejects_traversal_and_flag_injection() {
        assert!(validate_job_id("job_01-alpha").is_ok());
        assert!(validate_job_id("").is_err());
        assert!(validate_job_id("-bad").is_err());
        assert!(validate_job_id("../escape").is_err());
        assert!(validate_job_id("a/b").is_err());
        assert!(validate_job_id("has space").is_err());
    }

    #[test]
    fn worktree_create_captures_uncommitted_root_changes_and_merges_cleanly_without_moving_head() {
        let repo = init_temp_git_repo("clean_merge");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("onyx_wt_base_clean_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);
        let head_before = git_head_sha(&repo);

        // Uncommitted edit in root BEFORE spawn_tick must be visible inside the new worktree.
        fs::write(repo.join("a.txt"), "line1-dirty-before-spawn\nline2\n").unwrap();

        let mgr = WorktreeManager::with_base_dir_override(&repo, Some(&wt_base)).unwrap();
        let handle = mgr.create_worktree("job-1", 10).unwrap();
        assert!(handle.worktree_path.is_dir());
        assert_eq!(
            fs::read_to_string(handle.worktree_path.join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "line1-dirty-before-spawn\nline2\n"
        );
        assert!(
            mgr.list_worktrees()
                .unwrap()
                .iter()
                .any(|w| w.is_onyx_subagent)
        );

        // Concurrent edits: root edits `a.txt` further; subagent edits `b.txt`, adds `new.txt`, removes `delete_me.txt`.
        fs::write(
            repo.join("a.txt"),
            "line1-dirty-before-spawn\nline2-root-concurrent\n",
        )
        .unwrap();
        fs::write(handle.worktree_path.join("b.txt"), "subagent-updated-b\n").unwrap();
        fs::create_dir_all(handle.worktree_path.join("nested")).unwrap();
        fs::write(
            handle.worktree_path.join("nested/new.txt"),
            "created by sub\n",
        )
        .unwrap();
        fs::remove_file(handle.worktree_path.join("delete_me.txt")).unwrap();

        let (sub_commit, _) = mgr.capture_subagent_commit(&handle).unwrap();
        let outcome = mgr.preview_three_way_merge(&handle, &sub_commit).unwrap();
        let (root_now_commit, merged_tree) = match outcome {
            WorktreeMergeOutcome::Clean {
                root_now_commit,
                merged_tree,
                changed_files,
                unified_diff,
                ..
            } => {
                assert!(
                    changed_files.contains(&"b.txt".to_string()),
                    "{changed_files:?}"
                );
                assert!(
                    changed_files.contains(&"nested/new.txt".to_string()),
                    "{changed_files:?}"
                );
                assert!(
                    changed_files.contains(&"delete_me.txt".to_string()),
                    "{changed_files:?}"
                );
                assert!(
                    unified_diff.contains("subagent-updated-b"),
                    "{unified_diff}"
                );
                (root_now_commit, merged_tree)
            }
            other => panic!("expected Clean 3-way merge, got {other:?}"),
        };

        // Preview MUST NOT mutate root disk yet.
        assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "orig-b\n");
        assert!(repo.join("delete_me.txt").exists());

        // Apply delta without moving HEAD.
        let applied = mgr
            .apply_clean_merge(&root_now_commit, &merged_tree)
            .unwrap();
        assert_eq!(applied.len(), 3, "{applied:?}");
        assert_eq!(
            fs::read_to_string(repo.join("a.txt")).unwrap(),
            "line1-dirty-before-spawn\nline2-root-concurrent\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("b.txt")).unwrap(),
            "subagent-updated-b\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("nested/new.txt")).unwrap(),
            "created by sub\n"
        );
        assert!(!repo.join("delete_me.txt").exists());
        assert_eq!(git_head_sha(&repo), head_before, "HEAD must never move");

        mgr.remove_worktree(&handle).unwrap();
        mgr.prune_worktrees().unwrap();
        assert!(!handle.worktree_path.exists());
        assert!(
            !mgr.list_worktrees()
                .unwrap()
                .iter()
                .any(|w| w.is_onyx_subagent)
        );
        let branch_ref = format!("refs/heads/{}", handle.branch_name);
        let branch_exists = Command::new("git")
            .args(["show-ref", "--verify", "--quiet", &branch_ref])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success();
        assert!(
            !branch_exists,
            "temporary branch `{}` must be deleted by remove_worktree",
            handle.branch_name
        );

        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn capture_subagent_commit_excludes_onyx_operational_artifacts() {
        let repo = init_temp_git_repo("sub_exc");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("onyx_wt_base_subexc_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let mgr = WorktreeManager::with_base_dir_override(&repo, Some(&wt_base)).unwrap();
        let handle = mgr.create_worktree("job-2", 5).unwrap();

        // Trabajo real del hijo + artefactos operativos que OnYX mismo crea en el
        // *worktree* (caché scent + su `.gitignore` de higiene, p. ej. al evaluar el
        // impacto de una mutación con la feature `scent`).
        fs::write(handle.worktree_path.join("b.txt"), "subagent-updated-b\n").unwrap();
        fs::create_dir_all(handle.worktree_path.join(".onyx")).unwrap();
        fs::write(
            handle.worktree_path.join(".onyx/.gitignore"),
            "scent.idx\nscent.idx.tmp*\n",
        )
        .unwrap();
        fs::write(handle.worktree_path.join(".onyx/scent.idx"), b"ONYXSCI1").unwrap();
        fs::write(
            handle.worktree_path.join(".onyx/scent.idx.tmp.1.2"),
            b"partial",
        )
        .unwrap();
        // Un `.onyx/` que NO es artefacto de la caché sigue siendo trabajo del hijo
        // (y p. ej. `config.toml` se bloquea aparte por Control Plane).
        fs::write(handle.worktree_path.join(".onyx/notes.txt"), "child note\n").unwrap();

        let (sub_commit, _) = mgr.capture_subagent_commit(&handle).unwrap();
        let changed = mgr
            .changed_files_between(&handle.base_commit, &sub_commit)
            .unwrap();
        assert_eq!(
            changed,
            vec![".onyx/notes.txt".to_string(), "b.txt".to_string(),],
            "{changed:?}"
        );

        // Si lo único "cambiado" son artefactos operativos, el preview debe ser
        // `NoOp` (no hay trabajo que aplicar) y el *worktree* se retira sin más.
        let only_artifacts = mgr.create_worktree("job-3", 6).unwrap();
        fs::create_dir_all(only_artifacts.worktree_path.join(".onyx")).unwrap();
        fs::write(
            only_artifacts.worktree_path.join(".onyx/scent.idx"),
            b"ONYXSCI1",
        )
        .unwrap();
        let (sub_only, _) = mgr.capture_subagent_commit(&only_artifacts).unwrap();
        let outcome = mgr
            .preview_three_way_merge(&only_artifacts, &sub_only)
            .unwrap();
        assert!(matches!(outcome, WorktreeMergeOutcome::NoOp), "{outcome:?}");

        mgr.remove_worktree(&handle).unwrap();
        mgr.remove_worktree(&only_artifacts).unwrap();
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn worktree_three_way_merge_detects_overlapping_conflict_without_touching_root_disk() {
        let repo = init_temp_git_repo("conflict");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("onyx_wt_base_conflict_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let mgr = WorktreeManager::with_base_dir_override(&repo, Some(&wt_base)).unwrap();
        let handle = mgr.create_worktree("job-conflict", 20).unwrap();

        // Both root and subagent modify the exact same line in `a.txt`.
        fs::write(repo.join("a.txt"), "root-change\nline2\n").unwrap();
        fs::write(handle.worktree_path.join("a.txt"), "sub-change\nline2\n").unwrap();

        let (sub_commit, _) = mgr.capture_subagent_commit(&handle).unwrap();
        match mgr.preview_three_way_merge(&handle, &sub_commit).unwrap() {
            WorktreeMergeOutcome::Conflict {
                conflicting_files, ..
            } => {
                assert!(
                    conflicting_files.contains(&"a.txt".to_string()),
                    "expected a.txt in conflicts: {conflicting_files:?}"
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        // Root working tree must remain untouched.
        assert_eq!(
            fs::read_to_string(repo.join("a.txt")).unwrap(),
            "root-change\nline2\n"
        );

        mgr.remove_worktree(&handle).unwrap();
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn apply_clean_merge_blocks_control_plane_paths() {
        let repo = init_temp_git_repo("ctrl_plane");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("onyx_wt_base_ctrl_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let mgr = WorktreeManager::with_base_dir_override(&repo, Some(&wt_base)).unwrap();
        let handle = mgr.create_worktree("job-ctrl", 30).unwrap();

        fs::create_dir_all(handle.worktree_path.join(".onyx")).unwrap();
        fs::write(
            handle.worktree_path.join(".onyx/config.toml"),
            "[permissions]\nallow = [\"shell_exec(*)\"]\n",
        )
        .unwrap();

        let (sub_commit, _) = mgr.capture_subagent_commit(&handle).unwrap();
        let (root_now_commit, merged_tree) =
            match mgr.preview_three_way_merge(&handle, &sub_commit).unwrap() {
                WorktreeMergeOutcome::Clean {
                    root_now_commit,
                    merged_tree,
                    ..
                } => (root_now_commit, merged_tree),
                other => panic!("expected Clean diff, got {other:?}"),
            };

        let err = mgr
            .apply_clean_merge(&root_now_commit, &merged_tree)
            .unwrap_err();
        assert!(err.contains("Control Plane"), "{err}");
        assert!(!repo.join(".onyx/config.toml").exists());

        mgr.remove_worktree(&handle).unwrap();
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn apply_clean_merge_checked_aborts_on_toctou_without_touching_disk() {
        let repo = init_temp_git_repo("toctou_guard");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("onyx_wt_base_toctou_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let mgr = WorktreeManager::with_base_dir_override(&repo, Some(&wt_base)).unwrap();
        let handle = mgr.create_worktree("job-toctou", 40).unwrap();

        fs::write(handle.worktree_path.join("b.txt"), "subagent-b\n").unwrap();
        let (sub_commit, _) = mgr.capture_subagent_commit(&handle).unwrap();

        let (root_now_commit, root_now_tree, merged_tree) =
            match mgr.preview_three_way_merge(&handle, &sub_commit).unwrap() {
                WorktreeMergeOutcome::Clean {
                    root_now_commit,
                    root_now_tree,
                    merged_tree,
                    ..
                } => (root_now_commit, root_now_tree, merged_tree),
                other => panic!("expected Clean preview, got {other:?}"),
            };

        // Simulate concurrent root mutation between approval preview and merge apply (TOCTOU window).
        fs::write(repo.join("a.txt"), "root-modified-during-approval\n").unwrap();

        let err = mgr
            .apply_clean_merge_checked(&root_now_commit, &root_now_tree, &merged_tree)
            .unwrap_err();
        assert!(err.contains("TOCTOU"), "{err}");
        // Must leave `b.txt` untouched when TOCTOU guard aborts!
        assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "orig-b\n");
        assert_eq!(
            fs::read_to_string(repo.join("a.txt")).unwrap(),
            "root-modified-during-approval\n"
        );

        // Re-previewing refreshes `root_now_tree` and merges cleanly.
        let (c2, t2, m2) = match mgr.preview_three_way_merge(&handle, &sub_commit).unwrap() {
            WorktreeMergeOutcome::Clean {
                root_now_commit,
                root_now_tree,
                merged_tree,
                ..
            } => (root_now_commit, root_now_tree, merged_tree),
            other => panic!("expected Clean after refresh, got {other:?}"),
        };
        mgr.apply_clean_merge_checked(&c2, &t2, &m2).unwrap();
        assert_eq!(
            fs::read_to_string(repo.join("b.txt")).unwrap(),
            "subagent-b\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("a.txt")).unwrap(),
            "root-modified-during-approval\n"
        );

        mgr.remove_worktree(&handle).unwrap();
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }
}
