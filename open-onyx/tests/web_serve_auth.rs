//! Caja negra de `openonyx serve`: autenticación, cookies, CSRF (`Sec-Fetch-Site`),
//! CORS (`--cors-origin`) y cabeceras de seguridad, contra el binario real en loopback.
//!
//! Contrato de referencia: `open-onyx/README.md` (sección `serve` y `src/web.rs`) y el
//! propio código de `dispatch_http_request`.

mod common;

use common::{ServeGuard, ServeOpts, build_request};
use std::time::{Duration, Instant};

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'; require-trusted-types-for 'script'";

/// Lee el modo del snapshot (`BUILD`/`PLAN`/`EXPLORE`) con Bearer.
fn mode_of(srv: &ServeGuard) -> String {
    let r = srv.get("/api/v1/state", &[("Authorization", &srv.bearer())]);
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["mode"].as_str().unwrap_or("").to_string()
}

fn wait_mode(srv: &ServeGuard, want: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if mode_of(srv) == want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn health_is_public_but_every_api_route_requires_auth() {
    let srv = ServeGuard::start("auth_public");
    let h = srv.get("/api/v1/health", &[]);
    assert_eq!(h.status, 200);
    assert_eq!(h.json()["auth_required"], serde_json::json!(true));

    for (method, path) in [
        ("GET", "/api/v1/state"),
        ("GET", "/api/v1/events"),
        ("GET", "/api/v1/files?q=a"),
        ("POST", "/api/v1/command"),
        ("POST", "/api/v1/approval"),
        ("POST", "/api/v1/focus"),
        ("POST", "/api/v1/queue/clear"),
        ("GET", "/api/v1/does-not-exist"),
    ] {
        let body = (method == "POST").then_some("{}");
        let r = srv.request(&build_request(method, path, &[], body));
        assert_eq!(
            r.status,
            401,
            "{method} {path} sin credenciales: {}",
            r.text()
        );
        assert_eq!(
            r.header("www-authenticate"),
            Some("Bearer realm=\"openOnyx\""),
            "{method} {path}"
        );
        assert!(
            !r.text().contains(&srv.api_key),
            "401 nunca debe reflejar la API key"
        );
    }
}

#[test]
fn bearer_must_match_exactly_and_state_never_leaks_the_key() {
    let srv = ServeGuard::start("auth_bearer");
    let key = srv.api_key.clone();
    assert!(
        key.starts_with("onyx_") && key.len() == 5 + 64,
        "clave efímera 256 bits hex: {key}"
    );

    let ok = srv.get(
        "/api/v1/state",
        &[("Authorization", &format!("Bearer {key}"))],
    );
    assert_eq!(ok.status, 200);
    let state = ok.json();
    assert_eq!(state["schema"], "onyx.web.state.v1");
    assert!(
        !ok.text().contains(&key),
        "el snapshot no debe contener la API key"
    );

    // Mismo largo con el último carácter cambiado, prefijo, sufijo, vacío y otros esquemas.
    let mut flipped = key.clone();
    let last = flipped.pop().unwrap();
    flipped.push(if last == '0' { '1' } else { '0' });
    let bad: Vec<String> = vec![
        format!("Bearer {flipped}"),
        format!("Bearer {}", &key[..key.len() - 1]),
        format!("Bearer {key}x"),
        format!("Bearer {}", key.to_ascii_uppercase()),
        "Bearer ".to_string(),
        "Bearer".to_string(),
        format!("Basic {key}"),
        format!("Token {key}"),
        key.clone(),
    ];
    for auth in &bad {
        let r = srv.get("/api/v1/state", &[("Authorization", auth)]);
        assert_eq!(r.status, 401, "Authorization {auth:?} debe rechazarse");
    }
    // La API key como cookie de sesión tampoco vale: la cookie es un token derivado.
    let r = srv.get(
        "/api/v1/state",
        &[("Cookie", &format!("onyx_session={key}"))],
    );
    assert_eq!(r.status, 401, "la API key no debe aceptarse como cookie");
}

#[test]
fn credentials_from_a_previous_server_instance_are_rejected() {
    // "Caducado": una clave/cookie de otro proceso `serve` (otra sesión) no vale.
    let (old_key, old_cookie) = {
        let a = ServeGuard::start("auth_old");
        (a.api_key.clone(), a.login_cookie())
    };
    let b = ServeGuard::start("auth_new");
    assert_ne!(
        old_key, b.api_key,
        "cada arranque en loopback genera clave nueva"
    );
    let r = b.get(
        "/api/v1/state",
        &[("Authorization", &format!("Bearer {old_key}"))],
    );
    assert_eq!(r.status, 401);
    let r = b.get("/api/v1/state", &[("Cookie", &old_cookie)]);
    assert_eq!(r.status, 401, "cookie de otra instancia: {old_cookie}");
}

#[test]
fn login_validates_body_and_key_and_sets_hardened_cookie() {
    let srv = ServeGuard::start("auth_login");
    let sfs = ("Sec-Fetch-Site", "same-origin");

    for body in ["", "{", "[]", "{\"api_key\":1}", "{\"key\":\"x\"}", "null"] {
        let r = srv.post("/api/v1/auth/login", &[sfs], body);
        assert_eq!(r.status, 400, "cuerpo {body:?}");
        assert!(
            r.header("set-cookie").is_none(),
            "sin cookie con cuerpo {body:?}"
        );
    }
    let r = srv.post("/api/v1/auth/login", &[sfs], r#"{"api_key":"onyx_wrong"}"#);
    assert_eq!(r.status, 401);
    assert!(
        r.header("set-cookie").is_none(),
        "credencial errónea no emite cookie"
    );

    let body = format!(r#"{{"api_key":"{}"}}"#, srv.api_key);
    let r = srv.post("/api/v1/auth/login", &[sfs], &body);
    assert_eq!(r.status, 200);
    let cookie = r.header("set-cookie").unwrap().to_string();
    let attrs: Vec<&str> = cookie.split(';').map(str::trim).collect();
    let (name, value) = attrs[0].split_once('=').unwrap();
    assert_eq!(
        name, "onyx_session",
        "en HTTP plano no se usa `__Host-` (exige Secure)"
    );
    assert!(value.len() >= 32, "token de sesión con entropía: {value}");
    assert_ne!(value, srv.api_key, "la cookie no es la API key");
    assert!(!value.contains(&srv.api_key));
    assert!(attrs.contains(&"HttpOnly"), "{cookie}");
    assert!(
        attrs.contains(&"SameSite=Lax") || attrs.contains(&"SameSite=Strict"),
        "{cookie}"
    );
    assert!(attrs.contains(&"Path=/"), "{cookie}");
    assert!(!attrs.iter().any(|a| a.starts_with("Domain")), "{cookie}");

    // La cookie autentica GET /state.
    let r = srv.get("/api/v1/state", &[("Cookie", attrs[0])]);
    assert_eq!(r.status, 200);
    // Cookie manipulada: 401.
    let tampered = format!("onyx_session={}x", value);
    assert_eq!(
        srv.get("/api/v1/state", &[("Cookie", &tampered)]).status,
        401
    );
    // Cookie dentro de una cabecera con otras cookies.
    let mixed = format!("theme=dark; {}; other=1", attrs[0]);
    assert_eq!(srv.get("/api/v1/state", &[("Cookie", &mixed)]).status, 200);
}

#[test]
fn login_behind_https_proxy_uses_host_prefixed_secure_cookie() {
    let srv = ServeGuard::start("auth_host_prefix");
    let body = format!(r#"{{"api_key":"{}"}}"#, srv.api_key);
    let r = srv.post(
        "/api/v1/auth/login",
        &[
            ("Sec-Fetch-Site", "same-origin"),
            ("X-Forwarded-Proto", "https"),
        ],
        &body,
    );
    assert_eq!(r.status, 200);
    let cookie = r.header("set-cookie").unwrap().to_string();
    let attrs: Vec<&str> = cookie.split(';').map(str::trim).collect();
    // Reglas del prefijo `__Host-` (RFC 6265bis): Secure, Path=/ y sin Domain.
    assert!(attrs[0].starts_with("__Host-onyx_session="), "{cookie}");
    assert!(attrs.contains(&"Secure"), "{cookie}");
    assert!(attrs.contains(&"HttpOnly"), "{cookie}");
    assert!(attrs.contains(&"Path=/"), "{cookie}");
    assert!(!attrs.iter().any(|a| a.starts_with("Domain")), "{cookie}");
    assert_eq!(
        srv.get("/api/v1/state", &[("Cookie", attrs[0])]).status,
        200
    );
}

#[test]
fn logout_expires_both_cookie_names_and_clears_site_data() {
    let srv = ServeGuard::start("auth_logout");
    let r = srv.post(
        "/api/v1/auth/logout",
        &[("Sec-Fetch-Site", "same-origin")],
        "",
    );
    assert_eq!(r.status, 200);
    let cookies = r.headers_all("set-cookie");
    for name in ["onyx_session=", "__Host-onyx_session="] {
        let c = cookies
            .iter()
            .find(|c| c.starts_with(name))
            .unwrap_or_else(|| panic!("falta Set-Cookie {name}: {cookies:?}"));
        assert!(c.contains("Max-Age=0"), "{c}");
        assert!(c.starts_with(&format!("{name};")), "valor vacío: {c}");
    }
    let csd = r.header("clear-site-data").unwrap_or("");
    assert!(csd.contains("\"cookies\""), "{csd}");
}

#[test]
fn cross_site_posts_are_rejected_before_auth_and_do_not_mutate() {
    let srv = ServeGuard::start("csrf");
    let cookie = srv.login_cookie();
    let bearer = srv.bearer();
    assert_eq!(mode_of(&srv), "BUILD");

    let cmd = r#"{"command":"/mode plan"}"#;
    let mutating = [
        ("/api/v1/command", cmd),
        (
            "/api/v1/approval",
            r#"{"agent_id":1,"request":1,"approve":true}"#,
        ),
        ("/api/v1/focus", r#"{"agent_id":1}"#),
        ("/api/v1/queue/clear", "{}"),
        ("/api/v1/auth/login", "{\"api_key\":\"x\"}"),
        ("/api/v1/auth/logout", ""),
    ];
    for site in ["cross-site", "same-site", "Cross-Site", " CROSS-SITE "] {
        for (path, body) in mutating {
            for auth in [
                ("Cookie", cookie.as_str()),
                ("Authorization", bearer.as_str()),
            ] {
                let r = srv.post(path, &[("Sec-Fetch-Site", site), auth], body);
                assert_eq!(r.status, 403, "{site} {path} con {}: {}", auth.0, r.text());
                assert!(
                    r.header("set-cookie").is_none(),
                    "403 sin Set-Cookie ({path})"
                );
            }
        }
    }
    // La login CSRF con la clave buena también se bloquea.
    let good = format!(r#"{{"api_key":"{}"}}"#, srv.api_key);
    let r = srv.post(
        "/api/v1/auth/login",
        &[("Sec-Fetch-Site", "cross-site")],
        &good,
    );
    assert_eq!(r.status, 403);

    // Ningún `/mode plan` cross-site llegó al motor.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(mode_of(&srv), "BUILD", "un POST cross-site mutó el modo");

    // Control positivo: el mismo comando same-origin sí muta (prueba que el canal observa).
    let r = srv.post(
        "/api/v1/command",
        &[("Sec-Fetch-Site", "same-origin"), ("Cookie", &cookie)],
        cmd,
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(
        wait_mode(&srv, "PLAN", Duration::from_secs(10)),
        "same-origin /mode plan no aplicó"
    );

    // `Sec-Fetch-Site: none` (navegación directa / CLI) está permitido.
    let r = srv.post(
        "/api/v1/command",
        &[("Sec-Fetch-Site", "none"), ("Authorization", &bearer)],
        r#"{"command":"/mode build"}"#,
    );
    assert_eq!(r.status, 200);
    assert!(wait_mode(&srv, "BUILD", Duration::from_secs(10)));
}

#[test]
fn cors_origin_is_echoed_only_for_the_allowlisted_origin() {
    let allowed = "http://localhost:5173";
    let srv = ServeGuard::start_with(
        "cors",
        ServeOpts {
            cors_origin: Some("http://localhost:5173/"),
            ..ServeOpts::default()
        },
    );
    let bearer = srv.bearer();

    // Preflight con origen permitido.
    let pre = srv.request(&build_request(
        "OPTIONS",
        "/api/v1/command",
        &[
            ("Origin", allowed),
            ("Access-Control-Request-Method", "POST"),
            (
                "Access-Control-Request-Headers",
                "authorization, content-type",
            ),
        ],
        None,
    ));
    assert_eq!(pre.status, 204);
    assert_eq!(pre.header("access-control-allow-origin"), Some(allowed));
    assert_eq!(pre.header("access-control-allow-credentials"), Some("true"));
    let methods = pre.header("access-control-allow-methods").unwrap_or("");
    assert!(
        methods.contains("POST") && !methods.contains('*'),
        "{methods}"
    );

    // Preflight y GET con orígenes ajenos o parecidos: sin ACAO.
    for evil in [
        "http://evil.example",
        "http://localhost:5174",
        "https://localhost:5173",
        "http://localhost:5173.evil.example",
        "null",
        "*",
    ] {
        let pre = srv.request(&build_request(
            "OPTIONS",
            "/api/v1/state",
            &[("Origin", evil)],
            None,
        ));
        assert!(
            pre.header("access-control-allow-origin").is_none(),
            "preflight {evil}"
        );
        let r = srv.get(
            "/api/v1/state",
            &[("Origin", evil), ("Authorization", &bearer)],
        );
        assert!(
            r.header("access-control-allow-origin").is_none(),
            "GET {evil}"
        );
    }

    // SPA externa con Bearer + origen permitido puede POSTear aunque sea cross-site.
    let r = srv.post(
        "/api/v1/queue/clear",
        &[
            ("Origin", allowed),
            ("Sec-Fetch-Site", "cross-site"),
            ("Authorization", &bearer),
        ],
        "{}",
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("access-control-allow-origin"), Some(allowed));

    // ...pero no con cookie sola (sin Bearer) ni desde otro origen con Bearer.
    let cookie = srv.login_cookie();
    let r = srv.post(
        "/api/v1/queue/clear",
        &[
            ("Origin", allowed),
            ("Sec-Fetch-Site", "cross-site"),
            ("Cookie", &cookie),
        ],
        "{}",
    );
    assert_eq!(r.status, 403, "cross-site con cookie y sin Bearer");
    let r = srv.post(
        "/api/v1/queue/clear",
        &[
            ("Origin", "http://evil.example"),
            ("Sec-Fetch-Site", "cross-site"),
            ("Authorization", &bearer),
        ],
        "{}",
    );
    assert_eq!(r.status, 403, "cross-site con Bearer desde origen ajeno");
}

#[test]
fn without_cors_flag_no_response_ever_carries_acao() {
    let srv = ServeGuard::start("cors_off");
    let bearer = srv.bearer();
    for path in ["/", "/app.js", "/api/v1/health", "/api/v1/state"] {
        let r = srv.get(
            path,
            &[
                ("Origin", "http://localhost:5173"),
                ("Authorization", &bearer),
            ],
        );
        assert!(r.header("access-control-allow-origin").is_none(), "{path}");
    }
    let r = srv.request(&build_request(
        "OPTIONS",
        "/api/v1/command",
        &[("Origin", "http://localhost:5173")],
        None,
    ));
    assert!(r.header("access-control-allow-origin").is_none());
    // Bearer válido + cross-site sin CORS configurado ⇒ 403.
    let r = srv.post(
        "/api/v1/queue/clear",
        &[
            ("Origin", "http://localhost:5173"),
            ("Sec-Fetch-Site", "cross-site"),
            ("Authorization", &bearer),
        ],
        "{}",
    );
    assert_eq!(r.status, 403);
}

#[test]
fn foreign_host_header_grants_nothing() {
    // DNS rebinding: un Host ajeno no cambia la decisión de auth ni filtra el estado.
    let srv = ServeGuard::start("host");
    for host in [
        "evil.example",
        "127.0.0.1.evil.example:8787",
        "",
        "localhost:1",
    ] {
        let raw = format!("GET /api/v1/state HTTP/1.1\r\nHost: {host}\r\n\r\n");
        let r = srv.request(&raw);
        assert_eq!(r.status, 401, "Host {host:?}");
        let raw = format!(
            "GET /api/v1/state HTTP/1.1\r\nHost: {host}\r\nX-Forwarded-For: 127.0.0.1\r\nX-Real-IP: 127.0.0.1\r\n\r\n"
        );
        assert_eq!(
            srv.request(&raw).status,
            401,
            "cabeceras de proxy no autentican"
        );
    }
}

#[test]
fn every_response_carries_the_promised_security_headers() {
    let srv = ServeGuard::start("headers");
    let bearer = srv.bearer();
    let cases: Vec<(String, common::Response)> = vec![
        ("GET /".into(), srv.get("/", &[])),
        ("GET /app.js".into(), srv.get("/app.js", &[])),
        ("GET /api/v1/health".into(), srv.get("/api/v1/health", &[])),
        (
            "GET /api/v1/state 401".into(),
            srv.get("/api/v1/state", &[]),
        ),
        (
            "GET /api/v1/state".into(),
            srv.get("/api/v1/state", &[("Authorization", &bearer)]),
        ),
        (
            "GET 404".into(),
            srv.get("/nope", &[("Authorization", &bearer)]),
        ),
        (
            "POST 403".into(),
            srv.post("/api/v1/command", &[("Sec-Fetch-Site", "cross-site")], "{}"),
        ),
    ];
    for (label, r) in &cases {
        assert_eq!(r.header("content-security-policy"), Some(CSP), "{label}");
        assert_eq!(r.header("x-frame-options"), Some("DENY"), "{label}");
        assert_eq!(
            r.header("x-content-type-options"),
            Some("nosniff"),
            "{label}"
        );
        assert_eq!(r.header("cache-control"), Some("no-store"), "{label}");
        assert_eq!(
            r.header("cross-origin-resource-policy"),
            Some("same-origin"),
            "{label}"
        );
        assert!(r.header("referrer-policy").is_some(), "{label}");
        assert_eq!(r.header("connection"), Some("close"), "{label}");
        // Content-Length exacto: el cliente puede delimitar el cuerpo.
        let cl: usize = r.header("content-length").unwrap().parse().unwrap();
        assert_eq!(cl, r.body.len(), "{label}");
    }
}
