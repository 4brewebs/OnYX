//! Caja negra de `POST /api/v1/approval` sin aprobación pendiente.
//!
//! Contrato (`WebHub::push_web_approval`): sin aprobación activa ⇒ `409 Conflict`
//! (`no pending approval`); cuerpo mal tipado ⇒ `400`. Aprobar dos veces, aprobar algo
//! inexistente o ya resuelto nunca debe tener éxito.
//!
//! Pendiente: el caso con una aprobación real (doble aprobación de la misma petición,
//! aprobación de otra petición mientras hay una activa) necesita un LLM falso que proponga
//! una herramienta mutante; queda para el servidor compatible OpenAI de Colorado.

mod common_web;

use common_web::ServeGuard;

fn approve(srv: &ServeGuard, body: &str) -> common_web::Response {
    srv.post(
        "/api/v1/approval",
        &[
            ("Authorization", &srv.bearer()),
            ("Sec-Fetch-Site", "same-origin"),
        ],
        body,
    )
}

#[test]
fn approving_without_a_pending_request_is_a_conflict_every_time() {
    let srv = ServeGuard::start("approvals_none");
    let st = srv
        .get("/api/v1/state", &[("Authorization", &srv.bearer())])
        .json();
    assert!(
        st["pending_approval"].is_null(),
        "no debería haber aprobación pendiente: {st}"
    );

    for body in [
        r#"{"agent_id":1,"request":1,"approve":true}"#,
        r#"{"agent_id":1,"request":1,"approve":true}"#, // doble
        r#"{"agent_id":1,"request":1,"approve":false,"reason":"no"}"#, // ya "resuelta"
        r#"{"agent_id":0,"request":0,"approve":true}"#,
        r#"{"agent_id":18446744073709551615,"request":18446744073709551615,"approve":true}"#,
    ] {
        let r = approve(&srv, body);
        assert_eq!(r.status, 409, "{body}: {}", r.text());
        assert_eq!(r.json()["error"], "no pending approval", "{body}");
    }
    let st = srv
        .get("/api/v1/state", &[("Authorization", &srv.bearer())])
        .json();
    assert!(st["pending_approval"].is_null());
}

#[test]
fn malformed_approval_bodies_are_bad_requests() {
    let srv = ServeGuard::start("approvals_bad");
    for body in [
        "",
        "{}",
        r#"{"agent_id":1,"request":1}"#,
        r#"{"agent_id":-1,"request":1,"approve":true}"#,
        r#"{"agent_id":"1","request":1,"approve":true}"#,
        r#"{"agent_id":1.5,"request":1,"approve":true}"#,
        r#"{"agent_id":1,"request":1,"approve":"yes"}"#,
        r#"{"agent_id":18446744073709551616,"request":1,"approve":true}"#,
    ] {
        let r = approve(&srv, body);
        assert_eq!(r.status, 400, "{body:?}: {}", r.text());
    }
}

#[test]
fn approval_requires_auth_and_same_origin() {
    let srv = ServeGuard::start("approvals_auth");
    let body = r#"{"agent_id":1,"request":1,"approve":true}"#;
    let r = srv.post(
        "/api/v1/approval",
        &[("Sec-Fetch-Site", "same-origin")],
        body,
    );
    assert_eq!(r.status, 401);
    let r = srv.post(
        "/api/v1/approval",
        &[
            ("Authorization", &srv.bearer()),
            ("Sec-Fetch-Site", "cross-site"),
        ],
        body,
    );
    assert_eq!(r.status, 403);
}
