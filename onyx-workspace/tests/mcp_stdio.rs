//! `SDD-04` Fase 3B — `TEST-04.2` y `TEST-04.3` sobre el núcleo MCP stdio.
//!
//! **Diseño portable (Windows + Linux, sin python/node en CI):** este binario es
//! a la vez el *mock de servidor MCP* y la *suite de aserciones*. Cuando `cargo test`
//! lo invoca (sin argumentos) corre la suite; cuando `StdioMcpClient` lo spawnea con
//! `--mcp-mock-server` (vía `std::env::current_exe()`), habla JSON-RPC 2.0 por stdio.
//!
//! Cobertura:
//! - `TEST-04.2`: handshake completo (`initialize` → `notifications/initialized` →
//!   `tools/list` → `tools/call`), expansión de `${ENV_VAR}` solo en spawn,
//!   truncación `truncate_utf8`, `scrub_secrets` en resultados Y errores, timeout
//!   real que mata el proceso, y aislamiento de fallos (servidor roto junto a uno
//!   sano no aborta la sesión).
//! - `TEST-04.3`: `deny` exacto y por glob (`mcp__mock__*`) bloquean antes de
//!   ejecutar; `WorkspaceMode::Plan`/`Explore` → `Deny` para cualquier `mcp__*`
//!   (aunque haya `allow`); en `Build` sin reglas → `Ask` por defecto.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value as JsonValue, json};

use onyx_workspace::mcp::{McpRegistry, McpServerConfig, McpServerStatus, McpTransport};
use onyx_workspace::policy::{PermissionDecision, PermissionRules, WorkspacePolicy};
use onyx_workspace::state::WorkspaceMode;

const MOCK_ROLE_FLAG: &str = "--mcp-mock-server";
const GREETING: &str = "saludo-prueba-043b";
const LEAKY_TOKEN: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
const BOOM_TOKEN: &str = "ghp_00001111222233334444555566667777";

