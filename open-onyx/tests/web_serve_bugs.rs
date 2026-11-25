//! Hallazgos de `openonyx serve` destapados por Forja. Cada test está en `#[ignore]`
//! hasta que se arregle; ejecutar con `cargo test -p open-onyx --test web_serve_bugs -- --ignored`.

mod common;

use common::ServeGuard;
use std::io::Write;
use std::time::Duration;

/// `read_http_request` usa `BufRead::read_line` sin tope: acumula la línea entera en
/// memoria y solo después compara con `MAX_HTTP_HEADER_BYTES` (16 KiB). Un cliente sin
/// autenticar puede hacer crecer la memoria del proceso sin límite enviando una línea
/// sin `\n`. Lo correcto es dejar de leer (y cerrar) en cuanto se superan los 16 KiB.
#[test]
#[ignore = "BUG: read_line sin tope; el límite de 16 KiB de cabecera se comprueba después de leer la línea entera"]
fn header_limit_is_enforced_while_reading_not_after() {
    for prefix in [&b"GET /"[..], &b"GET / HTTP/1.1\r\nX-Long: "[..]] {
        let mut srv = ServeGuard::start("bug_header_line");
        let mut s = srv.connect();
        s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(prefix).unwrap();
        // 64 MiB sin salto de línea: muy por encima de los buffers de socket del kernel.
        let chunk = vec![b'A'; 1 << 20];
        let mut sent_mib = 0usize;
        for _ in 0..64 {
            if s.write_all(&chunk).is_err() {
                break;
            }
            sent_mib += 1;
        }
        drop(s);
        srv.assert_still_serving("tras la línea gigante");
        assert!(
            sent_mib < 64,
            "{:?}: el servidor aceptó {sent_mib} MiB en una sola línea de cabecera (límite 16 KiB)",
            String::from_utf8_lossy(prefix)
        );
    }
}
