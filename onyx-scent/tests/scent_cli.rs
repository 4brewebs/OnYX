//! Binario `scent` (FIX-72 / FIX-78). Requiere `--features cli`:
//!
//! ```text
//! cargo test -p onyx-scent --features cli --test scent_cli
//! ```
//!
//! FIX-72: el workflow actual (`ci.yml:29`, `cargo test --workspace --locked`) no activa la
//! feature `cli`, así que este fichero NO se compila en CI hasta que se añada ese paso
//! (requiere OK de Lucas; no se toca CI aquí).
#![cfg(feature = "cli")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "onyx_scent_cli_{prefix}_{}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn workspace() -> TempDir {
    let dir = TempDir::new("ws");
    fs::write(
        dir.0.join("lib.rs"),
        "pub fn entry_fn() { core_fn() }\npub fn core_fn() {}\n",
    )
    .unwrap();
    dir
}

fn scent(root: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_scent"))
        .args(args)
        .arg("--root")
        .arg(root)
        .arg("--no-cache")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Control: con `--max-hops 1` el impacto de `core_fn` llega a su llamador.
#[test]
fn impact_with_one_hop_reaches_the_caller() {
    let ws = workspace();
    let (ok, out) = scent(&ws.0, &["impact", "--target", "core_fn", "--max-hops", "1"]);
    assert!(ok && out.contains("entry_fn"), "{out}");
}

/// Control: `--max-rows 0` acaba igual que `--max-rows 1` (render_toon usa `max(1)`), que es
/// el mínimo del clamp del tool de workspace.
#[test]
fn outline_with_zero_rows_behaves_like_one_row() {
    let ws = workspace();
    let zero = scent(&ws.0, &["outline", "--max-rows", "0"]);
    let one = scent(&ws.0, &["outline", "--max-rows", "1"]);
    assert!(zero.0 && one.0);
    assert_eq!(zero.1, one.1);
}

#[test]
#[ignore = "BUG: FIX-78 el CLI pasa --max-hops 0 tal cual (iterations: 0, solo epicentros) en vez de acotar 1..=6 como el tool de workspace o salir con error (onyx-scent/src/bin/scent.rs; tools.rs:1760)"]
fn impact_with_zero_hops_is_clamped_or_rejected() {
    let ws = workspace();
    let (ok, out) = scent(&ws.0, &["impact", "--target", "core_fn", "--max-hops", "0"]);
    assert!(
        !ok || out.contains("entry_fn"),
        "--max-hops 0 accepted without clamp:\n{out}"
    );
}
