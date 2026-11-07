use onyx_core::graph::SnapshotStore; // o onyx_core::SnapshotStore si lo reexportas
use onyx_core::OnyxError;

pub struct FileSnapshotStore { /* ... */ }

impl SnapshotStore for FileSnapshotStore {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> { ... }
    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> { ... }
}