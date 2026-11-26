//! Auditoría VPS de Adolfo sobre `openonyx serve` (caja negra del binario).
//! FIX-35 (listener muere si `thread::spawn` falla), FIX-36 (cookie sin `Secure`/HSTS sin
//! `X-Forwarded-Proto`), FIX-37 / FIX-42 (CSRF con cookie), FIX-40 (`--sandbox off` filtra
//! `ONYX_WEB_API_KEY` y las claves de proveedor al hijo), FIX-41 (parser HTTP permisivo).
//! Cada bug lleva su control. Valores ficticios: nunca se usan claves reales ni red externa.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{FakeOpenAi, Step, TempDir};

const KEY: &str = "onyx-test-dummy-key";
/// Clave de proveedor ficticia que `serve` recibe en el entorno (FIX-40).
const PROVIDER_KEY: &str = "onyx-test-dummy-provider-key";

struct Srv {
    child: Child,
    port: u16,
    _home: TempDir,
    ws: TempDir,
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Lanza `openonyx serve` hermético. `prefix` permite envolverlo (p. ej. `unshare … prlimit`).
/// Si `key_via_env`, la API key va en `ONYX_WEB_API_KEY` (como en el despliegue del VPS).
fn launch(prefix: &[&str], extra: &[&str], api_base: &str, key_via_env: bool) -> Srv {
    use std::io::{BufRead, BufReader};
    let home = TempDir::new("vps_home");
    let ws = TempDir::new("vps_ws");
    std::fs::write(ws.0.join("README.md"), "fixture\n").unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&ws.0)
            .status()
            .unwrap()
            .success()
    );
    let bin = env!("CARGO_BIN_EXE_openonyx");
    let mut cmd = match prefix.split_first() {
        Some((first, rest)) => {
            let mut c = Command::new(first);
            c.args(rest).arg(bin);
            c
        }
        None => Command::new(bin),
    };
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &home.0)
        .env("USERPROFILE", &home.0)
        .env("ONYX_TRUST_FILE", home.0.join("trusted.toml"))
        .env("OPENAI_API_KEY", PROVIDER_KEY)
        .args([
            "serve",
            "--workspace",
            ws.0.to_str().unwrap(),
            "--bind",
            "127.0.0.1:0",
            "--provider",
            "openai",
            "--model",
            "fake-model",
            "--api-base",
            api_base,
            "--trust-workspace",
            "--auto-approve",
        ])
        .args(extra);
    if key_via_env {
        cmd.env("ONYX_WEB_API_KEY", KEY);
    } else {
        cmd.args(["--api-key", KEY]);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn openonyx serve");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut port = None;
    while Instant::now() < deadline && port.is_none() {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            if let Some(rest) = line.trim().strip_prefix("URL:") {
                port = rest
                    .trim()
                    .trim_end_matches('/')
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.parse::<u16>().ok());
            }
        }
    }
    let Some(port) = port else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("serve did not print its URL");
    };
    Srv {
        child,
        port,
        _home: home,
        ws,
    }
}

/// Petición HTTP cruda. Devuelve `(status, respuesta completa)`; status 0 si no hay respuesta.
fn raw(port: u16, req: &str) -> (u16, String) {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return (0, "connect failed".into());
    };
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    if s.write_all(req.as_bytes()).is_err() {
        return (0, "write failed".into());
    }
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, out)
}

