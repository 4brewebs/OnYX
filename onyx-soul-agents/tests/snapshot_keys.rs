//! Claves libres de `AgentSnapshotStore`: no salen del directorio del store
//! ni pisan los snapshots numéricos. Cada prueba usa su propio directorio
//! temporal, con centinelas propios fuera del store, y borra solo ese directorio.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use onyx_soul_agents::{AgentSnapshotStore, SnapshotStore};

static NEXT: AtomicU64 = AtomicU64::new(0);
const SENTINEL: &[u8] = b"centinela-externo";

/// `root/` propio de la prueba con `root/store/` como directorio del store.
/// Se crea en exclusiva (falla si ya existía) y se borra al soltarse.
struct Sandbox {
    root: PathBuf,
}
impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "onyx-forja-snapkeys-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("directorio temporal propio y nuevo");
        Self { root }
    }
    fn store_dir(&self) -> PathBuf {
        self.root.join("store")
    }
    /// Centinela fuera del store, dentro de nuestro `root`.
    fn sentinel(&self, name: &str) -> PathBuf {
        let p = self.root.join(name);
        fs::write(&p, SENTINEL).unwrap();
        p
    }
    /// Entradas de `root` salvo `store/`: lo que el store no debe tocar.
    fn outside(&self) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<_> = fs::read_dir(&self.root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "store")
            .map(|e| {
                let bytes = fs::read(e.path()).unwrap_or_default();
                (e.file_name().to_string_lossy().into_owned(), bytes)
            })
            .collect();
        v.sort();
        v
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Bytes como texto para que el fallo diga qué contenido se leyó o escribió.
fn txt(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn abs(p: &Path) -> String {
    assert!(p.is_absolute());
    p.to_str().unwrap().to_string()
}

// ---------- rutas que escapan (rojo en 3ec48b8) ----------

#[test]
fn save_with_parent_dir_key_does_not_write_outside_the_store() {
    let sb = Sandbox::new();
    let sentinel = sb.sentinel("escaped.bin");
    let mut store = AgentSnapshotStore::new(sb.store_dir());
    let before = sb.outside();
    // Rechazar o guardar dentro del store son válidos; tocar el centinela no.
    let _ = store.save("../escaped.bin", b"payload");
    assert_eq!(
        txt(&fs::read(&sentinel).unwrap()),
        txt(SENTINEL),
        "centinela pisado"
    );
    assert_eq!(sb.outside(), before);
}

#[test]
fn load_with_parent_dir_key_does_not_read_outside_the_store() {
    let sb = Sandbox::new();
    sb.sentinel("escaped.bin");
    let store = AgentSnapshotStore::new(sb.store_dir());
    assert_ne!(
        store.load("../escaped.bin").ok().map(|b| txt(&b)),
        Some(txt(SENTINEL)),
        "leyó un fichero fuera del store"
    );
}

#[test]
fn save_with_absolute_key_does_not_write_outside_the_store() {
    let sb = Sandbox::new();
    let sentinel = sb.sentinel("absolute.bin");
    let mut store = AgentSnapshotStore::new(sb.store_dir());
    let before = sb.outside();
    let _ = store.save(&abs(&sentinel), b"payload");
    assert_eq!(
        txt(&fs::read(&sentinel).unwrap()),
        txt(SENTINEL),
        "centinela pisado"
    );
    assert_eq!(sb.outside(), before);
}

#[test]
fn load_with_absolute_key_does_not_read_outside_the_store() {
    let sb = Sandbox::new();
    let sentinel = sb.sentinel("absolute.bin");
    let store = AgentSnapshotStore::new(sb.store_dir());
    assert_ne!(
        store.load(&abs(&sentinel)).ok().map(|b| txt(&b)),
        Some(txt(SENTINEL)),
        "leyó un fichero fuera del store"
    );
}

// ---------- colisión con el nombre de un tick (rojo en 3ec48b8) ----------

#[test]
fn free_key_named_like_a_tick_file_does_not_overwrite_numeric_snapshot() {
    let sb = Sandbox::new();
    let mut store = AgentSnapshotStore::new(sb.store_dir());
    store.save("3", b"tick three").unwrap();
    let _ = store.save("tick-3.bin", b"free key");
    assert_eq!(txt(&store.load("3").unwrap()), "tick three");
    assert_eq!(txt(&store.load_tick(3).unwrap()), "tick three");
}

// ---------- controles del contrato vigente (verde) ----------

#[test]
fn plain_free_keys_round_trip_inside_the_store_and_survive_restart() {
    let sb = Sandbox::new();
    let mut store = AgentSnapshotStore::new(sb.store_dir());
    let before = sb.outside();
    store.save("checkpoint-a", b"A").unwrap();
    store.save("notes.bin", b"N").unwrap();
    store.save("checkpoint-a", b"A2").unwrap();
    assert_eq!(store.load("checkpoint-a").unwrap(), b"A2");
    drop(store);
    let store = AgentSnapshotStore::new(sb.store_dir());
    assert_eq!(store.load("notes.bin").unwrap(), b"N");
    assert_eq!(store.load("checkpoint-a").unwrap(), b"A2");
    assert!(store.load("missing").is_err());
    // Una clave libre no es un tick.
    assert_eq!(store.latest_tick(), None);
    assert_eq!(sb.outside(), before);
}

#[test]
fn numeric_keys_alias_ticks_including_leading_zeros() {
    let sb = Sandbox::new();
    let mut store = AgentSnapshotStore::new(sb.store_dir());
    store.save("3", b"three").unwrap();
    assert_eq!(store.load("003").unwrap(), b"three");
    assert_eq!(store.load_tick(3).unwrap(), b"three");
    store.save("007", b"seven").unwrap();
    assert_eq!(store.load("7").unwrap(), b"seven");
    assert_eq!(store.latest_tick(), Some(7));
}
