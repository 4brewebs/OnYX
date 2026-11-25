//! `SDD-05` TEST-05.2: caché incremental persistente `.onyx/scent.idx` (formato `ONYXSCI1`).

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use onyx_scent::{DistillerConfig, TOON_HEADER, ToonPageOptions, WorkspaceIndex, decode_toon};

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "onyx_scent_sdd05_cache_{prefix}_{}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn seed(&self) -> usize {
        fs::create_dir_all(self.0.join("src")).unwrap();
        fs::write(
            self.0.join("src/engine.rs"),
            "pub fn entry() -> i32 { core() }\npub fn core() -> i32 { 7 }\n",
        )
        .unwrap();
        fs::write(
            self.0.join("src/service.ts"),
            "export function serve(req: string): string { return wrap(req) }\nexport function wrap(raw: string): string { return raw }\n",
        )
        .unwrap();
        fs::write(
            self.0.join("app.go"),
            "package main\n\nfunc Boot() int {\n    return tune()\n}\n\nfunc tune() int {\n    return 3\n}\n",
        )
        .unwrap();
        3
    }

    fn idx_path(&self) -> PathBuf {
        self.0.join(".onyx").join("scent.idx")
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn snapshot_outputs(idx: &WorkspaceIndex) -> (String, String, String) {
    let opts = ToonPageOptions::default();
    (
        idx.outline_toon(opts),
        idx.impact_toon("core", &DistillerConfig::default(), opts)
            .expect("impact must resolve"),
        idx.references_toon("wrap", opts)
            .expect("references must resolve"),
    )
}

#[test]
fn cold_start_builds_cache_and_reload_is_bit_identical() {
    let ws = TempWorkspace::new("cold");
    let total = ws.seed();

    let mut first = WorkspaceIndex::new();
    let s1 = first.index_workspace_with_cache(&ws.0).unwrap();
    assert_eq!(s1.total_files, total);
    assert_eq!(s1.reparsed_files, total);
    assert_eq!(s1.cache_hits, 0);
    assert!(
        ws.idx_path().exists(),
        "cold start must write .onyx/scent.idx"
    );
    let first_outputs = snapshot_outputs(&first);
    for out in [&first_outputs.0, &first_outputs.1, &first_outputs.2] {
        assert!(out.starts_with(TOON_HEADER), "{out}");
        decode_toon(out).expect("valid @TOON v1");
    }

    // Nuevo índice, recarga desde disco: 0 reparse, todo hit, salida bit-idéntica.
    let mut second = WorkspaceIndex::new();
    let s2 = second.index_workspace_with_cache(&ws.0).unwrap();
    assert_eq!(s2.reparsed_files, 0, "reload must not reparse");
    assert_eq!(s2.cache_hits, total, "reload must be all cache hits");
    assert_eq!(
        snapshot_outputs(&second),
        first_outputs,
        "outputs must be bit-identical"
    );
}

#[test]
fn stat_only_fast_path_reloads_under_20ms_on_unchanged_workspace() {
    let ws = TempWorkspace::new("fastpath");
    let total = ws.seed();

    let mut first = WorkspaceIndex::new();
    first.index_workspace_with_cache(&ws.0).unwrap();

    // La regla anti-racy exige mtime ≥ 2 s antiguo para el stat-only hit.
    std::thread::sleep(Duration::from_millis(2_100));

    let mut reloaded = WorkspaceIndex::new();
    let started = Instant::now();
    let stats = reloaded.index_workspace_with_cache(&ws.0).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(stats.reparsed_files, 0);
    assert_eq!(stats.cache_hits, total);
    assert!(
        elapsed < Duration::from_millis(20),
        "stat-only reload must be < 20 ms, took {elapsed:?}"
    );
}

#[test]
fn single_file_edit_reparses_only_that_file_even_with_same_size() {
    let ws = TempWorkspace::new("edit");
    let total = ws.seed();
    let ts = ws.0.join("src/service.ts");

    let mut first = WorkspaceIndex::new();
    first.index_workspace_with_cache(&ws.0).unwrap();

    // Edición del MISMO tamaño en bytes, dentro del mismo milisegundo: el hash
    // (no el stat) debe forzar el reparse del único archivo cambiado.
    let original = fs::read_to_string(&ts).unwrap();
    let mut changed = original.clone();
    changed.replace_range(16..17, "X"); // `serve` -> `Xerve` (igual longitud)
    assert_eq!(changed.len(), original.len());
    fs::write(&ts, &changed).unwrap();

    let mut reloaded = WorkspaceIndex::new();
    let stats = reloaded.index_workspace_with_cache(&ws.0).unwrap();
    assert_eq!(stats.reparsed_files, 1, "only the edited file may reparse");
    assert_eq!(stats.cache_hits, total - 1);

    let outline = reloaded.outline_toon(ToonPageOptions::default());
    assert!(outline.contains("Xerve"), "{outline}");
    assert!(!outline.contains("src/service.ts::serve "), "{outline}");
}

#[test]
fn removed_file_is_purged_from_cache_and_graph() {
    let ws = TempWorkspace::new("purge");
    let total = ws.seed();

    let mut first = WorkspaceIndex::new();
    first.index_workspace_with_cache(&ws.0).unwrap();

    fs::remove_file(ws.0.join("app.go")).unwrap();

    let mut reloaded = WorkspaceIndex::new();
    let stats = reloaded.index_workspace_with_cache(&ws.0).unwrap();
    assert_eq!(stats.total_files, total - 1);
    assert_eq!(stats.reparsed_files, 0);
    assert_eq!(stats.cache_hits, total - 1);
    let outline = reloaded.outline_toon(ToonPageOptions::default());
    assert!(!outline.contains("app.go::Boot"), "{outline}");

    // La caché persistida ya no contiene el archivo borrado.
    let mut third = WorkspaceIndex::new();
    let s3 = third.index_workspace_with_cache(&ws.0).unwrap();
    assert_eq!(s3.total_files, total - 1);
    assert_eq!(s3.cache_hits, total - 1);
    assert_eq!(s3.reparsed_files, 0);
}

#[test]
fn corrupt_truncated_and_foreign_caches_rebuild_cleanly() {
    let ws = TempWorkspace::new("corrupt");
    let total = ws.seed();

    let mut first = WorkspaceIndex::new();
    first.index_workspace_with_cache(&ws.0).unwrap();
    let expected = snapshot_outputs(&first);
    let payload = fs::read(ws.idx_path()).unwrap();

    // 1) Bit flip → checksum inválido.
    let mut flipped = payload.clone();
    flipped[32] ^= 0xff;
    fs::write(ws.idx_path(), &flipped).unwrap();
    let mut idx = WorkspaceIndex::new();
    let stats = idx.index_workspace_with_cache(&ws.0).unwrap();
    assert_eq!(
        stats.reparsed_files, total,
        "corrupt cache must rebuild from scratch"
    );
    assert_eq!(snapshot_outputs(&idx), expected);

    // 2) Truncado.
    fs::write(ws.idx_path(), &payload[..payload.len() / 3]).unwrap();
    let mut idx = WorkspaceIndex::new();
    let stats = idx.index_workspace_with_cache(&ws.0).unwrap();
    assert_eq!(stats.reparsed_files, total);
    assert_eq!(snapshot_outputs(&idx), expected);

    // 3) Magic/versión foránea.
    let mut foreign = b"ONYXSCXX".to_vec();
    foreign.extend_from_slice(&payload[8..]);
    fs::write(ws.idx_path(), &foreign).unwrap();
    let mut idx = WorkspaceIndex::new();
    let stats = idx.index_workspace_with_cache(&ws.0).unwrap();
    assert_eq!(stats.reparsed_files, total);
    assert_eq!(snapshot_outputs(&idx), expected);
}

#[test]
fn cache_save_maintains_gitignore_and_preserves_existing_rules() {
    let ws = TempWorkspace::new("gitignore");
    ws.seed();

    // Regla preexistente que DEBE conservarse.
    fs::create_dir_all(ws.0.join(".onyx")).unwrap();
    fs::write(ws.0.join(".onyx/.gitignore"), "vault/\n").unwrap();

    let mut idx = WorkspaceIndex::new();
    idx.index_workspace_with_cache(&ws.0).unwrap();

    let gitignore = fs::read_to_string(ws.0.join(".onyx/.gitignore")).unwrap();
    assert!(
        gitignore.lines().any(|l| l.trim() == "vault/"),
        "{gitignore}"
    );
    assert!(
        gitignore.lines().any(|l| l.trim() == "scent.idx"),
        "{gitignore}"
    );
    assert!(
        gitignore.lines().any(|l| l.trim() == "scent.idx.tmp*"),
        "{gitignore}"
    );

    // Sin archivos temporales huérfanos tras el rename atómico.
    let leftovers: Vec<String> = fs::read_dir(ws.0.join(".onyx"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("scent.idx.tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
