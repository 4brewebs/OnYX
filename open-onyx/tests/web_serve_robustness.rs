//! Caja negra de `openonyx serve`: robustez del servidor HTTP/1.1 escrito a mano.
//!
//! Peticiones malformadas, límites de cabecera (`MAX_HTTP_HEADER_BYTES` = 16 KiB) y de
//! cuerpo (`MAX_HTTP_BODY_BYTES` = 64 KiB), conexiones lentas (timeout de lectura de
//! 10 s por conexión), concurrencia, rutas estáticas hostiles y fuzz determinista.
//! Invariante común: el proceso sigue vivo y atendiendo después de cada agresión.

mod common;

use common::{Rng, ServeGuard, build_request};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;

/// Espera a que el servidor cierre la conexión (EOF o error) y devuelve lo leído.
fn read_until_closed(s: &mut std::net::TcpStream, limit: Duration) -> (Vec<u8>, Duration) {
    let start = Instant::now();
    s.set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while start.elapsed() < limit {
        match s.read(&mut buf) {
            Ok(0) => return (out, start.elapsed()),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return (out, start.elapsed()),
        }
    }
    panic!("la conexión no se cerró en {limit:?}");
}

#[test]
fn malformed_requests_never_crash_the_server() {
    let mut srv = ServeGuard::start("malformed");
    let garbage: Vec<&[u8]> = vec![
        b"",
        b"\r\n\r\n",
        b"GET\r\n\r\n",
        b"   \r\n\r\n",
        b"GET  \r\n\r\n",
        b"\x00\x01\x02\x03\r\n\r\n",
        b"\xff\xfe\xfd / HTTP/1.1\r\n\r\n",
        b"GET /\xc3\x28 HTTP/1.1\r\n\r\n",
        b"GET / HTTP/1.1\r\nNoColonHeader\r\n\r\n",
        b"GET / HTTP/1.1\r\n: empty-name\r\n\r\n",
        b"GET / HTTP/9.9\r\n\r\n",
        b"POST /api/v1/command HTTP/1.1\r\nContent-Length: abc\r\n\r\n",
        b"POST /api/v1/command HTTP/1.1\r\nContent-Length: -5\r\n\r\n",
        b"POST /api/v1/command HTTP/1.1\r\nContent-Length: 18446744073709551616\r\n\r\n",
        b"POST /api/v1/command HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        b"GET /?%%%%%% HTTP/1.1\r\n\r\n",
        b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03", // ClientHello TLS
    ];
    for raw in &garbage {
        let r = srv.try_request_bytes(raw);
        if let Ok(r) = r {
            assert!(
                r.status == 0 || (200..500).contains(&r.status),
                "entrada {:?} ⇒ estado {}",
                String::from_utf8_lossy(raw),
                r.status
            );
            assert_ne!(r.status, 500);
        }
        srv.assert_still_serving(&format!("tras {:?}", String::from_utf8_lossy(raw)));
    }
}

