//! Regresiones de `onyx-scent` (hallazgos de Colorado, `harness-scent.md`; numeración de
//! Lince en `SDD-fixes.md`, FIX-68…FIX-81). Solo API pública del crate.
//!
//! - Cada bug reproducible va con `#[ignore = "BUG: FIX-NN …"]` y, al lado, un control verde.
//! - FIX-68 aborta el proceso (stack overflow → SIGABRT): el caso se ejecuta en un proceso
//!   hijo relanzando este mismo binario de test (`current_exe`), nunca en un hilo.
//! - FIX-72 (el binario `scent` no se compila en CI) y FIX-78 viven en `scent_cli.rs`, bajo
//!   `#![cfg(feature = "cli")]`: el workflow actual (`cargo test --workspace`) no los compila.
//! - FIX-81 vive en `onyx-workspace/tests/scent_tools.rs`, bajo `feature = "scent"`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use onyx_scent::{
    DistillerConfig, Edge, RelationKind, ScentArena, ToonPageOptions, ToonRow, WorkspaceIndex,
    decode_toon, distill_heat, render_toon,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "onyx_scent_forja_{prefix}_{}_{n}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(fs::canonicalize(&dir).unwrap())
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn outline(idx: &WorkspaceIndex) -> String {
    idx.outline_toon(ToonPageOptions::default())
}

fn set_mtime_old(path: &Path) {
    let old = SystemTime::now() - Duration::from_secs(3600);
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(old)
        .unwrap();
}

// ───────────── FIX-68: anidamiento profundo (proceso hijo) ─────────────

const CHILD_ENV: &str = "FORJA_SCENT_DEPTH";

/// Cuerpo que solo trabaja dentro del proceso hijo (con `FORJA_SCENT_DEPTH`); en una
/// ejecución normal no hace nada.
#[test]
fn deep_nesting_child_entrypoint() {
    let Ok(depth) = std::env::var(CHILD_ENV) else {
        return;
    };
    let depth: usize = depth.parse().unwrap();
    let dir = TempDir::new("deep");
    let src = format!(
        "fn f() {{ let x = {}1{}; }}\npub fn ok() {{}}\n",
        "(".repeat(depth),
        ")".repeat(depth)
    );
    fs::write(dir.path().join("a.rs"), src).unwrap();
    let mut idx = WorkspaceIndex::new();
    // `Ok` o `Err` acotado: ambos valen; lo único prohibido es abortar.
    let r = idx.index_workspace(dir.path());
    println!("FORJA depth={depth} result_ok={}", r.is_ok());
}

/// Relanza este binario con solo `deep_nesting_child_entrypoint` y pila de 8 MiB (la de
/// los criterios de aceptación). Devuelve (éxito, stderr).
fn run_deep_child(depth: usize) -> (bool, String) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "deep_nesting_child_entrypoint",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, depth.to_string())
        .env("RUST_MIN_STACK", "8388608")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success() || !stdout.contains("FORJA depth="),
        "child printed a result but failed: {stderr}"
    );
    (
        out.status.success() && stdout.contains(&format!("FORJA depth={depth}")),
        stderr,
    )
}

#[test]
fn moderate_nesting_indexes_without_abort_in_child_process() {
    let (ok, stderr) = run_deep_child(200);
    assert!(ok, "depth 200 must not abort: {stderr}");
}

#[test]
#[ignore = "BUG: FIX-68 visit/visit_children recursivos desbordan la pila con ~20 000 paréntesis anidados en un .rs y abortan el proceso entero (SIGABRT) (onyx-scent/src/lang.rs:182-198, :224)"]
fn deep_nesting_does_not_abort_the_process() {
    for depth in [20_000, 100_000] {
        let (ok, stderr) = run_deep_child(depth);
        assert!(
            ok,
            "depth {depth} aborted the process: {}",
            stderr.lines().rev().take(3).collect::<Vec<_>>().join(" / ")
        );
    }
}

// ───────────── FIX-69: no UTF-8 (contenido y nombre) ─────────────

#[test]
fn utf8_workspace_indexes_every_file() {
    let dir = TempDir::new("utf8_ok");
    fs::write(dir.path().join("good.rs"), "pub fn good_symbol() {}\n").unwrap();
    fs::write(dir.path().join("ñandú.rs"), "pub fn otro() {}\n").unwrap();
    let mut idx = WorkspaceIndex::new();
    let stats = idx.index_workspace(dir.path()).unwrap();
    assert_eq!(stats.total_files, 2);
    let out = outline(&idx);
    assert!(out.contains("good_symbol") && out.contains("otro"), "{out}");
}

