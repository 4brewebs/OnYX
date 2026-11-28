//! `openonyx serve` expuesto como en un VPS: bind no-loopback (`0.0.0.0:0`) con
//! `--api-key` explícita, atacado desde fuera **sin autenticar**.
//!
//! Cada test corre bajo [`hard_timeout`] para no colgar nunca el CI. Las mediciones de
//! memoria, hilos y descriptores leen `/proc/<pid>` y solo se hacen en Linux. Lo que
//! falla hoy está en `#[ignore = "BUG: ..."]` (ejecutar con `-- --ignored`).

mod common_web;

use common_web::{ServeGuard, ServeOpts, build_request};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const VPS_KEY: &str = "forja-vps-explicit-key-0123456789";

thread_local! {
    /// PIDs de los `serve` que arrancó el test que corre en este hilo (los rellena [`track`]);
    /// [`hard_timeout`] los mata desde el hilo padre si el test no termina.
    static SERVE_PIDS: std::cell::RefCell<Option<std::sync::Arc<std::sync::Mutex<Vec<u32>>>>> =
        const { std::cell::RefCell::new(None) };
}

fn track(srv: ServeGuard) -> ServeGuard {
    SERVE_PIDS.with(|p| {
        if let Some(pids) = p.borrow().as_ref() {
            pids.lock().unwrap().push(srv.pid());
        }
    });
    srv
}

fn vps(tag: &str) -> ServeGuard {
    track(ServeGuard::start_with(
        tag,
        ServeOpts {
            bind: Some("0.0.0.0:0"),
            api_key: Some(VPS_KEY),
            ..ServeOpts::default()
        },
    ))
}

/// Ejecuta `f` en un hilo y falla si no termina en `limit` (el hilo huérfano no bloquea
/// la salida del binario de tests). Si vence el plazo, el hilo padre mata con SIGKILL los
/// `serve` que arrancó `f` (registrados con [`track`]): el `Drop` de `ServeGuard` vive en el
/// hilo atascado y no se ejecutaría, y el `serve` seguiría vivo (con su RSS y sus hilos)
/// hasta que termine el binario de tests.
fn hard_timeout<F: FnOnce() + Send + 'static>(limit: Duration, f: F) {
    let (tx, rx) = std::sync::mpsc::channel();
    let pids = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let thread_pids = pids.clone();
    let h = std::thread::spawn(move || {
        SERVE_PIDS.with(|p| *p.borrow_mut() = Some(thread_pids));
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(limit) {
        Ok(()) => h.join().unwrap(),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(p) = h.join() {
                std::panic::resume_unwind(p);
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            for pid in pids.lock().unwrap().iter() {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
            panic!("timeout duro de {limit:?} superado")
        }
    }
}

#[cfg(target_os = "linux")]
fn proc_field_kib(pid: u32, field: &str) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix(field))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("campo {field} en /proc/{pid}/status"))
}

#[cfg(target_os = "linux")]
fn fd_count(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .count()
}

fn legit_health_ms(srv: &ServeGuard) -> u128 {
    let t = Instant::now();
    let r = srv.get("/api/v1/health", &[]);
    assert_eq!(r.status, 200);
    t.elapsed().as_millis()
}

