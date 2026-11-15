//! Formato de claves de `AgentSnapshotStore` (fix e4088da): claves libres no
//! válidas son `OnyxError::Format` antes de tocar disco; `tick-N.bin` está
//! reservado también para `load`; toda clave que parsea como `u64` (`003`,
//! `+3`) es el tick. Cada prueba usa su propio directorio temporal exclusivo y
//! borra solo ese directorio.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use onyx_soul_agents::{AgentSnapshotStore, OnyxError, SnapshotStore};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// `root/` propio con `root/store/` como directorio del store. Creado en
/// exclusiva (falla si existía) y borrado al soltarse.
struct Sandbox {
    root: PathBuf,
}
impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "onyx-forja-snapfmt-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("directorio temporal propio y nuevo");
        Self { root }
    }
    fn store_dir(&self) -> PathBuf {
        self.root.join("store")
    }
    /// Todo el árbol bajo `root` (rutas relativas y contenido).
    fn tree(&self) -> Vec<(String, Option<Vec<u8>>)> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Option<Vec<u8>>)>) {
            for e in fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                if p.is_dir() {
                    out.push((rel, None));
                    walk(base, &p, out);
                } else {
                    out.push((rel, Some(fs::read(&p).unwrap())));
                }
            }
        }
        let mut v = Vec::new();
        walk(&self.root, &self.root, &mut v);
        v.sort();
        v
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn is_format<T: std::fmt::Debug>(r: &Result<T, OnyxError>) -> bool {
    matches!(r, Err(OnyxError::Format(_)))
}

/// Store con tick 3, una clave libre válida, `sub/x.bin` preexistente y un
/// centinela fuera del store.
fn seeded(sb: &Sandbox) -> AgentSnapshotStore {
    let mut store = AgentSnapshotStore::new(sb.store_dir());
    store.save("3", b"tick-tres").unwrap();
    store.save("libre.bin", b"libre").unwrap();
    fs::create_dir(sb.store_dir().join("sub")).unwrap();
    fs::write(
        sb.store_dir().join("sub").join("x.bin"),
        b"en-subdirectorio",
    )
    .unwrap();
    fs::write(sb.root.join("escaped.bin"), b"centinela").unwrap();
    store
}

fn bad_keys(sb: &Sandbox) -> Vec<String> {
    let abs = sb.root.join("escaped.bin");
    vec![
        String::new(),
        ".".into(),
        "..".into(),
        "./libre.bin".into(),
        "libre.bin/".into(),
        "sub/x.bin".into(),
        "../escaped.bin".into(),
        abs.to_str().unwrap().into(),
        "tick-3.bin".into(),
    ]
}

#[test]
fn invalid_free_keys_on_save_are_format_errors_and_touch_nothing() {
    let sb = Sandbox::new();
    let mut store = seeded(&sb);
    let before = sb.tree();
    for key in bad_keys(&sb) {
        let r = store.save(&key, b"payload");
        assert!(is_format(&r), "save({key:?}) = {r:?}");
        assert_eq!(sb.tree(), before, "save({key:?}) tocó disco");
    }
    assert_eq!(store.load("3").unwrap(), b"tick-tres");
}

#[test]
fn invalid_free_keys_on_load_are_format_errors_and_read_nothing() {
    let sb = Sandbox::new();
    let store = seeded(&sb);
    let before = sb.tree();
    for key in bad_keys(&sb) {
        let r = store.load(&key);
        assert!(is_format(&r), "load({key:?}) = {r:?}");
    }
    assert_eq!(sb.tree(), before);
}

#[test]
fn loading_tick_3_bin_as_a_free_key_does_not_return_the_numeric_snapshot() {
    let sb = Sandbox::new();
    let mut store = AgentSnapshotStore::new(sb.store_dir());
    store.save("3", b"tick-tres").unwrap();
    let r = store.load("tick-3.bin");
    assert!(is_format(&r), "{r:?}");
    assert_eq!(store.load("3").unwrap(), b"tick-tres");
}

#[test]
fn plus_sign_key_is_the_same_tick_as_3_and_003() {
    let sb = Sandbox::new();
    let mut store = AgentSnapshotStore::new(sb.store_dir());
    store.save("+3", b"con-signo").unwrap();
    assert_eq!(store.load("3").unwrap(), b"con-signo");
    assert_eq!(store.load("003").unwrap(), b"con-signo");
    assert_eq!(store.latest_tick(), Some(3));
    store.save("003", b"con-ceros").unwrap();
    assert_eq!(store.load("+3").unwrap(), b"con-ceros");
    let names: Vec<_> = sb.tree().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["store", "store/tick-3.bin"], "sin fichero '+3'");
}

#[test]
fn non_canonical_tick_like_names_remain_free_keys_and_survive_restart() {
    let sb = Sandbox::new();
    let mut store = AgentSnapshotStore::new(sb.store_dir());
    store.save("3", b"tick-tres").unwrap();
    for key in [
        "tick-03.bin",
        "tick-3.bin.bak",
        "tick-3",
        "snap.bin",
        "+3.bin",
    ] {
        store.save(key, key.as_bytes()).unwrap();
    }
    let reopened = AgentSnapshotStore::new(sb.store_dir());
    for key in [
        "tick-03.bin",
        "tick-3.bin.bak",
        "tick-3",
        "snap.bin",
        "+3.bin",
    ] {
        assert_eq!(reopened.load(key).unwrap(), key.as_bytes(), "{key}");
    }
    assert_eq!(reopened.load("3").unwrap(), b"tick-tres");
    assert_eq!(
        reopened.latest_tick(),
        Some(3),
        "libres no cuentan como ticks"
    );
}