fn post(port: u16, path: &str, headers: &[&str], body: &str) -> (u16, String) {
    let mut req = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for h in headers {
        req.push_str(h);
        req.push_str("\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    raw(port, &req)
}

/// Login con la API key: devuelve el par `onyx_session=<token>`.
fn login(port: u16) -> String {
    let (status, resp) = post(
        port,
        "/api/v1/auth/login",
        &["Content-Type: application/json"],
        &format!(r#"{{"api_key":"{KEY}"}}"#),
    );
    assert_eq!(status, 200, "{resp}");
    let line = resp
        .lines()
        .find(|l| l.starts_with("Set-Cookie: onyx_session="))
        .expect("session cookie");
    line.trim_start_matches("Set-Cookie: ")
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

const CMD: &str = r#"{"command":"/mode plan"}"#;
const DEAD_API: &str = "http://127.0.0.1:9/v1";

fn health(port: u16) -> u16 {
    raw(
        port,
        "GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .0
}

// ───────────── FIX-37 / FIX-42: CSRF con la cookie de sesión ─────────────

/// Control FIX-37: con `--cors-origin`, un POST cross-site desde el origen CORS permitido con
/// SOLO la cookie se bloquea.
#[test]
fn cors_cross_site_post_with_cookie_only_is_forbidden() {
    let srv = launch(
        &[],
        &["--cors-origin", "https://app.example"],
        DEAD_API,
        false,
    );
    let cookie = login(srv.port);
    let (status, resp) = post(
        srv.port,
        "/api/v1/command",
        &[
            &format!("Cookie: {cookie}"),
            "Sec-Fetch-Site: cross-site",
            "Origin: https://app.example",
            "Content-Type: application/json",
        ],
        CMD,
    );
    assert_eq!(status, 403, "{resp}");
}

/// Control FIX-37: un Bearer VÁLIDO cross-site con `--cors-origin` sí pasa (diseño).
#[test]
fn cors_cross_site_post_with_valid_bearer_is_allowed() {
    let srv = launch(
        &[],
        &["--cors-origin", "https://app.example"],
        DEAD_API,
        false,
    );
    let (status, resp) = post(
        srv.port,
        "/api/v1/command",
        &[
            &format!("Authorization: Bearer {KEY}"),
            "Sec-Fetch-Site: cross-site",
            "Origin: https://app.example",
            "Content-Type: application/json",
        ],
        CMD,
    );
    assert_eq!(status, 200, "{resp}");
}

/// FIX-37: desde el origen CORS permitido, añadir un `Authorization: Bearer x` INVÁLIDO a la
/// cookie basta para saltarse la guarda Sec-Fetch-Site: la guarda mira si hay Bearer (no si
/// es válido) y luego la cookie autentica.
#[test]
#[ignore = "BUG: FIX-37 con --cors-origin, cookie + Authorization: Bearer inválido + Sec-Fetch-Site: cross-site da 200: la guarda CSRF acepta cualquier Bearer presente y luego autentica la cookie (open-onyx/src/web.rs:1687-1689, :2146-2156)"]
fn cors_cross_site_post_with_cookie_and_bogus_bearer_is_forbidden() {
    let srv = launch(
        &[],
        &["--cors-origin", "https://app.example"],
        DEAD_API,
        false,
    );
    let cookie = login(srv.port);
    let (status, resp) = post(
        srv.port,
        "/api/v1/command",
        &[
            &format!("Cookie: {cookie}"),
            "Authorization: Bearer x",
            "Sec-Fetch-Site: cross-site",
            "Origin: https://app.example",
            "Content-Type: application/json",
        ],
        CMD,
    );
    assert_ne!(status, 200, "{resp}");
    assert!(status == 401 || status == 403, "{resp}");
}

/// Control FIX-42: el mismo POST simple (text/plain) con `Sec-Fetch-Site: cross-site` se
/// bloquea.
#[test]
fn text_plain_cookie_post_with_cross_site_fetch_metadata_is_forbidden() {
    let srv = launch(&[], &[], DEAD_API, false);
    let cookie = login(srv.port);
    let (status, resp) = post(
        srv.port,
        "/api/v1/command",
        &[
            &format!("Cookie: {cookie}"),
            "Sec-Fetch-Site: cross-site",
            "Content-Type: text/plain",
        ],
        CMD,
    );
    // 403 hoy (guarda Sec-Fetch-Site); 415/400 si el arreglo de FIX-42 exige
    // `application/json` antes de llegar a la guarda. Cualquiera es un rechazo.
    assert!(matches!(status, 400 | 403 | 415), "status {status}: {resp}");
}

/// FIX-42: un navegador sin Fetch Metadata (o un formulario `text/plain`) con la cookie
/// ejecuta comandos: sin `Sec-Fetch-Site` no hay ninguna otra defensa CSRF (ni
/// Content-Type JSON obligatorio, ni Origin, ni token).
#[test]
#[ignore = "BUG: FIX-42 POST /api/v1/command con cookie, Content-Type: text/plain y sin Sec-Fetch-Site da 200: la guarda CSRF solo actúa si llega Sec-Fetch-Site (open-onyx/src/web.rs:1684-1701)"]
fn text_plain_cookie_post_without_fetch_metadata_is_rejected() {
    let srv = launch(&[], &[], DEAD_API, false);
    let cookie = login(srv.port);
    let (status, resp) = post(
        srv.port,
        "/api/v1/command",
        &[&format!("Cookie: {cookie}"), "Content-Type: text/plain"],
        CMD,
    );
    // 415/400 (la API exige `application/json`, ficha de FIX-42) o 403 (guarda CSRF). Un 401
    // no vale: la cookie es válida, el rechazo tiene que ser por el tipo de contenido/CSRF.
    assert!(matches!(status, 400 | 403 | 415), "status {status}: {resp}");
}

// ───────────── FIX-35: el listener muere si thread::spawn falla ─────────────

/// Prefijo que limita los hilos de `serve` a 40 dentro de un user namespace propio (el
/// recuento de RLIMIT_NPROC solo ve los procesos de ese namespace: no depende de lo que
/// corra en la máquina). `None` si el host no permite user namespaces.
fn nproc_prefix() -> Option<Vec<&'static str>> {
    let ok = Command::new("unshare")
        .args([
            "--user",
            "--map-current-user",
            "prlimit",
            "--nproc=40:40",
            "--",
            "true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then(|| {
        vec![
            "unshare",
            "--user",
            "--map-current-user",
            "prlimit",
            "--nproc=40:40",
            "--",
        ]
    })
}

/// Como `nproc_prefix`, pero si el host no permite user namespaces / `prlimit` lo DICE en
/// stderr (`SKIP: …`, visible con `--nocapture`) y el test retorna sin evaluar nada. Es un
/// salto documentado, no un fallo ni un verde engañoso: en un host así FIX-35 no se ha
/// comprobado (ni el control ni el rojo) y hay que probarlo en otro. El ignorado, por tanto,
/// solo falla por el bug (el listener muerto) y nunca por falta de herramientas.
fn nproc_prefix_or_skip(test: &str) -> Option<Vec<&'static str>> {
    let prefix = nproc_prefix();
    if prefix.is_none() {
        eprintln!(
            "SKIP: {test} not evaluated: `unshare --user --map-current-user prlimit` unavailable (no user namespaces or util-linux); FIX-35 was NOT checked on this host"
        );
    }
    prefix
}

fn flood(port: u16, n: usize) {
    let socks: Vec<_> = (0..n)
        .filter_map(|_| TcpStream::connect(("127.0.0.1", port)).ok())
        .collect();
    std::thread::sleep(Duration::from_millis(1000));
    drop(socks);
    std::thread::sleep(Duration::from_millis(1500));
}

/// Control FIX-35: con el mismo límite, una ráfaga por debajo del tope no rompe nada.
#[cfg(target_os = "linux")]
#[test]
fn listener_survives_burst_below_thread_limit() {
    let Some(prefix) = nproc_prefix_or_skip("listener_survives_burst_below_thread_limit") else {
        return;
    };
    let srv = launch(&prefix, &["--sandbox", "off"], DEAD_API, false);
    assert_eq!(health(srv.port), 200);
    flood(srv.port, 10);
    assert_eq!(health(srv.port), 200);
}

/// FIX-35: al agotar hilos, `thread::spawn` hace pánico DENTRO del hilo de `accept_loop`;
/// el listener muere para siempre y el proceso sigue vivo (sin reinicio por systemd).
#[cfg(target_os = "linux")]
#[test]
#[ignore = "BUG: FIX-35 si thread::spawn falla (EAGAIN por RLIMIT_NPROC), el pánico mata el hilo de accept_loop; el proceso sigue vivo pero no vuelve a aceptar conexiones (open-onyx/src/web.rs:1520)"]
fn listener_survives_thread_exhaustion() {
    let Some(prefix) = nproc_prefix_or_skip("listener_survives_thread_exhaustion") else {
        return;
    };
    let mut srv = launch(&prefix, &["--sandbox", "off"], DEAD_API, false);
    assert_eq!(health(srv.port), 200);
    flood(srv.port, 80);
    assert!(
        srv.child.try_wait().unwrap().is_none(),
        "process must still be alive"
    );
    assert_eq!(
        health(srv.port),
        200,
        "listener must keep accepting after thread exhaustion"
    );
}

// ───────────── FIX-40: --sandbox off filtra ONYX_WEB_API_KEY ─────────────

fn tool_call_event(name: &str, args_json: &str) -> String {
    let args = args_json.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"call_1","type":"function","function":{{"name":"{name}","arguments":"{args}"}}}}]}},"finish_reason":null}}]}}"#
    )
}

