//! Persistencia del índice de snapshots entre procesos.

use std::sync::atomic::{AtomicU64, Ordering};

use onyx_soul_agents::{AgentSnapshotStore, SnapshotStore};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "onyx-soul-agents-snap-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn new_rereads_existing_tick_files_so_latest_tick_survives_a_restart() {
    let dir = temp_dir();
    let mut store = AgentSnapshotStore::new(&dir);
    store.save_tick(3, b"three").unwrap();
    store.save_tick(9, b"nine").unwrap();
    store.save_tick(1, b"one").unwrap();
    assert_eq!(store.latest_tick(), Some(9));
    drop(store);

    let store = AgentSnapshotStore::new(&dir);
    assert_eq!(
        store.latest_tick(),
        Some(9),
        "si new no relee el directorio, latest_tick queda vacío tras reiniciar"
    );
    assert_eq!(store.load_tick(1).unwrap(), b"one");
    assert_eq!(store.load_tick(3).unwrap(), b"three");
    assert_eq!(store.load_tick(9).unwrap(), b"nine");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_on_an_empty_directory_has_no_latest_tick() {
    let dir = temp_dir();
    let store = AgentSnapshotStore::new(&dir);
    assert_eq!(store.latest_tick(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_ignores_files_that_are_not_canonical_tick_snapshots() {
    let dir = temp_dir();
    let mut store = AgentSnapshotStore::new(&dir);
    store.save_tick(4, b"four").unwrap();
    store.save("notes", b"free-form").unwrap();
    std::fs::write(dir.join("tick-nope.bin"), b"bad").unwrap();
    std::fs::write(dir.join("tick-012.bin"), b"padded").unwrap();
    drop(store);

    let store = AgentSnapshotStore::new(&dir);
    assert_eq!(store.latest_tick(), Some(4));
    assert_eq!(store.load_tick(4).unwrap(), b"four");
    assert_eq!(store.load("notes").unwrap(), b"free-form");

    let _ = std::fs::remove_dir_all(&dir);
}
