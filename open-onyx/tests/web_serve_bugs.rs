//! Hallazgos de `openonyx serve` destapados por Forja. Cada test cubre su arreglo
//! (FIX-08/10) y corre en verde con el resto de la suite.

mod common_web;

use common_web::ServeGuard;
use std::io::Write;
use std::time::Duration;

/// FIX-08 (arreglado): `read_capped_line` corta la línea EN CUANTO supera
/// `MAX_HTTP_HEADER_BYTES` (16 KiB) DURING la lectura, contesta 431 y cierra:
/// un cliente sin autenticar ya no puede inflar la memoria con una línea sin `\n`.
#[test]
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

/// FIX-10 (arreglado): cada login emite un token aleatorio y el logout revoca la
/// sesión EN EL SERVIDOR: una cookie copiada antes del logout deja de autenticar.
#[test]
fn session_cookie_stops_working_after_logout() {
    let srv = ServeGuard::start("bug_logout");
    let cookie = srv.login_cookie();
    assert_eq!(srv.get("/api/v1/state", &[("Cookie", &cookie)]).status, 200);
    let r = srv.post(
        "/api/v1/auth/logout",
        &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
        "",
    );
    assert_eq!(r.status, 200);
    let after = srv.get("/api/v1/state", &[("Cookie", &cookie)]);
    assert_eq!(
        after.status, 401,
        "la cookie anterior al logout sigue siendo válida"
    );
}