fn main() {
    // 1) Rol servidor: hablar JSON-RPC en stdio y no devolver nunca.
    if std::env::args().any(|a| a == MOCK_ROLE_FLAG) {
        run_mock_server();
    }

    // 2) Rol suite: aserciones TEST-04.2 + TEST-04.3.
    unsafe { std::env::set_var("MOCK_GREETING_VALUE", GREETING) };

    let exe = std::env::current_exe().expect("current_exe");
    let mock_cfg = |timeout_secs: u64| McpServerConfig {
        transport: McpTransport::Stdio {
            command: exe.to_string_lossy().into_owned(),
            args: vec![MOCK_ROLE_FLAG.to_string()],
            env: vec![(
                "MOCK_GREETING".to_string(),
                "${MOCK_GREETING_VALUE}".to_string(),
            )],
        },
        timeout_secs,
        enabled: true,
    };

    let mut configs = BTreeMap::new();
    configs.insert(
        "broken".to_string(),
        McpServerConfig {
            transport: McpTransport::Stdio {
                command: "definitely-missing-onyx-mcp-binary-xyz".to_string(),
                args: Vec::new(),
                env: Vec::new(),
            },
            timeout_secs: 2,
            enabled: true,
        },
    );
    configs.insert("mock".to_string(), mock_cfg(15));
    configs.insert(
        "off".to_string(),
        McpServerConfig {
            transport: McpTransport::Stdio {
                command: "x".to_string(),
                args: Vec::new(),
                env: Vec::new(),
            },
            timeout_secs: 20,
            enabled: false,
        },
    );
    // `remote`: transporte HTTP contra un puerto loopback muerto (hermetic:
    // sin red real). La Fase 4B handshakea de verdad → `Failed` rápido.
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("local_addr").port()
    };
    configs.insert(
        "remote".to_string(),
        McpServerConfig {
            transport: McpTransport::Http {
                url: format!("http://127.0.0.1:{dead_port}/mcp"),
                headers: Vec::new(),
            },
            timeout_secs: 5,
            enabled: true,
        },
    );
    configs.insert("slowmock".to_string(), mock_cfg(1));

    let max_output_bytes: usize = 2048;
    let registry = McpRegistry::discover_from_config(&configs, max_output_bytes);
    let statuses = registry.server_statuses();

    let mut failures: Vec<String> = Vec::new();
    macro_rules! check {
        ($name:literal, $cond:expr) => {
            let name = $name;
            if $cond {
                println!("PASS  {name}");
            } else {
                println!("FAIL  {name}");
                failures.push(name.to_string());
            }
        };
    }

    // ── TEST-04.2: descubrimiento + aislamiento ──
    let status_of = |name: &str| -> Option<&McpServerStatus> {
        statuses.iter().find(|(n, _)| n == name).map(|(_, s)| s)
    };
    check!(
        "discover: mock Connected con 6 herramientas",
        matches!(
            status_of("mock"),
            Some(McpServerStatus::Connected { tool_count: 6 })
        )
    );
    check!(
        "discover: broken → Failed (aislado, no aborta la sesión)",
        matches!(status_of("broken"), Some(McpServerStatus::Failed(_)))
    );
    check!(
        "discover: off → Disabled",
        status_of("off") == Some(&McpServerStatus::Disabled)
    );
    check!(
        "discover: remote (HTTP caído) → Failed (aislado, no aborta)",
        matches!(status_of("remote"), Some(McpServerStatus::Failed(_)))
    );
    check!(
        "discover: slowmock Connected",
        matches!(
            status_of("slowmock"),
            Some(McpServerStatus::Connected { .. })
        )
    );

    // Schemas inyectables: siempre `requires_approval = true` (REQ-04.4).
    let schemas = registry.tool_schemas();
    let echo_schema = schemas.iter().find(|s| s.name == "mcp__mock__echo");
    check!("schemas: mcp__mock__echo presente", echo_schema.is_some());
    check!(
        "schemas: requires_approval = true para mcp__*",
        schemas
            .iter()
            .filter(|s| s.name.as_ref().starts_with("mcp__"))
            .all(|s| s.requires_approval)
    );

    // ── TEST-04.2: JSON-RPC end-to-end ──
    let echo = registry.call("mcp__mock__echo", &json!({ "text": "hola mundo" }));
    check!(
        "e2e: echo responde",
        echo.as_ref().is_ok_and(|t| t == "hola mundo")
    );

    // Expansión ${ENV_VAR} solo en el momento del spawn (el mock la devuelve).
    let envval = registry.call("mcp__mock__envval", &json!({}));
    check!(
        "e2e: env ${MOCK_GREETING_VALUE} expandido en spawn",
        envval.as_ref().is_ok_and(|t| t == GREETING)
    );

    // Truncación UTF-8 a `max_output_bytes` con marcador.
    let big = registry.call("mcp__mock__big", &json!({}));
    check!(
        "e2e: big truncado a max_output_bytes",
        big.as_ref().is_ok_and(|text| {
            text.len() <= max_output_bytes + 16 && text.ends_with("...[truncated]")
        })
    );

    // scrub_secrets en resultado: token `ghp_` + 32 chars → `[REDACTED]`.
    let leaky = registry.call("mcp__mock__leaky", &json!({}));
    check!(
        "e2e: leaky redactado (scrub_secrets en resultado)",
        leaky
            .as_deref()
            .is_ok_and(|t| t.contains("[REDACTED]") && !t.contains(LEAKY_TOKEN))
    );

    // scrub_secrets en error: `isError: true` con token → `[REDACTED]`.
    let boom = registry.call("mcp__mock__boom", &json!({}));
    check!(
        "e2e: boom (isError) redactado (scrub_secrets en error)",
        boom.as_ref().is_err_and(|e| {
            let msg = e.to_string();
            msg.contains("[REDACTED]") && !msg.contains(BOOM_TOKEN)
        })
    );

    // Timeout real: `slow` duerme 3 s; `timeout_secs = 1` vence y mata el proceso.
    let started = std::time::Instant::now();
    let slow = registry.call("mcp__slowmock__slow", &json!({}));
    let elapsed = started.elapsed();
    check!(
        "e2e: slow → timeout real (1 s) que mata el proceso",
        slow.is_err()
            && slow
                .as_ref()
                .is_err_and(|e| e.to_string().contains("timeout"))
            && elapsed >= Duration::from_millis(900)
            && elapsed < Duration::from_secs(3)
    );

    // Aislamiento: el mock sigue vivo y funcional tras el fallo de `broken`
    // y el timeout de `slowmock`.
    let echo_after = registry.call("mcp__mock__echo", &json!({ "text": "sigue vivo" }));
    check!(
        "aislamiento: mock sano tras fallos de otros servidores",
        echo_after.as_ref().is_ok_and(|t| t == "sigue vivo")
    );

    // Nombres malformados / servidores desconocidos.
    let malformed = registry.call("mcp__mock", &json!({}));
    check!(
        "call: nombre malformado rechazado",
        malformed
            .as_ref()
            .is_err_and(|e| e.to_string().contains("malformed"))
    );
    let unknown = registry.call("mcp__nope__tool", &json!({}));
    check!(
        "call: servidor desconocido rechazado",
        unknown
            .as_ref()
            .is_err_and(|e| e.to_string().contains("unknown MCP server"))
    );

    // ── TEST-04.3: policy (deny exacto/glob, Plan → Deny, Build → Ask) ──
    let base = WorkspacePolicy::new(std::env::temp_dir()).expect("workspace policy");

    // (a) `deny` exacto bloquea antes de ejecutar.
    let deny_exact = base.clone().with_permissions(PermissionRules {
        deny: vec!["mcp__mock__echo".to_string()],
        allow: vec!["mcp__mock__echo".to_string()], // el allow NO vence al deny
        ask: Vec::new(),
    });
    check!(
        "policy: deny exacto bloquea (aunque haya allow)",
        deny_exact.evaluate_permission_in_mode(WorkspaceMode::Build, "mcp__mock__echo", "{}")
            == PermissionDecision::Deny
    );

    // (b) `deny` por glob `mcp__mock__*` bloquea todas las herramientas del servidor.
    let deny_glob = base.clone().with_permissions(PermissionRules {
        deny: vec!["mcp__mock__*".to_string()],
        allow: Vec::new(),
        ask: Vec::new(),
    });
    check!(
        "policy: deny glob mcp__mock__* bloquea echo",
        deny_glob.evaluate_permission_in_mode(WorkspaceMode::Build, "mcp__mock__echo", "{}")
            == PermissionDecision::Deny
    );
    check!(
        "policy: deny glob mcp__mock__* bloquea big",
        deny_glob.evaluate_permission_in_mode(WorkspaceMode::Build, "mcp__mock__big", "{}")
            == PermissionDecision::Deny
    );
    check!(
        "policy: deny glob no afecta a otros servidores",
        deny_glob.evaluate_permission_in_mode(WorkspaceMode::Build, "mcp__slowmock__slow", "{}")
            == PermissionDecision::Ask
    );

    // (c) `allow` específico en Build.
    let allow_exact = base.clone().with_permissions(PermissionRules {
        deny: Vec::new(),
        allow: vec!["mcp__mock__echo".to_string()],
        ask: Vec::new(),
    });
    check!(
        "policy: allow exacto permite en Build",
        allow_exact.evaluate_permission_in_mode(WorkspaceMode::Build, "mcp__mock__echo", "{}")
            == PermissionDecision::Allow
    );

    // (d) Fallback sin reglas: cualquier `mcp__*` → `Ask` en Build (REQ-04.4).
    check!(
        "policy: fallback sin reglas → Ask en Build",
        base.evaluate_permission_in_mode(WorkspaceMode::Build, "mcp__mock__echo", "{}")
            == PermissionDecision::Ask
    );
    check!(
        "policy: herramientas no-MCP sin reglas siguen Allow",
        base.evaluate_permission_in_mode(WorkspaceMode::Build, "fs_read", "{}")
            == PermissionDecision::Allow
    );

    // (e) `Plan` y `Explore`: cualquier `mcp__*` → `Deny`, aunque haya `allow`.
    let allow_all = base.clone().with_permissions(PermissionRules {
        deny: Vec::new(),
        allow: vec!["mcp__*".to_string()],
        ask: Vec::new(),
    });
    check!(
        "policy: Plan → Deny para mcp__* (aunque haya allow)",
        allow_all.evaluate_permission_in_mode(WorkspaceMode::Plan, "mcp__mock__echo", "{}")
            == PermissionDecision::Deny
    );
    check!(
        "policy: Explore → Deny para mcp__* (aunque haya allow)",
        allow_all.evaluate_permission_in_mode(WorkspaceMode::Explore, "mcp__mock__echo", "{}")
            == PermissionDecision::Deny
    );
    check!(
        "policy: Plan → Allow para fs_read (no afectado)",
        allow_all.evaluate_permission_in_mode(WorkspaceMode::Plan, "fs_read", "{}")
            == PermissionDecision::Allow
    );

    if failures.is_empty() {
        println!("mcp_stdio: OK (todas las aserciones pasaron)");
    } else {
        println!("mcp_stdio: {} aserciones fallaron:", failures.len());
        for f in &failures {
            println!("  - {f}");
        }
        std::process::exit(1);
    }
}