#[test]
#[ignore = "BUG: FIX-69 un .rs con contenido no UTF-8 tumba el índice entero con Err(Parse) y se pierde good.rs (onyx-scent/src/index.rs:290-293)"]
fn non_utf8_content_does_not_drop_the_rest_of_the_index() {
    let dir = TempDir::new("utf8_bad_content");
    fs::write(dir.path().join("good.rs"), "pub fn good_symbol() {}\n").unwrap();
    fs::write(dir.path().join("bad.rs"), [0xffu8, 0xfe, b'f', b'n']).unwrap();
    let mut idx = WorkspaceIndex::new();
    let r = idx.index_workspace(dir.path());
    assert!(r.is_ok(), "one non-UTF-8 file aborted the index: {r:?}");
    assert!(outline(&idx).contains("good_symbol"));
}

#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-69 un nombre de fichero no UTF-8 se normaliza con to_string_lossy y el join posterior no existe: Err(Io) tumba el índice entero (onyx-scent/src/index.rs:879, :237)"]
fn non_utf8_file_name_does_not_drop_the_rest_of_the_index() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let dir = TempDir::new("utf8_bad_name");
    fs::write(dir.path().join("good.rs"), "pub fn good_symbol() {}\n").unwrap();
    let name = OsString::from_vec(vec![b'a', 0xff, b'.', b'r', b's']);
    fs::write(dir.path().join(name), "pub fn x() {}\n").unwrap();
    let mut idx = WorkspaceIndex::new();
    let r = idx.index_workspace(dir.path());
    assert!(
        r.is_ok(),
        "one non-UTF-8 file name aborted the index: {r:?}"
    );
    assert!(outline(&idx).contains("good_symbol"));
}

// ───────────── FIX-70: round-trip TOON ─────────────

fn row(subject: &str, object: &str) -> ToonRow {
    ToonRow {
        subject: subject.into(),
        predicate: "fn".into(),
        object: object.into(),
        meta: "kind:fn".into(),
    }
}

/// Regresión P0-4 (verificada correcta): `|`, `\n`, `\r` y `\\` se escapan y redondean.
#[test]
fn toon_roundtrip_preserves_pipes_newlines_and_backslashes() {
    let rows = vec![
        row("we\nird|n.rs::f", "fn f(a: A|B) -> \\x"),
        row("c\r.rs::g", "fn g()"),
    ];
    let out = render_toon(&rows, ToonPageOptions::default());
    assert_eq!(decode_toon(&out).unwrap(), rows, "{out}");
}

#[test]
#[ignore = "BUG: FIX-70 decode_toon no invierte render_toon: un subject con # inicial se toma por comentario y los campos se recortan con trim (onyx-scent/src/toon.rs:109, :120-123)"]
fn toon_roundtrip_preserves_leading_hash_and_spaces() {
    let rows = vec![row("#dir/a.rs::f", "  fn f()  ")];
    let out = render_toon(&rows, ToonPageOptions::default());
    assert_eq!(decode_toon(&out).unwrap(), rows, "{out}");
}

// ───────────── FIX-71: `.onyx` symlink fuera de la raíz ─────────────

#[test]
fn cache_is_written_under_regular_onyx_dir() {
    let dir = TempDir::new("onyx_regular");
    fs::write(dir.path().join("a.rs"), "pub fn a() {}\n").unwrap();
    let mut idx = WorkspaceIndex::new();
    idx.index_workspace_with_cache(dir.path()).unwrap();
    assert!(dir.path().join(".onyx/scent.idx").is_file());
    assert!(dir.path().join(".onyx/.gitignore").is_file());
}

#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-71 si .onyx es un symlink fuera de la raíz, save_cache crea scent.idx y .gitignore fuera del workspace (onyx-scent/src/index.rs:133-138; cache.rs:32-52)"]
fn symlinked_onyx_dir_does_not_write_outside_root() {
    let dir = TempDir::new("onyx_symlink");
    let ws = dir.path().join("ws");
    let out = dir.path().join("outside");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&out).unwrap();
    fs::write(ws.join("a.rs"), "pub fn a() {}\n").unwrap();
    std::os::unix::fs::symlink(&out, ws.join(".onyx")).unwrap();
    let mut idx = WorkspaceIndex::new();
    let _ = idx.index_workspace_with_cache(&ws);
    let written: Vec<_> = fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        written.is_empty(),
        "files written outside the root: {written:?}"
    );
}