fn connect_raw(srv: &ServeGuard) -> TcpStream {
    let s = TcpStream::connect_timeout(&srv.addr, Duration::from_secs(5)).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

#[test]
fn vps_bind_requires_the_explicit_key_and_never_prints_it() {
    hard_timeout(Duration::from_secs(60), || {
        let srv = vps("vps_key");
        assert_eq!(srv.api_key, VPS_KEY);
        let r = srv.get("/api/v1/state", &[]);
        assert_eq!(r.status, 401);
        let r = srv.get(
            "/api/v1/state",
            &[("Authorization", &format!("Bearer {VPS_KEY}"))],
        );
        assert_eq!(r.status, 200);
        assert!(!r.text().contains(VPS_KEY));
        assert!(
            !srv.banner.contains(VPS_KEY),
            "con --api-key el banner no debe imprimir la clave: {}",
            srv.banner
        );
    });
}

// ---------------------------------------------------------------- 1. cabeceras gigantes

#[cfg(target_os = "linux")]
#[test]
fn unauthenticated_giant_header_lines_do_not_grow_memory() {
    hard_timeout(Duration::from_secs(120), || {
        let srv = vps("vps_rss");
        let pid = srv.pid();
        let before = proc_field_kib(pid, "VmRSS:");
        let addr = srv.addr;
        let workers: Vec<_> = (0..4)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut s = TcpStream::connect(addr).unwrap();
                    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                    let prefix: &[u8] = if i % 2 == 0 {
                        b"GET /"
                    } else {
                        b"GET / HTTP/1.1\r\nX: "
                    };
                    let _ = s.write_all(prefix);
                    let chunk = vec![b'A'; 1 << 20];
                    for _ in 0..64 {
                        if s.write_all(&chunk).is_err() {
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        let after = proc_field_kib(pid, "VmRSS:");
        let grown_mib = after.saturating_sub(before) / 1024;
        assert!(
            grown_mib < 64,
            "RSS creció {grown_mib} MiB ({before} KiB → {after} KiB) con cabeceras sin autenticar"
        );
    });
}

#[test]
fn oversized_requests_get_a_4xx_answer() {
    hard_timeout(Duration::from_secs(60), || {
        let srv = vps("vps_4xx");
        let big_header = format!(
            "GET / HTTP/1.1\r\nHost: x\r\nX-Big: {}\r\n\r\n",
            "a".repeat(20 * 1024)
        );
        let r = srv
            .try_request(&big_header)
            .unwrap_or_else(|_| common_web::Response::parse(b""));
        assert!(
            (400..500).contains(&r.status),
            "cabecera > 16 KiB ⇒ {}",
            r.status
        );
        let big_line = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(20 * 1024));
        let r = srv
            .try_request(&big_line)
            .unwrap_or_else(|_| common_web::Response::parse(b""));
        assert!(
            (400..500).contains(&r.status),
            "request line > 16 KiB ⇒ {}",
            r.status
        );
        let r = srv
            .try_request("POST /api/v1/auth/login HTTP/1.1\r\nContent-Length: 999999999\r\n\r\n")
            .unwrap_or_else(|_| common_web::Response::parse(b""));
        assert!(
            (400..500).contains(&r.status),
            "Content-Length enorme ⇒ {}",
            r.status
        );
    });
}

// ------------------------------------------------------------ 2. Content-Length enorme o falso

#[test]
fn huge_or_lying_content_length_is_bounded_before_and_after_login() {
    hard_timeout(Duration::from_secs(90), || {
        let mut srv = vps("vps_cl");
        let bearer = format!("Bearer {VPS_KEY}");
        for auth in [None, Some(bearer.as_str())] {
            let auth_hdr = auth
                .map(|a| format!("Authorization: {a}\r\n"))
                .unwrap_or_default();
            // Enorme: se descarta al instante sin leer ni reservar el cuerpo.
            let mut s = connect_raw(&srv);
            let t = Instant::now();
            s.write_all(
                format!("POST /api/v1/command HTTP/1.1\r\n{auth_hdr}Content-Length: 18446744073709551615\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            assert!(
                t.elapsed() < Duration::from_secs(3),
                "CL enorme no se cortó a tiempo"
            );
            assert!(!buf.starts_with(b"HTTP/1.1 200"), "CL enorme procesado");

            // Mentiroso por arriba: anuncia 1000 y manda 10 ⇒ el servidor espera y corta
            // por timeout de lectura (10 s), sin procesar la petición.
            let mut s = connect_raw(&srv);
            s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let t = Instant::now();
            s.write_all(
                format!("POST /api/v1/command HTTP/1.1\r\n{auth_hdr}Content-Length: 1000\r\n\r\n{{\"command\"")
                    .as_bytes(),
            )
            .unwrap();
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            assert!(
                buf.is_empty(),
                "cuerpo incompleto procesado: {:?}",
                String::from_utf8_lossy(&buf)
            );
            assert!(
                t.elapsed() < Duration::from_secs(15),
                "no cortó el cuerpo incompleto"
            );

            // Mentiroso por abajo: anuncia 2 y manda más; solo se interpretan 2 bytes
            // (`{}`), lo sobrante no se cuela como segunda petición (Connection: close).
            let raw = format!(
                "POST /api/v1/queue/clear HTTP/1.1\r\n{auth_hdr}Sec-Fetch-Site: same-origin\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}GET /api/v1/state HTTP/1.1\r\n\r\n"
            );
            let r = srv.request(&raw);
            let want = if auth.is_some() { 200 } else { 401 };
            assert_eq!(r.status, want);
            assert!(
                !r.text().contains("onyx.web.state.v1"),
                "petición contrabandeada respondida"
            );
            srv.assert_still_serving("tras Content-Length falsos");
        }
    });
}

// ------------------------------------------------------------------------ 3. slowloris

#[test]
fn slowloris_does_not_block_a_legitimate_client() {
    hard_timeout(Duration::from_secs(90), || {
        let mut srv = vps("vps_slowloris");
        let mut conns: Vec<TcpStream> = (0..150)
            .map(|_| {
                let mut s = connect_raw(&srv);
                s.write_all(b"GET / HTTP/1.1\r\n").unwrap();
                s
            })
            .collect();
        // 6 s de goteo: un byte por conexión cada 2 s.
        for _ in 0..3 {
            for s in conns.iter_mut() {
                let _ = s.write_all(b"X");
            }
            let ms = legit_health_ms(&srv);
            assert!(
                ms < 2000,
                "cliente legítimo tardó {ms} ms con 150 slowloris"
            );
            std::thread::sleep(Duration::from_secs(2));
        }
        srv.assert_still_serving("con slowloris activo");
    });
}

#[test]
fn slowloris_connections_are_cut_by_a_total_request_deadline() {
    hard_timeout(Duration::from_secs(90), || {
        let srv = vps("vps_slowloris_deadline");
        let mut s = connect_raw(&srv);
        s.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        // 30 s goteando un byte cada 3 s (por debajo del timeout de lectura de 10 s).
        let start = Instant::now();
        let mut cut = false;
        while start.elapsed() < Duration::from_secs(30) {
            if s.write_all(b"X").is_err() {
                cut = true;
                break;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        if !cut {
            s.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut b = [0u8; 1];
            cut = matches!(s.read(&mut b), Ok(0));
        }
        assert!(cut, "la conexión lenta sigue viva tras 30 s de goteo");
    });
}

// --------------------------------------------------- 4. muchas conexiones sin autenticar

#[cfg(target_os = "linux")]
#[test]
fn idle_unauthenticated_connections_release_threads_and_fds_after_close() {
    hard_timeout(Duration::from_secs(90), || {
        let mut srv = vps("vps_conns");
        let pid = srv.pid();
        std::thread::sleep(Duration::from_millis(300));
        let threads0 = proc_field_kib(pid, "Threads:");
        let fds0 = fd_count(pid);
        let conns: Vec<TcpStream> = (0..300).map(|_| connect_raw(&srv)).collect();
        std::thread::sleep(Duration::from_millis(800));
        let ms = legit_health_ms(&srv);
        assert!(
            ms < 2000,
            "cliente legítimo tardó {ms} ms con 300 conexiones ociosas"
        );
        let threads1 = proc_field_kib(pid, "Threads:");
        let fds1 = fd_count(pid);
        eprintln!("hilos {threads0}→{threads1}, fds {fds0}→{fds1} con 300 conexiones ociosas");
        drop(conns);
        // El servidor libera hilo y descriptor de cada conexión cerrada.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let t = proc_field_kib(pid, "Threads:");
            let f = fd_count(pid);
            if t <= threads0 + 5 && f <= fds0 + 5 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "recursos no liberados: hilos {threads0}→{t}, fds {fds0}→{f}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        srv.assert_still_serving("tras 300 conexiones ociosas");
    });
}

#[cfg(target_os = "linux")]
#[test]
fn concurrent_unauthenticated_connections_are_capped() {
    hard_timeout(Duration::from_secs(60), || {
        let srv = vps("vps_conn_cap");
        let pid = srv.pid();
        std::thread::sleep(Duration::from_millis(300));
        let threads0 = proc_field_kib(pid, "Threads:");
        let _conns: Vec<TcpStream> = (0..300).map(|_| connect_raw(&srv)).collect();
        std::thread::sleep(Duration::from_millis(800));
        let threads1 = proc_field_kib(pid, "Threads:");
        assert!(
            threads1 < threads0 + 128,
            "300 conexiones sin autenticar crearon {} hilos",
            threads1 - threads0
        );
    });
}

// ----------------------------------------------------------------- 5. fuerza bruta

#[test]
fn brute_force_never_locks_out_the_real_key() {
    hard_timeout(Duration::from_secs(90), || {
        let srv = vps("vps_bruteforce");
        for i in 0..100 {
            let body = format!(r#"{{"api_key":"forja-guess-{i:04}"}}"#);
            let r = srv.post(
                "/api/v1/auth/login",
                &[("Sec-Fetch-Site", "same-origin")],
                &body,
            );
            assert!(
                r.status == 401 || r.status == 429,
                "intento {i}: {}",
                r.status
            );
            assert!(r.header("set-cookie").is_none());
        }
        let body = format!(r#"{{"api_key":"{VPS_KEY}"}}"#);
        let r = srv.post(
            "/api/v1/auth/login",
            &[("Sec-Fetch-Site", "same-origin")],
            &body,
        );
        // Si hay rate limit, no debe convertirse en un bloqueo permanente del dueño.
        assert!(r.status == 200 || r.status == 429, "{}", r.status);
    });
}

#[test]
fn login_brute_force_is_rate_limited() {
    hard_timeout(Duration::from_secs(120), || {
        let srv = vps("vps_ratelimit");
        let t = Instant::now();
        let mut limited = false;
        for i in 0..200 {
            let body = format!(r#"{{"api_key":"forja-guess-{i:04}"}}"#);
            let r = srv.post(
                "/api/v1/auth/login",
                &[("Sec-Fetch-Site", "same-origin")],
                &body,
            );
            if r.status == 429 {
                limited = true;
                break;
            }
        }
        let elapsed = t.elapsed();
        assert!(
            limited || elapsed > Duration::from_secs(20),
            "200 intentos fallidos en {elapsed:?} sin 429 ni retardo"
        );
    });
}

// --------------------------------------------------------------- 6. logout (ver web_serve_bugs)

#[test]
fn vps_session_cookie_stops_working_after_logout() {
    hard_timeout(Duration::from_secs(60), || {
        let srv = vps("vps_logout");
        let cookie = srv.login_cookie();
        let r = srv.post(
            "/api/v1/auth/logout",
            &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
            "",
        );
        assert_eq!(r.status, 200);
        assert_eq!(srv.get("/api/v1/state", &[("Cookie", &cookie)]).status, 401);
        assert_eq!(
            srv.get("/api/v1/events", &[("Cookie", &cookie)]).status,
            401
        );
    });
}

// ----------------------------------------------------------------------- 7. SSE

#[test]
fn sse_rejects_missing_expired_and_foreign_cookies() {
    hard_timeout(Duration::from_secs(90), || {
        let expired = {
            let old = vps("vps_sse_old");
            old.login_cookie()
        };
        let srv = vps("vps_sse");
        let good = srv.login_cookie();
        let (_, good_val) = good.split_once('=').unwrap();
        let cases = vec![
            ("sin cookie".to_string(), None),
            (
                "cookie de otra instancia".to_string(),
                Some(expired.clone()),
            ),
            (
                "cookie inventada".to_string(),
                Some("onyx_session=deadbeef".to_string()),
            ),
            (
                "otro nombre".to_string(),
                Some(format!("session={good_val}")),
            ),
            (
                "valor truncado".to_string(),
                Some(format!("onyx_session={}", &good_val[..good_val.len() - 1])),
            ),
            (
                "API key como cookie".to_string(),
                Some(format!("onyx_session={VPS_KEY}")),
            ),
            ("vacía".to_string(), Some("onyx_session=".to_string())),
        ];
        for (label, cookie) in cases {
            let extra: Vec<(&str, &str)> = cookie
                .as_deref()
                .map(|c| ("Cookie", c))
                .into_iter()
                .collect();
            let r = srv.request(&build_request("GET", "/api/v1/events", &extra, None));
            assert_eq!(r.status, 401, "SSE con {label}");
            assert!(
                !r.text().contains("event:"),
                "SSE con {label} emitió eventos"
            );
        }
        // Control: la cookie buena sí abre el stream.
        let mut s = srv.connect();
        s.write_all(build_request("GET", "/api/v1/events", &[("Cookie", &good)], None).as_bytes())
            .unwrap();
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).unwrap();
        assert!(buf[..n].starts_with(b"HTTP/1.1 200"));
    });
}

// ------------------------------------------------------------ 8. traversal en /files

#[test]
fn file_search_survives_double_encoding_and_overlong_utf8() {
    hard_timeout(Duration::from_secs(60), || {
        let srv = track(ServeGuard::start_with(
            "vps_files",
            ServeOpts {
                bind: Some("0.0.0.0:0"),
                api_key: Some(VPS_KEY),
                prepare: Some(&|ws: &std::path::Path| {
                    std::fs::write(ws.join("inside.txt"), "ok").unwrap();
                    let out = ws.parent().unwrap().join("outside_vps");
                    std::fs::create_dir_all(&out).unwrap();
                    std::fs::write(out.join("vps_secret.txt"), "SECRET").unwrap();
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&out, ws.join("escape")).unwrap();
                }),
                ..ServeOpts::default()
            },
        ));
        let bearer = format!("Bearer {VPS_KEY}");
        for q in [
            "%252e%252e%252f",
            "%25252e%25252e",
            "%c0%ae%c0%ae%2f",
            "%e0%80%ae%e0%80%ae",
            "..%c0%af..%c0%afetc",
            "%2e%2e%5c%2e%2e%5c",
            "vps_secret",
            "escape%2fvps",
            "%2fetc%2fpasswd",
            "....%2f%2f....%2f%2f",
        ] {
            let r = srv.get(
                &format!("/api/v1/files?q={q}"),
                &[("Authorization", &bearer)],
            );
            assert_eq!(r.status, 200, "q={q}");
            let files = r.json()["files"].as_array().unwrap().clone();
            for f in files {
                let f = f.as_str().unwrap();
                assert!(
                    !f.contains("vps_secret") && !f.starts_with("escape/"),
                    "q={q}: {f}"
                );
                assert!(!f.starts_with('/') && !f.contains(".."), "q={q}: {f}");
            }
        }
        let r = srv.get("/api/v1/files?q=%252e%252e", &[]);
        assert_eq!(r.status, 401, "sin auth no hay búsqueda");
    });
}

// ----------------------------------------------------------------------- 9. CSRF

#[test]
fn cross_site_fetch_metadata_blocks_every_post_on_a_public_bind() {
    hard_timeout(Duration::from_secs(60), || {
        let srv = vps("vps_csrf");
        let cookie = srv.login_cookie();
        for path in [
            "/api/v1/command",
            "/api/v1/approval",
            "/api/v1/focus",
            "/api/v1/queue/clear",
            "/api/v1/auth/login",
            "/api/v1/auth/logout",
        ] {
            let r = srv.post(
                path,
                &[
                    ("Cookie", &cookie),
                    ("Origin", "https://evil.example"),
                    ("Sec-Fetch-Site", "cross-site"),
                ],
                "{}",
            );
            assert_eq!(r.status, 403, "{path}");
        }
    });
}

/// FIX-42: un `POST` con cookie y `Content-Type: text/plain` (lo que puede enviar un formulario
/// HTML cross-site con `enctype="text/plain"`) y sin `Sec-Fetch-Site` (navegadores antiguos o
/// proxies que la quitan) se ejecuta. El guard CSRF solo mira `Sec-Fetch-Site`; el servidor no
/// exige `application/json` en las rutas mutantes. Sin `Origin` a propósito: así el test solo
/// pasa si se exige el tipo de contenido, y no se confunde con FIX-94. Basta cualquier 4xx
/// (400/403/415).
#[test]
fn mutating_post_without_json_content_type_is_rejected() {
    hard_timeout(Duration::from_secs(60), || {
        let srv = vps("vps_ctype");
        let cookie = srv.login_cookie();
        let raw = format!(
            "POST /api/v1/queue/clear HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: {cookie}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}"
        );
        let r = srv.request(&raw);
        assert!(
            (400..500).contains(&r.status),
            "POST text/plain con cookie y sin Sec-Fetch-Site ⇒ {}",
            r.status
        );
    });
}

/// FIX-94: aunque el cuerpo sea `application/json`, sin `Sec-Fetch-Site` el guard no mira
/// `Origin`: un POST con cookie y `Origin` ajeno se acepta (mitigado por SameSite=Lax).
#[test]
fn foreign_origin_without_fetch_metadata_is_rejected() {
    hard_timeout(Duration::from_secs(60), || {
        let srv = vps("vps_origin");
        let cookie = srv.login_cookie();
        let r = srv.post(
            "/api/v1/queue/clear",
            &[("Cookie", &cookie), ("Origin", "https://evil.example")],
            "{}",
        );
        assert_eq!(
            r.status, 403,
            "POST JSON con Origin ajeno y sin Sec-Fetch-Site ⇒ {}",
            r.status
        );
    });
}