const FINISH_TOOLS: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

/// Ejecuta un turno en el que el modelo pide `shell_exec env` y devuelve lo que el hijo vio
/// (todo su entorno, con los valores ficticios de `KEY` y `PROVIDER_KEY` si se filtran).
fn child_env(sandbox: &str) -> String {
    let fake = FakeOpenAi::start(vec![
        Step::Sse {
            events: vec![
                tool_call_event("shell_exec", r#"{"command":"env > leak.txt; echo done"}"#),
                FINISH_TOOLS.to_string(),
            ],
            done: true,
        },
        Step::Sse {
            events: vec![common::delta("fin"), common::FINISH_STOP.to_string()],
            done: true,
        },
    ]);
    let srv = launch(&[], &["--sandbox", sandbox], &fake.base_url(), true);
    let (status, resp) = post(
        srv.port,
        "/api/v1/command",
        &[
            &format!("Authorization: Bearer {KEY}"),
            "Content-Type: application/json",
        ],
        r#"{"command":"print the env"}"#,
    );
    assert_eq!(status, 200, "{resp}");
    let leak = srv.ws.0.join("leak.txt");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && fake.requests().len() < 2 {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        leak.exists(),
        "shell_exec did not run; requests: {:?}",
        fake.requests()
    );
    std::fs::read_to_string(leak).unwrap()
}

/// Control FIX-40: con `--sandbox workspace-write` el entorno se limpia: ni la clave de la
/// web ni la del proveedor llegan al hijo.
#[cfg(unix)]
#[test]
fn workspace_write_sandbox_does_not_leak_web_or_provider_keys() {
    let seen = child_env("workspace-write");
    assert!(!seen.contains(KEY), "child saw the web API key: {seen:?}");
    assert!(
        !seen.contains(PROVIDER_KEY),
        "child saw the provider key: {seen:?}"
    );
}

/// FIX-40: con `--sandbox off`, el hijo de `shell_exec` hereda el entorno completo de
/// `serve`, incluida la API key de la PWA. Un agente (o un prompt inyectado) la lee.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-40 con --sandbox off, shell_exec hereda ONYX_WEB_API_KEY del proceso serve (no hay env_clear ni filtro de claves sensibles en el modo Off) (onyx-workspace/src/sandbox.rs:1363)"]
fn sandbox_off_does_not_leak_web_api_key() {
    let seen = child_env("off");
    assert!(!seen.contains(KEY), "child saw the web API key: {seen:?}");
}

/// FIX-40 (misma causa, otra clave): con `--sandbox off` el hijo también hereda
/// `OPENAI_API_KEY` y el resto de claves de proveedor. Su ficha pide que ninguna llegue al
/// hijo; quitar solo `ONYX_WEB_API_KEY` no cierra FIX-40.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-40 con --sandbox off, shell_exec hereda también OPENAI_API_KEY y demás claves de proveedor (sandbox.rs:1363, plain_shell_command sin env_clear)"]
fn sandbox_off_does_not_leak_provider_api_keys() {
    let seen = child_env("off");
    assert!(
        !seen.contains(PROVIDER_KEY),
        "child saw the provider key: {seen:?}"
    );
}

// ───────────── FIX-36: cookie sin `Secure` ni HSTS si el proxy no manda X-Forwarded-Proto ─────────────

/// Login con cabeceras extra; devuelve `(status, respuesta completa)`.
fn login_with(port: u16, headers: &[&str]) -> (u16, String) {
    let mut h = vec!["Content-Type: application/json"];
    h.extend_from_slice(headers);
    post(
        port,
        "/api/v1/auth/login",
        &h,
        &format!(r#"{{"api_key":"{KEY}"}}"#),
    )
}

fn set_cookie_line(resp: &str) -> String {
    resp.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap_or_default()
        .to_string()
}

fn has_header(resp: &str, name: &str) -> bool {
    resp.lines().take_while(|l| !l.is_empty()).any(|l| {
        l.to_ascii_lowercase()
            .starts_with(&format!("{}:", name.to_ascii_lowercase()))
    })
}

/// Control FIX-36: detrás de un proxy que SÍ manda `X-Forwarded-Proto: https`, la cookie va
/// con `__Host-` y `Secure`.
#[test]
fn login_behind_https_proxy_sets_secure_host_cookie() {
    let srv = launch(&[], &[], DEAD_API, false);
    let (status, resp) = login_with(srv.port, &["X-Forwarded-Proto: https"]);
    assert_eq!(status, 200, "{resp}");
    let cookie = set_cookie_line(&resp);
    assert!(cookie.contains("__Host-"), "{cookie}");
    assert!(cookie.contains("Secure"), "{cookie}");
}

/// Control FIX-36: en loopback puro y sin proxy, HTTP sin `Secure` es el comportamiento local
/// documentado (la cookie `Secure` no se enviaría por `http://127.0.0.1` en todos los
/// navegadores). Que el rojo de abajo no lo cambie.
#[test]
fn login_on_plain_loopback_keeps_local_http_cookie() {
    let srv = launch(&[], &[], DEAD_API, false);
    let (status, resp) = login_with(srv.port, &[]);
    assert_eq!(status, 200, "{resp}");
    let cookie = set_cookie_line(&resp);
    assert!(cookie.contains("onyx_session="), "{cookie}");
    assert!(!cookie.contains("Secure"), "{cookie}");
}

/// FIX-36: con un bind NO loopback (despliegue en un VPS) y un proxy que no manda
/// `X-Forwarded-Proto` (nginx no lo manda por defecto), el login emite la cookie sin
/// `Secure` ni `__Host-` y la respuesta no lleva `Strict-Transport-Security`.
///
/// Se conecta por 127.0.0.1 a un servidor enlazado a 0.0.0.0. La ficha admite dos arreglos
/// (bind no loopback, o un flag explícito `--behind-tls`); este test cubre el primero: si el
/// arreglo es solo el flag, hay que añadirlo a los `extra` del test.
#[test]
#[ignore = "BUG: FIX-36 en bind no loopback y sin X-Forwarded-Proto: https, el login emite onyx_session sin Secure ni __Host- y sin Strict-Transport-Security: el modo seguro depende de la cabecera del proxy (open-onyx/src/web.rs:1726-1740, :2217-2231)"]
fn exposed_bind_login_without_forwarded_proto_is_secure_and_sends_hsts() {
    let srv = launch(&[], &["--bind", "0.0.0.0:0"], DEAD_API, false);
    let (status, resp) = login_with(srv.port, &[]);
    assert_eq!(status, 200, "{resp}");
    let cookie = set_cookie_line(&resp);
    let mut missing = Vec::new();
    if !cookie.contains("Secure") {
        missing.push("Secure");
    }
    if !cookie.contains("__Host-") {
        missing.push("__Host- prefix");
    }
    if !has_header(&resp, "Strict-Transport-Security") {
        missing.push("Strict-Transport-Security header");
    }
    assert!(
        missing.is_empty(),
        "exposed login lacks {missing:?}; Set-Cookie: {cookie}"
    );
}

// ───────────── FIX-41: parser HTTP permisivo ─────────────

/// Petición cruda con half-close (`shutdown(Write)`) tras escribirla, para que el servidor no
/// espere más bytes; devuelve el status (0 si no hay respuesta) y la respuesta.
fn raw_half_close(port: u16, req: &[u8]) -> (u16, String) {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return (0, "connect failed".into());
    };
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    if s.write_all(req).is_err() {
        return (0, "write failed".into());
    }
    let _ = s.shutdown(std::net::Shutdown::Write);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let out = String::from_utf8_lossy(&out).into_owned();
    let status = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, out)
}

