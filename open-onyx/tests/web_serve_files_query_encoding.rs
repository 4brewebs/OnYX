//! Caja negra de la decodificación de `q` en `GET /api/v1/files?q=`.
//!
//! La query sigue `application/x-www-form-urlencoded`: `+` es un espacio y `%2B` es un
//! `+` literal. El caso `read+me` de `web_serve_files` no distinguía las dos lecturas
//! (ninguna coincide), así que un mutante que dejara `+` sin traducir sobrevivía. Aquí el
//! workspace tiene ficheros con espacio y con `+` en el nombre para que cada lectura dé
//! un resultado distinto.

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
    for rel in ["notes/my notes.txt", "notes/c++ tips.md", "notes/plain.txt"] {
        let p = ws.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }
}

#[test]
fn plus_is_a_space_and_percent_2b_is_a_literal_plus() {
    let srv = ServeGuard::start_with(
        "files_enc",
        ServeOpts {
            prepare: Some(&prepare),
            ..ServeOpts::default()
        },
    );
    assert_eq!(
        files(&srv, "my+notes"),
        vec!["notes/my notes.txt".to_string()],
        "`+` debe decodificarse como espacio"
    );
    assert_eq!(
        files(&srv, "my%20notes"),
        vec!["notes/my notes.txt".to_string()],
        "`%20` es espacio"
    );
    assert_eq!(
        files(&srv, "c%2B%2B"),
        vec!["notes/c++ tips.md".to_string()],
        "`%2B` es un `+` literal"
    );
    assert_eq!(
        files(&srv, "c+%2B"),
        Vec::<String>::new(),
        "`c+%2B` es `c +`, que no aparece en `c++ tips.md`"
    );
}
