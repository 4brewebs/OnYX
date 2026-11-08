//! Persistencia de snapshots en disco.
//! Zero-deps. Solo `std`.

use std::fs;
use std::path::{Path, PathBuf};

use onyx_core::graph::SnapshotStore;
use onyx_core::OnyxError;

/// Almacena snapshots como ficheros en un directorio.
/// El `key` se usa como nombre de fichero (se sanitiza mínimamente).
pub struct FileSnapshotStore {
    dir: PathBuf,
}

impl FileSnapshotStore {
    /// Crea el store. El directorio se crea si no existe.
    pub fn new(dir: impl AsRef<Path>) -> Result<Self, OnyxError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| OnyxError::Io(e.to_string()))?;
        Ok(Self { dir })
    }

    fn path_for(&self, key: &str) -> PathBuf {
        // Evita path traversal básico
        let safe = key.replace(['/', '\\', '.'], "_");
        self.dir.join(format!("{safe}.snap"))
    }
}

impl SnapshotStore for FileSnapshotStore {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        let path = self.path_for(key);
        fs::write(&path, bytes).map_err(|e| OnyxError::Io(e.to_string()))
    }

    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> {
        let path = self.path_for(key);
        fs::read(&path).map_err(|e| OnyxError::Io(e.to_string()))
    }
}