/// Peticiones mal formadas de Adolfo (`raw.py`), todas con credenciales válidas, que un
/// parser HTTP/1.1 estricto debe contestar con 400.
fn malformed_requests() -> Vec<(&'static str, Vec<u8>)> {
    let auth = format!("Authorization: Bearer {KEY}\r\n");
    let body = CMD;
    let len = body.len();
    let post = |headers: String| {
        format!("POST /api/v1/command HTTP/1.1\r\nHost: x\r\n{auth}{headers}\r\n{body}")
            .into_bytes()
    };
    vec![
        // Sin cuerpo, para que un valor inválido tomado como 0 dé 200 (con cuerpo, el 400
        // vendría del JSON vacío y no del framing).
        (
            "Content-Length negativo",
            b"GET /api/v1/health HTTP/1.1\r\nHost: x\r\nContent-Length: -1\r\n\r\n".to_vec(),
        ),
        (
            "Content-Length no numérico",
            b"GET /api/v1/health HTTP/1.1\r\nHost: x\r\nContent-Length: abc\r\n\r\n".to_vec(),
        ),
        (
            "espacio antes de ':'",
            post(format!(
                "Content-Type: application/json\r\nContent-Length : {len}\r\n"
            )),
        ),
        (
            "Transfer-Encoding: chunked + Content-Length",
            post(format!(
                "Content-Type: application/json\r\nTransfer-Encoding: chunked\r\nContent-Length: {len}\r\n"
            )),
        ),
        (
            "obs-fold (` Content-Length` en continuación)",
            post(format!(
                "Content-Type: application/json\r\nX-A: 1\r\n Content-Length: {len}\r\n"
            )),
        ),
        (
            "línea de petición sin versión HTTP",
            format!("GET /api/v1/state\r\n{auth}\r\n").into_bytes(),
        ),
        (
            "solo LF (sin CRLF)",
            b"GET /api/v1/health HTTP/1.1\nHost: x\n\n".to_vec(),
        ),
    ]
}

