//! `SDD-04` Fase 4B — `TEST-04.2` (HTTP): E2E de `HttpMcpClient` (Streamable
//! HTTP) contra un *mock* HTTP MCP **in-proceso** sobre `127.0.0.1:0`
//! (portable Windows+Linux, hermetic: sin red real ni python/node en CI).
//!
//! Cobertura:
//! - Handshake completo (`initialize` → `notifications/initialized` →
//!   `tools/list` → `tools/call`) con respuesta **JSON directo**.
//! - Respuesta **SSE** (`data:` hasta el JSON-RPC con el id esperado).
//! - `Mcp-Session-Id` persistente: el mock rechaza (400) `tools/list`/
//!   `tools/call` sin la cabecera tras el `initialize`.
//! - `headers = { Authorization = "Bearer ${VAR}" }`: expansión en request
//!   time (el mock recibe el valor expandido en cada request).
//! - `scrub_secrets` en el resultado de `tools/call` (token `ghp_*` roto).
//! - Aislamiento de errores HTTP: servidor caído (`Failed`) junto a uno sano;
//!   error HTTP 500 mapeado a `McpError::Transport` accionable.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value as JsonValue, json};

use onyx_workspace::mcp::{McpRegistry, McpServerConfig, McpServerStatus, McpTransport};

const SESSION_ID: &str = "onyx-mcp-e2e-session-42";
const LEAKY_TOKEN: &str = "ghp_0a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p";

/// Estado compartido con el mock (lo escribe el thread del mock; los tests
/// asercionan sobre él al terminar).
struct MockState {
    /// Responder `tools/*` en SSE (en vez de JSON directo).
    sse: bool,
    /// `tools/call` responde `500` (prueba de error HTTP mapeado).
    fail_tools_call: AtomicBool,
    /// Resultado de `tools/call` para la herramienta `leaky`.
    leaky_result: String,
    session_ids: Mutex<Vec<String>>,
    auth_headers: Mutex<Vec<String>>,
}

impl MockState {
    fn new(sse: bool) -> Arc<Self> {
        Arc::new(Self {
            sse,
            fail_tools_call: AtomicBool::new(false),
            leaky_result: format!("token {LEAKY_TOKEN} listo"),
            session_ids: Mutex::new(Vec::new()),
            auth_headers: Mutex::new(Vec::new()),
        })
    }
}

/// Arranca el mock en un hilo; devuelve el listener (el puerto lo decide el
/// test con `local_addr()`).
fn spawn_mock(listener: TcpListener, state: Arc<MockState>) {
    thread::spawn(move || {
        for stream in listener.incoming().filter_map(Result::ok) {
            let mut stream = stream;
            let _ = handle_conn(&mut stream, &state);
        }
    });
}