#[test]
fn oversized_headers_and_bodies_are_dropped_without_a_response() {
    let mut srv = ServeGuard::start("limits");
    let bearer = srv.bearer();

    // Cabeceras por debajo del límite: se sirven.
    let pad = "a".repeat(MAX_HEADER - 2048);
    let ok = srv.request(&format!(
        "GET / HTTP/1.1\r\nHost: x\r\nX-Pad: {pad}\r\n\r\n"
    ));
    assert_eq!(ok.status, 200);

    // Cabeceras por encima del límite (varias líneas): sin respuesta, conexión cerrada.
    let mut raw = String::from("GET / HTTP/1.1\r\nHost: x\r\n");
    for i in 0..40 {
        raw.push_str(&format!("X-H{i}: {}\r\n", "b".repeat(512)));
    }
    raw.push_str("\r\n");
    assert!(raw.len() > MAX_HEADER);
    let r = srv
        .try_request(&raw)
        .unwrap_or_else(|_| common::Response::parse(b""));
    assert_eq!(r.status, 0, "cabeceras > 16 KiB no deben procesarse");
    srv.assert_still_serving("tras cabeceras enormes");

    // Cuerpo en el límite exacto (64 KiB): se procesa.
    let json_core = r#"{"agent_id":1}"#;
    let body = format!("{json_core}{}", " ".repeat(MAX_BODY - json_core.len()));
    assert_eq!(body.len(), MAX_BODY);
    let r = srv.post(
        "/api/v1/focus",
        &[
            ("Authorization", &bearer),
            ("Sec-Fetch-Site", "same-origin"),
        ],
        &body,
    );
    assert_eq!(r.status, 200, "cuerpo de 64 KiB exactos debe aceptarse");

    // Un byte más: se descarta sin leerlo ni responder.
    let body = format!("{json_core}{}", " ".repeat(MAX_BODY + 1 - json_core.len()));
    let r = srv
        .try_request(&build_request(
            "POST",
            "/api/v1/focus",
            &[("Authorization", &bearer)],
            Some(&body),
        ))
        .unwrap_or_else(|_| common::Response::parse(b""));
    assert_eq!(r.status, 0, "cuerpo de 64 KiB + 1 debe descartarse");

    // Content-Length enorme anunciado sin cuerpo: cierre inmediato, sin esperar.
    let mut s = srv.connect();
    s.write_all(b"POST /api/v1/command HTTP/1.1\r\nContent-Length: 999999999999\r\n\r\n")
        .unwrap();
    let (resp, took) = read_until_closed(&mut s, Duration::from_secs(5));
    assert!(
        resp.is_empty(),
        "sin respuesta: {:?}",
        String::from_utf8_lossy(&resp)
    );
    assert!(
        took < Duration::from_secs(2),
        "debe cortar sin esperar el cuerpo: {took:?}"
    );
    srv.assert_still_serving("tras cuerpos enormes");
}

#[test]
fn slow_and_partial_clients_time_out_and_do_not_block_others() {
    let mut srv = ServeGuard::start("slowloris");
    // 40 conexiones a medio enviar (slowloris básico) + una con cuerpo incompleto.
    let mut slow: Vec<std::net::TcpStream> = (0..40)
        .map(|i| {
            let mut s = srv.connect();
            s.write_all(format!("GET / HTTP/1.1\r\nHost: x\r\nX-Slow-{i}: ").as_bytes())
                .unwrap();
            s
        })
        .collect();
    let mut partial = srv.connect();
    partial
        .write_all(b"POST /api/v1/auth/login HTTP/1.1\r\nContent-Length: 100\r\n\r\n{\"api_k")
        .unwrap();

    // Mientras tanto, el resto de clientes se atiende sin demora.
    for _ in 0..10 {
        let t = Instant::now();
        srv.assert_still_serving("con 41 clientes lentos");
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "health tardó {:?}",
            t.elapsed()
        );
    }

    // El servidor corta cada conexión colgada (timeout de lectura de 10 s).
    let (resp, took) = read_until_closed(&mut partial, Duration::from_secs(20));
    assert!(resp.is_empty(), "un cuerpo incompleto no debe procesarse");
    assert!(
        took >= Duration::from_secs(5),
        "cortó demasiado pronto: {took:?}"
    );
    for s in slow.iter_mut().take(3) {
        let (resp, _) = read_until_closed(s, Duration::from_secs(20));
        assert!(
            resp.is_empty() || resp.starts_with(b"HTTP/1.1 200"),
            "respuesta inesperada a cabecera a medias"
        );
    }
    srv.assert_still_serving("tras expirar clientes lentos");
}

