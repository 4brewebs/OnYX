use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Política de seguridad y límites de I/O para operaciones sobre el workspace.
#[derive(Clone, Debug)]
pub struct WorkspacePolicy {
    pub root: PathBuf,
    pub ignored_dirs: Vec<String>,
    pub max_read_bytes: usize,
    pub max_output_bytes: usize,
    pub command_timeout: Duration,
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
        })
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        path.components().any(|c| {
            if let Component::Normal(name) = c {
                let s = name.to_string_lossy();
                self.ignored_dirs
                    .iter()
                    .any(|ignored| ignored == s.as_ref())
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