fn handle_conn(stream: &mut TcpStream, state: &MockState) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    // Cabeceras hasta el CRLF vacío.
    let mut head = String::new();
    let mut line = String::new();
    while !head.contains("\r\n\r\n") {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        head.push_str(&line);
    }
    let mut content_length = 0usize;
    let mut session_id: Option<String> = None;
    let mut auth: Option<String> = None;
    for hl in head.lines().skip(1) {
        if let Some((k, v)) = hl.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = v.trim().parse().unwrap_or(0),
                "mcp-session-id" => session_id = Some(v.trim().to_string()),
                "authorization" => auth = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let req: JsonValue = serde_json::from_slice(&body).unwrap_or(json!({}));
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned();

    if let Some(a) = &auth {
        state.auth_headers.lock().unwrap().push(a.clone());
    }
    if let Some(s) = &session_id {
        state.session_ids.lock().unwrap().push(s.clone());
    }

    // El mock exige `Mcp-Session-Id` en `tools/list`/`tools/call`: si el
    // cliente no la persiste, esto responde 400 y la suite falla.
    if matches!(method, "tools/list" | "tools/call") && session_id.as_deref() != Some(SESSION_ID) {
        return write_response(
            stream,
            400,
            "application/json",
            "missing Mcp-Session-Id",
            None,
        );
    }

    let (status, ct, payload, with_session) = match method {
        "initialize" => (
            200,
            "application/json",
            json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": { "name": "onyx-mock-http", "version": "0.1" }
                }
            })
            .to_string(),
            true,
        ),
        "notifications/initialized" => (202, "application/json", String::new(), false),
        "tools/list" => (
            200,
            "application/json",
            json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "tools": [
                    { "name": "echo", "description": "echo de texto",
                      "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } },
                    { "name": "leaky", "description": "filtra un token",
                      "inputSchema": { "type": "object", "properties": {} } }
                ] }
            })
            .to_string(),
            true,
        ),
        "tools/call" if state.fail_tools_call.load(Ordering::SeqCst) => (
            500,
            "application/json",
            format!("boom interno: {LEAKY_TOKEN}"),
            true,
        ),
        "tools/call" => {
            let tool = req
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let result = if tool == "leaky" {
                state.leaky_result.clone()
            } else {
                let text = req
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .and_then(|a| a.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                format!("echo:{text}")
            };
            (
                200,
                "application/json",
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "content": [ { "type": "text", "text": result } ] }
                })
                .to_string(),
                true,
            )
        }
        _ => (404, "application/json", "unknown method".to_string(), true),
    };

    if state.sse && status != 202 {
        // SSE: el payload JSON-RPC va como evento `data:` (el cliente debe
        // leerlo hasta el id esperado).
        let body = format!("event: message\r\ndata: {payload}\r\n\r\n");
        write_response(
            stream,
            200,
            "text/event-stream",
            &body,
            if with_session { Some(SESSION_ID) } else { None },
        )
    } else {
        write_response(
            stream,
            status,
            ct,
            &payload,
            if with_session { Some(SESSION_ID) } else { None },
        )
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    ct: &str,
    body: &str,
    session: Option<&str>,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let mut out = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {ct}\r\n");
    if let Some(s) = session {
        out.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    out.push_str(body);
    stream.write_all(out.as_bytes())?;
    stream.flush()
}

/// Arranca un mock y devuelve `(url, state)`.
fn start_mock(sse: bool) -> (String, Arc<MockState>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local_addr").port();
    let state = MockState::new(sse);
    spawn_mock(listener, state.clone());
    (format!("http://127.0.0.1:{port}/mcp"), state)
}

fn http_cfg(url: &str, headers: Vec<(&str, &str)>, timeout_secs: u64) -> McpServerConfig {
    McpServerConfig {
        transport: McpTransport::Http {
            url: url.to_string(),
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        },
        timeout_secs,
        enabled: true,
    }
}

/// Puerto loopback que no está escuchando (fallo HTTP rápido y hermetic).
fn dead_loopback_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = l.local_addr().expect("local_addr").port();
    drop(l);
    port
}

#[test]
fn http_e2e_json_direct_with_session_and_scrub() {
    unsafe { std::env::set_var("ONYX_MCP_HTTP_E2E_TOKEN_JSON", "super-secret-4b-json") };
    let (url, state) = start_mock(false);
    let mut configs = BTreeMap::new();
    configs.insert(
        "mock".to_string(),
        http_cfg(
            &url,
            vec![("Authorization", "Bearer ${ONYX_MCP_HTTP_E2E_TOKEN_JSON}")],
            15,
        ),
    );
    let registry = McpRegistry::discover_from_config(&configs, 2048);
    let statuses = registry.server_statuses();
    assert_eq!(statuses.len(), 1);
    assert!(
        matches!(statuses[0].1, McpServerStatus::Connected { tool_count: 2 }),
        "expected Connected{{tool_count: 2}}, got {:?}",
        statuses[0].1
    );

    let schemas = registry.tool_schemas();
    let echo = schemas.iter().find(|s| s.name == "mcp__mock__echo");
    assert!(echo.is_some(), "mcp__mock__echo missing: {schemas:?}");
    assert!(
        schemas
            .iter()
            .all(|s| s.name.as_ref().starts_with("mcp__") && s.requires_approval),
        "todas las schemas mcp__* deben requerir aprobación"
    );

    // JSON directo: echo funciona de punta a punta.
    let echo_out = registry
        .call("mcp__mock__echo", &json!({ "text": "hola http" }))
        .expect("echo debe responder");
    assert_eq!(echo_out, "echo:hola http");

    // scrub_secrets: el token `ghp_*` del resultado se roba.
    let leaky = registry.call("mcp__mock__leaky", &json!({}));
    let leaky = leaky.expect("leaky debe responder");
    assert!(
        !leaky.contains(LEAKY_TOKEN),
        "el token no debe filtrar al resultado: {leaky}"
    );

    // `Mcp-Session-Id` persistida: todas las peticiones tras `initialize`
    // (tools/list, tools/call x2) llegaron con la cabecera.
    let sessions = state.session_ids.lock().unwrap().clone();
    assert!(sessions.len() >= 3, "peticiones con session: {sessions:?}");
    assert!(
        sessions.iter().skip(1).all(|s| s == SESSION_ID),
        "post-initialize debe llevar la session: {sessions:?}"
    );

    // `headers ${VAR}` expandidas en request time: el mock vio el valor.
    let auths = state.auth_headers.lock().unwrap().clone();
    assert!(!auths.is_empty(), "el mock debe haber visto Authorization");
    assert!(
        auths.iter().all(|a| a == "Bearer super-secret-4b-json"),
        "expansión de ${{VAR}} en headers: {auths:?}"
    );
}