#[test]
fn concurrent_clients_get_consistent_answers() {
    let mut srv = ServeGuard::start("concurrency");
    let app_len = srv.get("/app.js", &[]).body.len();
    assert!(app_len > 1000);
    let addr = srv.addr;
    let bearer = srv.bearer();
    let handles: Vec<_> = (0..48)
        .map(|t| {
            let bearer = bearer.clone();
            std::thread::spawn(move || {
                let mut rng = Rng::new(0xC0FFEE + t);
                for _ in 0..6 {
                    let (raw, want, len) = match rng.below(4) {
                        0 => (
                            build_request("GET", "/app.js", &[], None),
                            200,
                            Some(app_len),
                        ),
                        1 => (
                            build_request(
                                "GET",
                                "/api/v1/state",
                                &[("Authorization", &bearer)],
                                None,
                            ),
                            200,
                            None,
                        ),
                        2 => (build_request("GET", "/api/v1/state", &[], None), 401, None),
                        _ => (build_request("GET", "/api/v1/health", &[], None), 200, None),
                    };
                    let mut s = std::net::TcpStream::connect(addr).unwrap();
                    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
                    s.write_all(raw.as_bytes()).unwrap();
                    let mut buf = Vec::new();
                    s.read_to_end(&mut buf).unwrap();
                    let r = common::Response::parse(&buf);
                    assert_eq!(r.status, want, "hilo {t}");
                    if let Some(l) = len {
                        assert_eq!(r.body.len(), l, "hilo {t}: cuerpo truncado");
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("hilo cliente");
    }
    srv.assert_still_serving("tras 288 peticiones concurrentes");
}

#[test]
fn hostile_static_paths_never_escape_the_embedded_assets() {
    let mut srv = ServeGuard::start("traversal");
    std::fs::write(srv.ws.ws.join("secret.txt"), "FORJA-SECRET-IN-WORKSPACE").unwrap();
    let bearer = srv.bearer();
    let paths = [
        "/../Cargo.toml",
        "/../../../../etc/passwd",
        "/%2e%2e/%2e%2e/etc/passwd",
        "/%2E%2E%2F%2E%2E%2Fetc%2Fpasswd",
        "/..%2f..%2fCargo.toml",
        "/..%5c..%5cCargo.toml",
        "/%252e%252e/Cargo.toml",
        "/web/../../Cargo.toml",
        "/index.html/../../secret.txt",
        "/secret.txt",
        "//etc/passwd",
        "/./secret.txt",
        "/app.js/../secret.txt",
        "/app.js%00.txt",
        "/APP.JS",
        "/src/main.rs",
        "/web/app.js",
        "/.onyx/config.toml",
        "/.git/config",
        "/C:/Windows/win.ini",
        "/\\..\\..\\secret.txt",
        "http://evil.example/secret.txt",
        "*",
    ];
    for p in paths {
        for auth in [None, Some(bearer.as_str())] {
            let extra: Vec<(&str, &str)> = auth.map(|a| ("Authorization", a)).into_iter().collect();
            let r = srv.request(&build_request("GET", p, &extra, None));
            assert!(
                r.status == 401 || r.status == 404,
                "{p} (auth={}) ⇒ {}",
                auth.is_some(),
                r.status
            );
            let text = r.text();
            assert!(
                !text.contains("FORJA-SECRET"),
                "{p} filtró un fichero del workspace"
            );
            assert!(!text.contains("[package]"), "{p} filtró Cargo.toml");
            assert!(!text.contains("root:"), "{p} filtró /etc/passwd");
        }
    }
    // La query string no cambia el recurso (cache-busting del PWA).
    let r = srv.get("/app.js?v=../../secret.txt", &[]);
    assert_eq!(r.status, 200);
    assert!(
        r.header("content-type")
            .unwrap()
            .starts_with("application/javascript")
    );
    srv.assert_still_serving("tras rutas hostiles");
}

#[test]
fn unsupported_methods_never_succeed_or_echo() {
    let srv = ServeGuard::start("methods");
    let bearer = srv.bearer();
    for method in [
        "PUT", "DELETE", "PATCH", "HEAD", "TRACE", "CONNECT", "PROPFIND", "FOO",
    ] {
        for path in [
            "/",
            "/app.js",
            "/api/v1/state",
            "/api/v1/command",
            "/api/v1/auth/login",
        ] {
            let raw = format!(
                "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: {bearer}\r\nX-Echo-Probe: forja-echo\r\nContent-Length: 2\r\n\r\n{{}}"
            );
            let r = srv.request(&raw);
            assert!(
                !(200..300).contains(&r.status),
                "{method} {path} ⇒ {} (no debe tener éxito)",
                r.status
            );
            assert!(
                !r.text().contains("forja-echo"),
                "{method} {path} refleja cabeceras"
            );
        }
    }
    // Métodos en minúsculas se normalizan (`get` ≡ `GET`).
    let r = srv.request("get /api/v1/health HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(r.status, 200);
}

/// Genera una petición HTTP pseudoaleatoria (semilla fija) mezclando trozos válidos y hostiles.
fn fuzz_request(rng: &mut Rng, bearer: &str) -> (Vec<u8>, bool) {
    let methods = [
        "GET",
        "POST",
        "OPTIONS",
        "PUT",
        "get",
        "",
        "G\x00T",
        "POST POST",
    ];
    let paths = [
        "/",
        "/app.js",
        "/api/v1/state",
        "/api/v1/files?q=",
        "/api/v1/command",
        "/api/v1/approval",
        "/api/v1/focus",
        "/api/v1/auth/login",
        "/%2e%2e/",
        "/api/v1/state/../health",
        "/api/v1//state",
        "/api/v1/state?",
        "/\u{1F980}",
        "/api/v1/files?q=%",
        "/api/v1/files?q=%zz%2",
    ];
    let bodies = [
        "",
        "{}",
        "{\"command\":\"\"}",
        "{\"command\":\"/__onyx_subagent_proposal x\"}",
        "{\"agent_id\":-1,\"request\":1,\"approve\":true}",
        "{\"agent_id\":18446744073709551615}",
        "[1,2,3]",
        "{\"api_key\":\"\u{0000}\"}",
        "\u{FEFF}{}",
        "{\"command\":\"\\ud800\"}",
    ];
    let method = *rng.pick(&methods);
    let path = *rng.pick(&paths);
    let authed = rng.chance(1, 2);
    let mut raw = format!("{method} {path} HTTP/1.1\r\n");
    if rng.chance(4, 5) {
        raw.push_str("Host: 127.0.0.1\r\n");
    }
    if authed {
        let name = *rng.pick(&["Authorization", "authorization", "AUTHORIZATION"]);
        raw.push_str(&format!("{name}: {bearer}\r\n"));
    }
    if rng.chance(1, 3) {
        let site = *rng.pick(&["same-origin", "cross-site", "none", "same-site", ""]);
        raw.push_str(&format!("Sec-Fetch-Site: {site}\r\n"));
    }
    for _ in 0..rng.below(4) {
        let junk = *rng.pick(&[
            "X-A: \u{00e9}\u{4e2d}",
            "Cookie: onyx_session=; onyx_session",
            "Cookie: ;;;=",
            "Origin: null",
            "Content-Type: text/plain",
            "Connection: keep-alive",
            "Expect: 100-continue",
            "Range: bytes=0-1",
        ]);
        raw.push_str(junk);
        raw.push_str("\r\n");
    }
    let body = *rng.pick(&bodies);
    let declared = match rng.below(5) {
        0 => body.len().saturating_sub(1),
        _ => body.len(),
    };
    raw.push_str(&format!("Content-Length: {declared}\r\n\r\n{body}"));
    (raw.into_bytes(), authed)
}

#[test]
fn seeded_http_fuzz_keeps_auth_invariants() {
    let mut srv = ServeGuard::start("fuzz");
    let bearer = srv.bearer();
    for seed in [1u64, 7, 42, 1337, 0xDEAD_BEEF] {
        let mut rng = Rng::new(seed);
        for i in 0..60 {
            let (raw, authed) = fuzz_request(&mut rng, &bearer);
            let text = String::from_utf8_lossy(&raw).into_owned();
            let Ok(r) = srv.try_request_bytes(&raw) else {
                continue;
            };
            let ctx = format!("seed={seed} i={i} req={text:?}");
            assert!(r.status < 500 || r.status == 503, "{ctx} ⇒ {}", r.status);
            assert!(
                !r.text().contains(&srv.api_key),
                "{ctx}: refleja la API key"
            );
            // Sin credenciales nunca se obtiene 200 en la API salvo `health`.
            let first_line = text.lines().next().unwrap_or("");
            if !authed
                && r.status == 200
                && first_line.contains("/api/v1/")
                && !first_line.contains("/api/v1/health")
            {
                panic!("{ctx}: 200 sin autenticar");
            }
        }
        srv.assert_still_serving(&format!("tras fuzz seed={seed}"));
    }
}
