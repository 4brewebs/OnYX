//! Caja negra de `openonyx serve`: el PWA embebido y el canal SSE.
//!
//! - Assets: tipos MIME, coherencia `index.html` ↔ `app.js` ↔ `manifest.webmanifest`
//!   (todo lo referenciado se sirve; todo id que `app.js` busca existe en el HTML) y
//!   compatibilidad con la CSP estricta que promete el README (sin scripts ni
//!   manejadores inline, cero `innerHTML`).
//! - SSE: cabeceras, formato `event:`/`data:`, eventos vivos tras una mutación, límite de
//!   32 suscriptores y liberación de plazas cuando el cliente corta.

mod common_web;

use common_web::{ServeGuard, build_request};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn attr_values<'a>(html: &'a str, attr: &str) -> Vec<&'a str> {
    let needle = format!("{attr}=\"");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(&needle) {
        let after = &rest[i + needle.len()..];
        let end = after.find('"').unwrap();
        out.push(&after[..end]);
        rest = &after[end..];
    }
    out
}

#[test]
fn embedded_assets_are_served_with_exact_content_types() {
    let srv = ServeGuard::start("assets");
    let cases = [
        ("/", "text/html; charset=utf-8"),
        ("/index.html", "text/html; charset=utf-8"),
        ("/styles.css", "text/css; charset=utf-8"),
        ("/app.js", "application/javascript; charset=utf-8"),
        (
            "/manifest.webmanifest",
            "application/manifest+json; charset=utf-8",
        ),
        ("/icon.svg", "image/svg+xml; charset=utf-8"),
        ("/logo.jpg", "image/jpeg"),
    ];
    for (path, ct) in cases {
        let r = srv.get(path, &[]);
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.header("content-type"), Some(ct), "{path}");
        assert!(!r.body.is_empty(), "{path} vacío");
    }
    // Bytes exactos del JPEG (sin transcodificar a UTF-8).
    let logo = srv.get("/logo.jpg", &[]);
    assert_eq!(&logo.body[..3], &[0xFF, 0xD8, 0xFF], "cabecera SOI de JPEG");
    assert_eq!(
        &logo.body[logo.body.len() - 2..],
        &[0xFF, 0xD9],
        "marcador EOI de JPEG"
    );
    let fav = srv.get("/favicon.ico", &[]);
    assert_eq!(fav.status, 204);
    assert!(fav.body.is_empty());
}

#[test]
fn every_asset_referenced_by_index_and_manifest_is_served() {
    let srv = ServeGuard::start("asset_refs");
    let html = srv.get("/", &[]).text();
    let mut refs: Vec<String> = attr_values(&html, "src")
        .into_iter()
        .chain(attr_values(&html, "href"))
        .filter(|u| u.starts_with('/'))
        .map(str::to_string)
        .collect();
    let manifest = srv.get("/manifest.webmanifest", &[]).json();
    assert_eq!(manifest["start_url"], "/");
    for icon in manifest["icons"].as_array().expect("icons") {
        let src = icon["src"].as_str().unwrap().to_string();
        let ty = icon["type"].as_str().unwrap().to_string();
        let r = srv.get(&src, &[]);
        assert_eq!(r.status, 200, "icono {src}");
        assert!(
            r.header("content-type").unwrap().starts_with(&ty),
            "icono {src}: manifest dice {ty}, servidor {:?}",
            r.header("content-type")
        );
        refs.push(src);
    }
    refs.push(manifest["start_url"].as_str().unwrap().to_string());
    refs.sort();
    refs.dedup();
    assert!(
        refs.iter().any(|r| r == "/app.js"),
        "index.html carga /app.js: {refs:?}"
    );
    for r in &refs {
        let resp = srv.get(r, &[]);
        assert_eq!(resp.status, 200, "recurso referenciado {r} no se sirve");
    }
}