#[test]
fn http_e2e_sse_response() {
    unsafe { std::env::set_var("ONYX_MCP_HTTP_E2E_TOKEN_SSE", "super-secret-4b-sse") };
    let (url, state) = start_mock(true);
    let mut configs = BTreeMap::new();
    configs.insert(
        "sse".to_string(),
        http_cfg(
            &url,
            vec![("Authorization", "Bearer ${ONYX_MCP_HTTP_E2E_TOKEN_SSE}")],
            15,
        ),
    );
    let registry = McpRegistry::discover_from_config(&configs, 2048);
    let statuses = registry.server_statuses();
    assert!(
        matches!(statuses[0].1, McpServerStatus::Connected { tool_count: 2 }),
        "handshake SSE falló: {:?}",
        statuses[0].1
    );
    let out = registry
        .call("mcp__sse__echo", &json!({ "text": "por sse" }))
        .expect("echo SSE debe responder");
    assert_eq!(out, "echo:por sse");
    let sessions = state.session_ids.lock().unwrap().clone();
    assert!(
        sessions.iter().skip(1).all(|s| s == SESSION_ID),
        "SSE debe persistir la session igualmente: {sessions:?}"
    );
}

#[test]
fn http_e2e_failure_isolated_from_healthy_server() {
    let (url, _state) = start_mock(false);
    let dead_port = dead_loopback_port();
    let mut configs = BTreeMap::new();
    configs.insert("good".to_string(), http_cfg(&url, vec![], 15));
    configs.insert(
        "dead".to_string(),
        http_cfg(&format!("http://127.0.0.1:{dead_port}/mcp"), vec![], 3),
    );
    let registry = McpRegistry::discover_from_config(&configs, 2048);
    let statuses = registry.server_statuses();
    let status_of = |n: &str| {
        statuses
            .iter()
            .find(|(name, _)| name == n)
            .unwrap()
            .1
            .clone()
    };
    assert!(
        matches!(
            status_of("good"),
            McpServerStatus::Connected { tool_count: 2 }
        ),
        "good: {:?}",
        status_of("good")
    );
    match &status_of("dead") {
        McpServerStatus::Failed(msg) => {
            assert!(!msg.is_empty(), "el fallo debe reportar su causa");
        }
        other => panic!("dead debe ser Failed, no {other:?}"),
    }
    // El servidor caído no afecta al sano.
    let out = registry
        .call("mcp__good__echo", &json!({ "text": "sigo vivo" }))
        .expect("el servidor sano debe seguir respondiendo");
    assert_eq!(out, "echo:sigo vivo");
}

#[test]
fn http_e2e_missing_header_var_fails_actionably() {
    let (url, _state) = start_mock(false);
    let mut configs = BTreeMap::new();
    configs.insert(
        "unvar".to_string(),
        http_cfg(
            &url,
            vec![("Authorization", "Bearer ${ONYX_MCP_HTTP_E2E_NO_EXIST_XYZ}")],
            15,
        ),
    );
    let registry = McpRegistry::discover_from_config(&configs, 2048);
    let statuses = registry.server_statuses();
    match &statuses[0].1 {
        McpServerStatus::Failed(msg) => {
            assert!(msg.contains("header"), "debe indicar la cabecera: {msg}");
        }
        other => panic!("sin la variable debe fallar, no {other:?}"),
    }
}

#[test]
fn http_e2e_http_500_mapped_to_transport_error() {
    let (url, state) = start_mock(false);
    state.fail_tools_call.store(true, Ordering::SeqCst);
    let mut configs = BTreeMap::new();
    configs.insert("boom".to_string(), http_cfg(&url, vec![], 15));
    let registry = McpRegistry::discover_from_config(&configs, 2048);
    // El handshake (initialize/tools/list) sigue siendo 200.
    assert!(
        matches!(
            registry.server_statuses()[0].1,
            McpServerStatus::Connected { tool_count: 2 }
        ),
        "{:?}",
        registry.server_statuses()[0].1
    );
    let err = registry
        .call("mcp__boom__echo", &json!({ "text": "x" }))
        .expect_err("tools/call con 500 debe fallar");
    assert!(
        err.to_string().contains("HTTP 500"),
        "error accionable con el código: {err}"
    );
    // El cuerpo del 500 (con el token) no puede filtrar al error.
    assert!(!err.to_string().contains(LEAKY_TOKEN), "{err}");
}