// ───────────── FIX-73: determinismo byte a byte del `.idx` ─────────────

fn two_saves(prefix: &str) -> (Vec<u8>, Vec<u8>) {
    let dir = TempDir::new(prefix);
    fs::write(dir.path().join("a.rs"), "pub fn a() { b() }\nfn b() {}\n").unwrap();
    let mut idx = WorkspaceIndex::new();
    idx.index_workspace(dir.path()).unwrap();
    let p1 = dir.path().join("one.idx");
    let p2 = dir.path().join("two.idx");
    idx.save_cache(&p1).unwrap();
    // saved_at_secs tiene resolución de segundos.
    std::thread::sleep(Duration::from_millis(1100));
    idx.save_cache(&p2).unwrap();
    (fs::read(p1).unwrap(), fs::read(p2).unwrap())
}

/// Control: salvo los bytes 12..20 (`saved_at_secs`) y el checksum final, el payload es
/// estable a igualdad de contenido.
#[test]
fn cache_payload_is_stable_except_saved_at_field() {
    let (a, b) = two_saves("idx_masked");
    assert_eq!(a.len(), b.len());
    let n = a.len();
    assert_eq!(a[..12], b[..12]);
    assert_eq!(a[20..n - 32], b[20..n - 32]);
}

#[test]
#[ignore = "BUG: FIX-73 saved_at_secs se escribe en el .idx y nunca se lee: dos guardados del mismo índice no son byte a byte iguales (onyx-scent/src/cache.rs:58-65, :130)"]
fn cache_payload_is_byte_identical_for_identical_index() {
    let (a, b) = two_saves("idx_raw");
    assert!(a == b, "two saves of the same index differ");
}

// ───────────── FIX-74 / FIX-75: distiller ─────────────

fn call_pair() -> (TempDir, WorkspaceIndex, usize) {
    let dir = TempDir::new("distill");
    fs::write(
        dir.path().join("a.rs"),
        "pub fn caller_fn() { callee_fn() }\npub fn callee_fn() {}\n",
    )
    .unwrap();
    let mut idx = WorkspaceIndex::new();
    idx.index_workspace(dir.path()).unwrap();
    let epicenter = idx.resolve_targets("callee_fn")[0];
    (dir, idx, epicenter)
}

fn neighbor_vs_epicenter(cfg: &DistillerConfig) -> (u16, u16) {
    let (_dir, idx, epi) = call_pair();
    let hits = distill_heat(&idx.arena, &[epi], cfg);
    let e = hits.iter().find(|h| h.node_id == epi).unwrap();
    let n = hits
        .iter()
        .find(|h| h.node_id != epi)
        .expect("caller must be reached");
    (e.temperature_bp, n.temperature_bp)
}

#[test]
fn heat_decays_with_in_range_decay() {
    let (epi, nb) = neighbor_vs_epicenter(&DistillerConfig {
        iterations: 3,
        decay_bp: 10_000,
        threshold_bp: 0,
    });
    assert!(
        nb < epi,
        "neighbor {nb} must be colder than epicenter {epi}"
    );
}

#[test]
#[ignore = "BUG: FIX-74 decay_bp fuera del rango documentado 0..=10_000 no se valida: con 60000 el vecino queda a la misma temperatura que el epicentro (onyx-scent/src/distiller.rs:12)"]
fn out_of_range_decay_does_not_equalize_heat() {
    let (epi, nb) = neighbor_vs_epicenter(&DistillerConfig {
        iterations: 3,
        decay_bp: 60_000,
        threshold_bp: 0,
    });
    assert!(nb < epi, "neighbor {nb} as hot as epicenter {epi}");
}

#[test]
fn distill_on_well_formed_arena_does_not_panic() {
    let (_dir, idx, epi) = call_pair();
    let hits = distill_heat(&idx.arena, &[epi, 999], &DistillerConfig::default());
    assert!(hits.iter().any(|h| h.node_id == epi));
}