/// Control FIX-41: la misma petición bien formada (CRLF, `Content-Length` correcto, HTTP/1.1)
/// se atiende con 200, y un GET sencillo también.
#[test]
fn well_formed_requests_are_served() {
    let srv = launch(&[], &[], DEAD_API, false);
    let auth = format!("Authorization: Bearer {KEY}\r\n");
    let ok_post = format!(
        "POST /api/v1/command HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{CMD}",
        CMD.len()
    );
    let (status, resp) = raw_half_close(srv.port, ok_post.as_bytes());
    assert_eq!(status, 200, "{resp}");
    let (status, resp) = raw_half_close(
        srv.port,
        b"GET /api/v1/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200, "{resp}");
}

/// FIX-41: `read_http_request` acepta framing ambiguo o inválido; detrás de un proxy es
/// superficie de desincronización. Cada caso debe ser 400. Se recogen todos los que no lo
/// son para que el informe diga cuáles (con el arreglo, la lista queda vacía).
#[test]
#[ignore = "BUG: FIX-41 el parser HTTP acepta espacio antes de ':', obs-fold, TE+CL (gana CL), Content-Length negativo o no numérico (tomado como 0), petición sin versión y solo LF en vez de contestar 400 (open-onyx/src/web.rs:2080-2091)"]
fn malformed_http_requests_are_rejected_with_400() {
    let srv = launch(&[], &[], DEAD_API, false);
    let mut accepted = Vec::new();
    for (label, req) in malformed_requests() {
        let (status, resp) = raw_half_close(srv.port, &req);
        if status != 400 {
            let first = resp.lines().next().unwrap_or("<no response>");
            accepted.push(format!("{label}: {first}"));
        }
    }
    assert!(
        accepted.is_empty(),
        "not rejected with 400:\n{}",
        accepted.join("\n")
    );
}