#[test]
fn index_html_is_compatible_with_the_strict_csp() {
    let srv = ServeGuard::start("csp_html");
    let html = srv.get("/", &[]).text();
    let lower = html.to_ascii_lowercase();
    // `script-src 'self'` sin 'unsafe-inline': todo <script> debe tener src.
    let mut rest = lower.as_str();
    while let Some(i) = rest.find("<script") {
        let tag_end = rest[i..].find('>').unwrap() + i;
        let tag = &rest[i..tag_end];
        assert!(
            tag.contains(" src="),
            "script inline bloqueado por CSP: {tag}"
        );
        rest = &rest[tag_end..];
    }
    // Manejadores inline (`onclick=` …) y `javascript:` también quedan bloqueados.
    for bad in [
        " onclick=",
        " onload=",
        " onerror=",
        " onsubmit=",
        " oninput=",
        "javascript:",
    ] {
        assert!(
            !lower.contains(bad),
            "index.html usa `{bad}` (CSP lo bloquea)"
        );
    }
    // `style-src 'self'` bloquea atributos style y <style> inline.
    assert!(
        !lower.contains(" style=\""),
        "atributo style inline bloqueado por CSP"
    );
    assert!(
        !lower.contains("<style"),
        "<style> inline bloqueado por CSP"
    );
}

#[test]
fn app_js_respects_trusted_types_and_matches_index_ids() {
    let srv = ServeGuard::start("csp_js");
    let js = srv.get("/app.js", &[]).text();
    let html = srv.get("/", &[]).text();
    // `require-trusted-types-for 'script'` + README "zero innerHTML".
    for sink in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
    ] {
        assert!(!js.contains(sink), "app.js usa el sink `{sink}`");
    }
    // Cada `getElementById('x')` de app.js existe como id="x" en index.html.
    let ids: Vec<&str> = attr_values(&html, "id");
    let mut missing = Vec::new();
    let mut rest = js.as_str();
    let needle = "getElementById('";
    let mut count = 0;
    while let Some(i) = rest.find(needle) {
        let after = &rest[i + needle.len()..];
        let end = after.find('\'').unwrap();
        let id = &after[..end];
        count += 1;
        if !ids.contains(&id) {
            missing.push(id.to_string());
        }
        rest = &after[end..];
    }
    assert!(
        count > 20,
        "se esperaban muchas búsquedas por id, hubo {count}"
    );
    assert!(
        missing.is_empty(),
        "ids que app.js busca y no están en index.html: {missing:?}"
    );
}

#[test]
fn every_api_path_used_by_app_js_is_routed() {
    let srv = ServeGuard::start("api_wiring");
    let js = srv.get("/app.js", &[]).text();
    let mut paths: Vec<String> = Vec::new();
    for quote in ['\'', '`'] {
        let needle = format!("{quote}/api/v1/");
        let mut rest = js.as_str();
        while let Some(i) = rest.find(&needle) {
            let after = &rest[i + 1..];
            let end = after.find([quote, '?', '$']).unwrap();
            paths.push(after[..end].to_string());
            rest = &after[end..];
        }
    }
    paths.sort();
    paths.dedup();
    assert!(paths.len() >= 8, "{paths:?}");
    let bearer = srv.bearer();
    for p in &paths {
        if p == "/api/v1/events" {
            continue; // SSE: cubierto aparte (no cierra la conexión).
        }
        let get = srv.get(p, &[("Authorization", &bearer)]);
        let post = srv.post(
            p,
            &[
                ("Authorization", &bearer),
                ("Sec-Fetch-Site", "same-origin"),
            ],
            "{}",
        );
        assert!(
            get.status != 404 || post.status != 404,
            "app.js llama a {p} pero el servidor no tiene ruta (GET {} / POST {})",
            get.status,
            post.status
        );
    }
}

/// Abre una suscripción SSE y devuelve (stream, cabecera, resto ya leído).
fn open_sse(srv: &ServeGuard, auth: (&str, &str)) -> (TcpStream, String, Vec<u8>) {
    let mut s = srv.connect();
    s.write_all(build_request("GET", "/api/v1/events", &[auth], None).as_bytes())
        .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(10);
    while common::find(&buf, b"\r\n\r\n").is_none() && Instant::now() < deadline {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let split = common::find(&buf, b"\r\n\r\n").expect("cabecera SSE");
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let rest = buf[split + 4..].to_vec();
    (s, head, rest)
}

/// Lee frames SSE (`\n\n`) hasta que `pred` acepte uno o venza `limit`.
fn next_frame_matching(
    s: &mut TcpStream,
    pending: &mut Vec<u8>,
    limit: Duration,
    pred: impl Fn(&str) -> bool,
) -> Option<String> {
    let deadline = Instant::now() + limit;
    s.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut chunk = [0u8; 8192];
    loop {
        while let Some(i) = common::find(pending, b"\n\n") {
            let frame = String::from_utf8_lossy(&pending[..i]).into_owned();
            pending.drain(..i + 2);
            if pred(&frame) {
                return Some(frame);
            }
        }
        if Instant::now() > deadline {
            return None;
        }
        match s.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
            Err(_) => {}
        }
    }
}

