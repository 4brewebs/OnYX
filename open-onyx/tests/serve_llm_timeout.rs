//! `openonyx serve` + proveedor LLM colgado (`FIX-02`, criterio 3): una
//! petición encolgada en el proveedor (timeout de read) **no impide** que
//! `/api/v1/health` responda, y vencido el tope el agente va a `failed` en
//! tiempo acotado y `/rewind` se procesa y aplica.
//!
//! Caja negra: binario + proveedor falso OpenAI-compatible en loopback (solo
//! `std::net`). `--llm-timeout-read 1` / `--llm-timeout-connect 2` mantienen
//! el test acotado (3 intentos × 1 s + backoff 400 + 1200 ms ≈ 4,6 s).
#![cfg(unix)]

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::{FakeOpenAi, Serve, Step, TempDir, delta, run_goal};

fn state_json(serve: &Serve) -> serde_json::Value {
    serde_json::from_str(&serve.state()).expect("state is JSON")
}

fn root_state(serve: &Serve) -> String {
    state_json(serve)["agents"][0]["state"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Control: con `--llm-timeout-*` fijados, una respuesta normal sigue llegando
/// (los timeouts no rompen a un proveedor sano).
#[test]
fn normal_response_unaffected_by_llm_timeout_flags() {
    let fake = FakeOpenAi::start(vec![Step::Sse {
        events: vec![delta("ok"), common::FINISH_STOP.to_string()],
        done: true,
    }]);
    let serve = Serve::start_with(
        &fake.base_url(),
        &["--llm-timeout-connect", "2", "--llm-timeout-read", "1"],
    );
    let (state, texts) = run_goal(&serve, "hola");
    assert_eq!(state, "done", "{texts:?}");
    assert_eq!(texts.last().map(String::as_str), Some("ok"));
}

/// `FIX-02` (criterio 3): proveedor que acepta la conexión y no responde nunca.
#[test]
fn hung_llm_request_keeps_health_up_and_rewind_works_after_timeout() {
    // El proveedor acepta la conexión y se queda mudo (`Step::Silent`).
    let fake = FakeOpenAi::start(vec![Step::Silent]);
    let snaps = TempDir::new("snaps_llmto");
    let pack = snaps.0.join("pack");
    let serve = Serve::start_with(
        &fake.base_url(),
        &[
            "--snapshot-dir",
            pack.to_str().unwrap(),
            "--llm-timeout-connect",
            "2",
            "--llm-timeout-read",
            "1",
        ],
    );

    // Baseline: health responde antes del objetivo.
    let health = serve.http("GET", "/api/v1/health", "");
    assert!(health.contains("ok"), "health baseline: {health}");
    // Milestone `init` (tick 0) garantizado por `MilestoneSnapshotSystem`.
    let tick_pre = state_json(&serve)["tick"].as_u64().unwrap();

    // Objetivo: la petición se encolga en el proveedor mudo.
    assert_eq!(serve.command("colgar"), r#"{"ok":true}"#);

    // Espera a que el proveedor reciba la petición: a partir de ahí el
    // cliente está bloqueado en el read (vencimiento ≈ 4,6 s).
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.requests().is_empty() {
        assert!(
            Instant::now() < deadline,
            "el proveedor nunca recibió la petición; stderr: {}",
            serve.stderr()
        );
        thread::sleep(Duration::from_millis(50));
    }

    // Con la petición colgada: `/api/v1/health` debe responder a tiempo.
    let start = Instant::now();
    let health = serve.http("GET", "/api/v1/health", "");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "health no respondió con la petición colgada ({:?}): {health}",
        start.elapsed()
    );
    assert!(health.contains("ok"), "health durante el colgón: {health}");

    // Vencido el tope: el agente llega a `failed` en tiempo acotado.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut state_now = String::new();
    while Instant::now() < deadline {
        state_now = root_state(&serve);
        if state_now == "failed" {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        state_now, "failed",
        "el agente debe fallar con el timeout acotado (state: {state_now}); stderr: {}",
        serve.stderr()
    );

    // Tras el timeout: `/rewind` al milestone previo al objetivo se procesa
    // y se aplica. El agente raíz nace con el objetivo: restaurado el
    // milestone pre-objetivo, el agente ya no existe en el grafo (estado
    // vacío) o, si ese milestone lo incluye, está `idle`. En ningún caso
    // sigue `failed`.
    assert_eq!(
        serve.command(&format!("/rewind {tick_pre}")),
        r#"{"ok":true}"#
    );
    let rewind_applied = |s: &str| s.is_empty() || s == "idle";
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut s = String::new();
    while Instant::now() < deadline {
        s = root_state(&serve);
        if rewind_applied(&s) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        rewind_applied(&s),
        "el /rewind no se aplicó (state: {s:?}); stderr: {}",
        serve.stderr()
    );
}