#[test]
#[ignore = "BUG: FIX-75 una arista con source_id/target_id fuera de nodes provoca index out of bounds en distill_heat (onyx-scent/src/distiller.rs:73, :78)"]
fn dangling_edge_does_not_panic() {
    let (_dir, idx, epi) = call_pair();
    let mut arena = ScentArena::new();
    arena.nodes = idx.arena.nodes.clone();
    arena.edges = idx.arena.edges.clone();
    arena.edges.push(Edge {
        source_id: 5,
        target_id: 9,
        kind: RelationKind::Calls,
        weight_bp: 5000,
    });
    let r = std::panic::catch_unwind(|| distill_heat(&arena, &[epi], &DistillerConfig::default()));
    let hits = r.expect("distill_heat panicked on a dangling edge");
    assert!(hits.iter().any(|h| h.node_id == epi));
}

// ───────────── FIX-76: tope de bytes de la página TOON ─────────────

#[test]
fn toon_paginates_small_rows_within_max_bytes() {
    let rows: Vec<ToonRow> = (0..200)
        .map(|i| row(&format!("a.rs::f{i}"), &format!("fn f{i}()")))
        .collect();
    let out = render_toon(
        &rows,
        ToonPageOptions {
            max_bytes: 512,
            ..ToonPageOptions::default()
        },
    );
    assert!(out.len() <= 512, "len {}", out.len());
    assert!(out.contains("# page: next_cursor="), "{out}");
}

#[test]
#[ignore = "BUG: FIX-76 la primera fila TOON se emite aunque supere max_bytes (emitted > 0): 100 kB con max_bytes=512 (onyx-scent/src/toon.rs:70)"]
fn first_toon_row_respects_max_bytes() {
    let rows = vec![row("s", &"x".repeat(100_000))];
    let out = render_toon(
        &rows,
        ToonPageOptions {
            max_bytes: 512,
            ..ToonPageOptions::default()
        },
    );
    assert!(
        out.len() <= 1024,
        "page of {} bytes with max_bytes=512",
        out.len()
    );
}

// ───────────── FIX-77: herencia en Python y Java ─────────────

fn references_of(files: &[(&str, &str)], target: &str) -> Vec<ToonRow> {
    let dir = TempDir::new("refs");
    for (name, src) in files {
        fs::write(dir.path().join(name), src).unwrap();
    }
    let mut idx = WorkspaceIndex::new();
    idx.index_workspace(dir.path()).unwrap();
    let out = idx
        .references_toon(target, ToonPageOptions::default())
        .unwrap();
    decode_toon(&out)
        .unwrap()
        .into_iter()
        .filter(|r| r.predicate != "defined_at")
        .collect()
}

fn mentions(rows: &[ToonRow], name: &str) -> bool {
    rows.iter()
        .any(|r| r.subject.contains(name) || r.object.contains(name) || r.meta.contains(name))
}

#[test]
fn typescript_class_heritage_yields_type_references() {
    let rows = references_of(
        &[
            ("b.ts", "export class Base {}\n"),
            ("c.ts", "export interface Contract {}\n"),
            (
                "a.ts",
                "export class Child extends Base implements Contract {}\n",
            ),
        ],
        "Child",
    );
    assert!(
        mentions(&rows, "Base") || mentions(&rows, "Contract"),
        "{rows:?}"
    );
}

#[test]
#[ignore = "BUG: FIX-77 visit_java no emite raw_type_refs: extends/implements no generan referencias (onyx-scent/src/lang.rs:571-657)"]
fn java_class_heritage_yields_type_references() {
    let rows = references_of(
        &[
            ("Base.java", "public class Base {}\n"),
            ("Contract.java", "public interface Contract {}\n"),
            (
                "Child.java",
                "public class Child extends Base implements Contract {}\n",
            ),
        ],
        "Child",
    );
    assert!(
        mentions(&rows, "Base") && mentions(&rows, "Contract"),
        "{rows:?}"
    );
}

#[test]
#[ignore = "BUG: FIX-77 visit_python no emite raw_type_refs: las clases base no generan referencias (onyx-scent/src/lang.rs:466-570)"]
fn python_class_bases_yield_type_references() {
    let rows = references_of(
        &[
            ("base.py", "class Base:\n    pass\n"),
            ("child.py", "class Child(Base):\n    pass\n"),
        ],
        "Child",
    );
    assert!(mentions(&rows, "Base"), "{rows:?}");
}

// ───────────── FIX-79: `target` / `node_modules` anidados ─────────────

