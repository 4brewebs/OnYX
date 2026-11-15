//! Tick-indexed snapshot store.
//!
//! Keeps one snapshot per tick so the application can rewind.
//! Reuses the file format of `FileSnapshotStore` but adds an index.
//!
//! [`SnapshotStore`] keys that parse as `u64` are ticks (`"3"` and `"003"`
//! both mean `tick-3.bin`). Any other key is a free key: a single relative
//! file name inside the store directory. Free keys with a separator, `.`,
//! `..`, an absolute path or a prefix, and free keys equal to a canonical
//! tick file name (`tick-3.bin`), are rejected with `OnyxError::Format`
//! before touching the file system. Existing files are never deleted or
//! renamed. This bounds the key to the directory by name only; it does not
//! guard against links placed inside the directory, nor promise durability.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

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

/// Tick of a canonical tick file name: `tick-12.bin`, not `tick-012.bin`.
fn tick_of(name: &str) -> Option<u64> {
    let tick: u64 = name
        .strip_prefix("tick-")?
        .strip_suffix(".bin")?
        .parse()
        .ok()?;
    (name == format!("tick-{tick}.bin")).then_some(tick)
}

/// Path of a free key, or why it is not one.
fn free_path(dir: &Path, key: &str) -> Result<PathBuf, OnyxError> {
    let mut parts = Path::new(key).components();
    let single = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(name)), None) if name == key
    );
    if !single {
        return Err(OnyxError::Format(format!(
            "snapshot key {key:?} must be a single relative file name"
        )));
    }
    if tick_of(key).is_some() {
        return Err(OnyxError::Format(format!(
            "snapshot key {key:?} is reserved for a numeric tick"
        )));
    }
    Ok(dir.join(key))
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
        // `tick-012.bin` no es el nombre que escribe `path_for(12)`.
        if let Some(tick) = tick_of(name) {
            index.insert(tick, path);
        }
    }
    index
}

impl SnapshotStore for AgentSnapshotStore {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        // Accept either a numeric tick or a free-form key.
        if let Ok(tick) = key.parse::<u64>() {
            self.save_tick(tick, bytes)
        } else {
            let path = free_path(&self.dir, key)?;
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
            let path = free_path(&self.dir, key)?;
            let mut f = File::open(&path).map_err(|e| OnyxError::Io(e.to_string()))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)
                .map_err(|e| OnyxError::Io(e.to_string()))?;
            Ok(buf)
        }
    }
}