fn parse_state_frame(frame: &str) -> serde_json::Value {
    let mut lines = frame.lines();
    assert_eq!(lines.next(), Some("event: state"), "{frame}");
    let data = lines
        .next()
        .and_then(|l| l.strip_prefix("data: "))
        .expect("data:");
    assert!(
        lines.next().is_none(),
        "un frame state es una sola línea data: {frame}"
    );
    serde_json::from_str(data).expect("data JSON")
}

#[test]
fn sse_streams_state_events_and_reacts_to_commands() {
    let srv = ServeGuard::start("sse");
    let cookie = srv.login_cookie();
    let (mut s, head, mut pending) = open_sse(&srv, ("Cookie", &cookie));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("content-type: text/event-stream"), "{head}");
    assert!(lower.contains("cache-control: no-cache"), "{head}");
    assert!(
        !lower.contains("content-length"),
        "SSE no debe fijar Content-Length: {head}"
    );
    assert!(lower.contains("x-content-type-options: nosniff"), "{head}");

    let first = next_frame_matching(&mut s, &mut pending, Duration::from_secs(10), |f| {
        f.starts_with("event: state")
    })
    .expect("primer evento state");
    let st = parse_state_frame(&first);
    assert_eq!(st["schema"], "onyx.web.state.v1");
    assert_eq!(st["mode"], "BUILD");
    assert!(
        !first.contains(&srv.api_key),
        "el stream no debe contener la API key"
    );
    let v0 = st["version"].as_u64().unwrap();

    let r = srv.post(
        "/api/v1/command",
        &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
        r#"{"command":"/mode plan"}"#,
    );
    assert_eq!(r.status, 200);
    let frame = next_frame_matching(&mut s, &mut pending, Duration::from_secs(10), |f| {
        f.starts_with("event: state") && f.contains("\"mode\":\"PLAN\"")
    })
    .expect("evento state con mode=PLAN tras /mode plan");
    let st = parse_state_frame(&frame);
    assert!(
        st["version"].as_u64().unwrap() > v0,
        "la versión del snapshot avanza"
    );
}

#[test]
fn sse_unauthenticated_is_rejected_and_does_not_consume_slots() {
    let srv = ServeGuard::start("sse_unauth");
    for _ in 0..40 {
        let r = srv.get("/api/v1/events", &[]);
        assert_eq!(r.status, 401);
        let r = srv.get("/api/v1/events", &[("Cookie", "onyx_session=forged")]);
        assert_eq!(r.status, 401);
    }
    let (_s, head, _) = open_sse(&srv, ("Authorization", &srv.bearer()));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

#[test]
fn sse_caps_subscribers_at_32_and_frees_slots_after_disconnect() {
    let mut srv = ServeGuard::start("sse_cap");
    let bearer = srv.bearer();
    let mut subs: Vec<TcpStream> = (0..32)
        .map(|i| {
            let (s, head, _) = open_sse(&srv, ("Authorization", &bearer));
            assert!(head.starts_with("HTTP/1.1 200"), "suscriptor {i}: {head}");
            s
        })
        .collect();
    let over = srv.get("/api/v1/events", &[("Authorization", &bearer)]);
    assert_eq!(over.status, 503, "el suscriptor 33 debe recibir 503");
    srv.assert_still_serving("con 32 SSE abiertos");

    // Los clientes cortan sin avisar; el servidor lo detecta al escribir (cambio de estado
    // o `: ping` cada 15 s) y libera las plazas.
    for s in subs.drain(..) {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    // Forzar escrituras: un cambio de estado hace que todos los hilos SSE escriban.
    let _ = srv.post(
        "/api/v1/command",
        &[
            ("Authorization", &bearer),
            ("Sec-Fetch-Site", "same-origin"),
        ],
        r#"{"command":"/mode plan"}"#,
    );
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut reopened = false;
    while Instant::now() < deadline {
        let mut s = srv.connect();
        s.write_all(
            build_request("GET", "/api/v1/events", &[("Authorization", &bearer)], None).as_bytes(),
        )
        .unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut buf = [0u8; 64];
        if let Ok(n) = s.read(&mut buf) {
            if buf[..n].starts_with(b"HTTP/1.1 200") {
                reopened = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        reopened,
        "las plazas SSE no se liberaron tras desconectar a los clientes"
    );
}
