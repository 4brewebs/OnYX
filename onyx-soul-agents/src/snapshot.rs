//! Tick-indexed snapshot store.
//!
//! Keeps one snapshot per tick so the application can rewind.
//! Reuses the file format of `FileSnapshotStore` but adds an index.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use onyx_core::{OnyxError, SnapshotStore};

pub struct AgentSnapshotStore {
    dir: PathBuf,
    index: BTreeMap<u64, PathBuf>,
}

impl AgentSnapshotStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).ok();
        Self {
            index: index_existing_ticks(&dir),
            dir,
        }
    }

    /// Path of the snapshot for a given tick.
    fn path_for(&self, tick: u64) -> PathBuf {
        self.dir.join(format!("tick-{tick}.bin"))
    }

    /// Save under a tick key. The application decides when to call this.
    pub fn save_tick(&mut self, tick: u64, bytes: &[u8]) -> Result<(), OnyxError> {
        let path = self.path_for(tick);
        let mut f = File::create(&path).map_err(|e| OnyxError::Io(e.to_string()))?;
        f.write_all(bytes)
            .map_err(|e| OnyxError::Io(e.to_string()))?;
        self.index.insert(tick, path);
        Ok(())
    }

    /// Load the snapshot for a tick.
    pub fn load_tick(&self, tick: u64) -> Result<Vec<u8>, OnyxError> {
        let path = self.path_for(tick);
        let mut f = File::open(&path).map_err(|e| OnyxError::Io(e.to_string()))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .map_err(|e| OnyxError::Io(e.to_string()))?;
        Ok(buf)
    }

    /// Latest tick that has a snapshot, if any.
    pub fn latest_tick(&self) -> Option<u64> {
        self.index.keys().next_back().copied()
    }
}

fn index_existing_ticks(dir: &Path) -> BTreeMap<u64, PathBuf> {
    let mut index = BTreeMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return index;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(tick_str) = name
            .strip_prefix("tick-")
            .and_then(|rest| rest.strip_suffix(".bin"))
        else {
            continue;
        };
        let Ok(tick) = tick_str.parse::<u64>() else {
            continue;
        };
        // `tick-012.bin` no es el nombre que escribe `path_for(12)`.
        if name != format!("tick-{tick}.bin") {
            continue;
        }
        index.insert(tick, path);
    }
    index
}

impl SnapshotStore for AgentSnapshotStore {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        // Accept either a numeric tick or a free-form key.
        if let Ok(tick) = key.parse::<u64>() {
            self.save_tick(tick, bytes)
        } else {
            let path = self.dir.join(key);
            let mut f = File::create(&path).map_err(|e| OnyxError::Io(e.to_string()))?;
            f.write_all(bytes)
                .map_err(|e| OnyxError::Io(e.to_string()))?;
            Ok(())
        }
    }

    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> {
        if let Ok(tick) = key.parse::<u64>() {
            self.load_tick(tick)
        } else {
            let path = self.dir.join(key);
            let mut f = File::open(&path).map_err(|e| OnyxError::Io(e.to_string()))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)
                .map_err(|e| OnyxError::Io(e.to_string()))?;
            Ok(buf)
        }
    }
}