/// Servidor MCP *mock* sobre stdio (JSON-RPC 2.0). Herramientas:
/// `echo`, `big` (~100 KB), `leaky` (token `ghp_`), `slow` (3 s), `boom`
/// (`isError: true` con token) y `envval` (valor de `MOCK_GREETING`).
/// Emite además una notificación `notifications/log` tras `initialize` para
/// verificar que el cliente ignora líneas sin su `id`.
fn run_mock_server() -> ! {
    let mut out = std::io::stdout();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<JsonValue>(line) else {
            continue; // tolerante: líneas no-JSON se ignoran
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        match method {
            "initialize" => {
                if let Some(id) = id {
                    emit(
                        &mut out,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "onyx-mock", "version": "0.1" }
                            }
                        }),
                    );
                    // Ruido sin `id`: el cliente debe ignorarlo.
                    emit(
                        &mut out,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/log",
                            "params": { "level": "info", "data": "mock started" }
                        }),
                    );
                }
            }
            "notifications/initialized" => {}
            "tools/list" => {
                if let Some(id) = id {
                    let tool = |name: &str| {
                        json!({
                            "name": name,
                            "description": format!("mock tool {name}"),
                            "inputSchema": {
                                "type": "object",
                                "properties": if name == "echo" {
                                    json!({ "text": { "type": "string" } })
                                } else {
                                    json!({})
                                }
                            }
                        })
                    };
                    emit(
                        &mut out,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [
                                    tool("echo"),
                                    tool("big"),
                                    tool("leaky"),
                                    tool("slow"),
                                    tool("boom"),
                                    tool("envval")
                                ]
                            }
                        }),
                    );
                }
            }
            "tools/call" => {
                let Some(id) = id else { continue };
                let tool = req
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let args = req
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let result: JsonValue = match tool {
                    "echo" => json!({
                        "content": [{
                            "type": "text",
                            "text": args.get("text").and_then(|t| t.as_str()).unwrap_or("")
                        }]
                    }),
                    "big" => {
                        const CHUNK: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
                        let mut text = String::new();
                        for _ in 0..(100_000 / CHUNK.len()) {
                            text.push_str(CHUNK);
                        }
                        text.truncate(100_000);
                        json!({ "content": [{ "type": "text", "text": text }] })
                    }
                    "leaky" => json!({
                        "content": [{ "type": "text", "text": format!("token={LEAKY_TOKEN} end") }]
                    }),
                    "slow" => {
                        std::thread::sleep(Duration::from_secs(3));
                        json!({ "content": [{ "type": "text", "text": "done-slow" }] })
                    }
                    "boom" => json!({
                        "isError": true,
                        "content": [{ "type": "text", "text": format!("boom failed {BOOM_TOKEN}") }]
                    }),
                    "envval" => {
                        let v = std::env::var("MOCK_GREETING")
                            .unwrap_or_else(|_| "<missing>".to_string());
                        json!({ "content": [{ "type": "text", "text": v }] })
                    }
                    other => {
                        emit(
                            &mut out,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32601, "message": format!("unknown tool `{other}`") }
                            }),
                        );
                        continue;
                    }
                };
                emit(
                    &mut out,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                );
            }
            other => {
                if let Some(id) = id {
                    emit(
                        &mut out,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": format!("unknown method `{other}`") }
                        }),
                    );
                }
            }
        }
    }
    std::process::exit(0);
}

fn emit(out: &mut impl Write, value: &JsonValue) {
    let mut s = value.to_string();
    s.push('\n');
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}