#[test]
fn root_target_and_node_modules_are_ignored() {
    let dir = TempDir::new("ignored_root");
    fs::create_dir_all(dir.path().join("target/debug")).unwrap();
    fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
    fs::write(dir.path().join("target/debug/gen.rs"), "pub fn gen() {}\n").unwrap();
    fs::write(
        dir.path().join("node_modules/x/i.js"),
        "function dep() {}\n",
    )
    .unwrap();
    fs::write(dir.path().join("lib.rs"), "pub fn real() {}\n").unwrap();
    let mut idx = WorkspaceIndex::new();
    let stats = idx.index_workspace(dir.path()).unwrap();
    assert_eq!(stats.total_files, 1, "{}", outline(&idx));
}

#[test]
#[ignore = "BUG: FIX-79 IGNORED_DIRS se aplica por nombre de componente a cualquier profundidad: src/target/mod.rs o pkg/node_modules no se indexan (onyx-scent/src/index.rs:834; decisión de diseño pendiente, SDD-fixes §7)"]
fn nested_dirs_named_target_or_node_modules_are_indexed() {
    let dir = TempDir::new("ignored_nested");
    fs::create_dir_all(dir.path().join("src/target")).unwrap();
    fs::create_dir_all(dir.path().join("web/node_modules")).unwrap();
    fs::write(dir.path().join("src/target/mod.rs"), "pub fn hidden() {}\n").unwrap();
    fs::write(
        dir.path().join("web/node_modules/local.ts"),
        "export function vendored() {}\n",
    )
    .unwrap();
    let mut idx = WorkspaceIndex::new();
    let stats = idx.index_workspace(dir.path()).unwrap();
    assert_eq!(stats.total_files, 2, "{}", outline(&idx));
}

// ───────────── FIX-80: caché falsificada con checksum válido ─────────────

/// Prepara un workspace con caché y devuelve (dir, bytes del `.idx`). `a.rs` tiene mtime
/// antiguo para que la recarga tome el camino stat-only.
fn workspace_with_cache() -> (TempDir, Vec<u8>) {
    let dir = TempDir::new("forged");
    let f = dir.path().join("a.rs");
    fs::write(&f, "pub fn real_fn() {}\n").unwrap();
    set_mtime_old(&f);
    let mut idx = WorkspaceIndex::new();
    idx.index_workspace_with_cache(dir.path()).unwrap();
    let bytes = fs::read(dir.path().join(".onyx/scent.idx")).unwrap();
    (dir, bytes)
}

fn replace_all(bytes: &mut [u8], from: &[u8], to: &[u8]) -> usize {
    assert_eq!(from.len(), to.len());
    let mut n = 0;
    let mut i = 0;
    while i + from.len() <= bytes.len() {
        if &bytes[i..i + from.len()] == from {
            bytes[i..i + from.len()].copy_from_slice(to);
            n += 1;
            i += from.len();
        } else {
            i += 1;
        }
    }
    n
}

fn reload_outline(dir: &Path) -> String {
    let mut idx = WorkspaceIndex::new();
    idx.index_workspace_with_cache(dir).unwrap();
    outline(&idx)
}

/// Control: una caché retocada sin recalcular el checksum se descarta y se reindexa.
#[test]
fn tampered_cache_with_stale_checksum_is_rejected() {
    let (dir, mut bytes) = workspace_with_cache();
    assert!(replace_all(&mut bytes, b"real_fn", b"evil_fn") > 0);
    fs::write(dir.path().join(".onyx/scent.idx"), &bytes).unwrap();
    let out = reload_outline(dir.path());
    assert!(out.contains("real_fn") && !out.contains("evil_fn"), "{out}");
}

#[test]
#[ignore = "BUG: FIX-80 una caché .idx falsificada con checksum blake3 recalculado (sin clave) y stat coincidente se acepta sin rehash e inyecta símbolos inexistentes (onyx-scent/src/index.rs:259-261; cache.rs:118-125; decisión de diseño §7)"]
fn forged_cache_with_valid_checksum_does_not_inject_symbols() {
    let (dir, mut bytes) = workspace_with_cache();
    assert!(replace_all(&mut bytes, b"real_fn", b"evil_fn") > 0);
    let n = bytes.len();
    let sum = *blake3::hash(&bytes[..n - 32]).as_bytes();
    bytes[n - 32..].copy_from_slice(&sum);
    fs::write(dir.path().join(".onyx/scent.idx"), &bytes).unwrap();
    let out = reload_outline(dir.path());
    assert!(
        out.contains("real_fn") && !out.contains("evil_fn"),
        "forged cache accepted:\n{out}"
    );
}
