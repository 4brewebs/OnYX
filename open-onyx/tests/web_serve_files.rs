//! Caja negra de `GET /api/v1/files?q=` (autocompletado `@` del PWA).
//!
//! Contrato (`WebHub::search_files` + `collect_workspace_files`): devuelve como mucho 12
//! rutas **relativas** del índice del workspace que contienen `q` (sin distinguir
//! mayúsculas), ordenadas; nunca sigue enlaces simbólicos ni lista `.git`, el vault de
//! `.onyx` ni artefactos `*.tr`/`*.onyxpack`. La consulta es un filtro, no una ruta: las
//! secuencias de traversal no pueden sacar nada de fuera.

mod common;

use common::{ServeGuard, ServeOpts};
use std::path::Path;

fn files(srv: &ServeGuard, raw_query: &str) -> Vec<String> {
    let r = srv.get(
        &format!("/api/v1/files?q={raw_query}"),
        &[("Authorization", &srv.bearer())],
    );
    assert_eq!(r.status, 200, "q={raw_query}: {}", r.text());
    r.json()["files"]
        .as_array()
        .expect("files[]")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn prepare(ws: &Path) {
    let w = |rel: &str, body: &str| {
        let p = ws.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    w("src/main.rs", "fn main() {}");
    w("src/lib.rs", "");
    w("docs/Guía Ñandú.md", "unicode");
    w("README.md", "");
    w(".git/config", "[core]");
    w(".onyx/vault/run1/chat.tr", "");
    w("snapshots/snapshots.onyxpack", "");
    w("target/debug/build.log", "");
    w("node_modules/x/index.js", "");
    w("session.tr", "");
    for i in 0..30 {
        w(&format!("many/file_{i:02}.txt"), "");
    }
    // Fuera del workspace (hermano de `ws`), alcanzable solo por symlink.
    let outside = ws.parent().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("forja_outside_secret.txt"), "SECRET").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, ws.join("link_dir")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("forja_outside_secret.txt"),
            ws.join("link_file.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink("/etc", ws.join("etc_link")).unwrap();
    }
}

fn start() -> ServeGuard {
    ServeGuard::start_with(
        "files",
        ServeOpts {
            prepare: Some(&prepare),
            ..ServeOpts::default()
        },
    )
}

#[test]
fn file_search_is_a_relative_bounded_sorted_filter() {
    let srv = start();
    let all = files(&srv, "");
    assert_eq!(all.len(), 12, "como mucho 12 resultados: {all:?}");
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(all, sorted, "resultados ordenados");

    assert_eq!(files(&srv, "main"), vec!["src/main.rs".to_string()]);
    assert_eq!(
        files(&srv, "MAIN.RS"),
        vec!["src/main.rs".to_string()],
        "case-insensitive"
    );
    assert_eq!(
        files(&srv, "src%2Fl"),
        vec!["src/lib.rs".to_string()],
        "q percent-decoded"
    );
    assert_eq!(
        files(&srv, "read+me"),
        Vec::<String>::new(),
        "`+` es espacio en la query"
    );
    let uni = files(&srv, "%C3%91and%C3%BA");
    assert_eq!(
        uni,
        vec!["docs/Guía Ñandú.md".to_string()],
        "UTF-8 percent-encoded"
    );
    let many = files(&srv, "many/");
    assert_eq!(many.len(), 12);
    assert!(many.iter().all(|f| f.starts_with("many/file_")));
    for f in &all {
        assert!(
            !f.starts_with('/') && !f.contains(".."),
            "ruta no relativa: {f}"
        );
        assert!(!f.contains('\\'), "separador normalizado a `/`: {f}");
    }
}

#[test]
fn file_search_hides_control_plane_and_artifacts() {
    let srv = start();
    for q in [
        ".git",
        "config",
        "vault",
        ".tr",
        "onyxpack",
        "target",
        "node_modules",
        "session",
    ] {
        let got = files(&srv, q);
        assert!(
            got.iter().all(|f| !f.starts_with(".git/")
                && !f.contains(".onyx/vault")
                && !f.ends_with(".tr")
                && !f.ends_with(".onyxpack")
                && !f.starts_with("target/")
                && !f.starts_with("node_modules/")),
            "q={q} lista artefactos o plano de control: {got:?}"
        );
    }
}

#[test]
fn file_search_never_escapes_via_traversal_or_symlinks() {
    let srv = start();
    let hostile = [
        "..",
        "../",
        "%2e%2e%2f",
        "%2E%2E%2Foutside",
        "..%2f..%2fetc%2fpasswd",
        "%2fetc%2fpasswd",
        "/etc/passwd",
        "C:%5CWindows",
        "outside",
        "forja_outside_secret",
        "secret",
        "passwd",
        "link_",
        "etc_link",
        "%00",
        "%",
        "%zz",
        "%c3",
        "\u{0}",
    ];
    for q in hostile {
        let got = files(&srv, q);
        for f in &got {
            assert!(
                !f.contains("forja_outside_secret"),
                "q={q}: symlink seguido: {got:?}"
            );
            assert!(
                !f.starts_with("link_dir/") && f != "link_file.txt",
                "q={q}: {got:?}"
            );
            assert!(!f.starts_with("etc_link/"), "q={q}: {got:?}");
            assert!(!f.starts_with('/') && !f.contains(".."), "q={q}: {got:?}");
        }
    }
    // Control: el fichero de fuera existe y es legible por symlink en el FS real.
    #[cfg(unix)]
    assert!(srv.ws.ws.join("link_file.txt").exists());
}

#[test]
fn file_search_requires_auth() {
    let srv = start();
    let r = srv.get("/api/v1/files?q=main", &[]);
    assert_eq!(r.status, 401);
    assert!(!r.text().contains("main.rs"));
}
