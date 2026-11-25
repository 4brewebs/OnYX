//! Caja negra de `openonyx serve` contra un proveedor falso OpenAI-compatible en loopback
//! (solo `std::net`), centrado en el final del stream SSE (`open-onyx/src/openai.rs`).
//!
//! Contrato OpenAI de streaming: el stream termina con `data: [DONE]` y el último chunk con
//! contenido trae `finish_reason`. Un cierre TCP limpio sin `[DONE]` ni `finish_reason` es un
//! stream truncado: no puede darse por terminado con `finish_reason = "stop"`.
//!
//! Hermético: `env_clear`, `HOME`/`ONYX_TRUST_FILE` en un tempdir, clave falsa, sin red externa.
#![cfg(unix)]

mod common;

use common::{FINISH_STOP, FakeOpenAi, Serve, Step, delta, run_goal};

/// Control: stream completo con `finish_reason: "stop"` y `[DONE]` => `done` con el texto entero.
#[test]
fn complete_stream_with_done_marker_finishes_with_full_text() {
    let fake = FakeOpenAi::start(vec![Step::Sse {
        events: vec![delta("Hola "), delta("mundo"), FINISH_STOP.to_string()],
        done: true,
    }]);
    let serve = Serve::start(&fake.base_url());
    let (state, texts) = run_goal(&serve, "saluda");
    assert_eq!(state, "done", "{texts:?}");
    assert_eq!(texts.last().map(String::as_str), Some("Hola mundo"));
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert!(reqs[0].starts_with("/v1/chat/completions "), "{reqs:?}");
    assert!(
        reqs[0].contains("\"stream\":true"),
        "serve must stream: {reqs:?}"
    );
}

/// Control: cortado a mitad de un evento (JSON incompleto) => no es un `done`.
#[test]
fn stream_cut_in_the_middle_of_an_event_fails() {
    let fake = FakeOpenAi::start(vec![Step::Raw(format!(
        "data: {}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"mun",
        delta("Hola ")
    ))]);
    let serve = Serve::start(&fake.base_url());
    let (state, texts) = run_goal(&serve, "saluda");
    assert_eq!(state, "failed", "{texts:?}");
    assert!(
        !texts.iter().any(|t| t == "Hola "),
        "partial text must not become the answer: {texts:?}"
    );
}

/// Stream truncado: deltas de texto sin `finish_reason` ni `[DONE]` y cierre TCP limpio.
#[test]
#[ignore = "BUG: EOF limpio sin [DONE] ni finish_reason se acepta como finish_reason=stop y el texto truncado queda como respuesta final (open-onyx/src/openai.rs:468-470)"]
fn stream_closed_cleanly_without_done_marker_is_not_a_stop() {
    let fake = FakeOpenAi::start(vec![Step::Sse {
        events: vec![delta("Hola "), delta("mun")],
        done: false,
    }]);
    let serve = Serve::start(&fake.base_url());
    let (state, texts) = run_goal(&serve, "saluda");
    assert_ne!(
        state, "done",
        "truncated stream accepted as complete answer; memory: {texts:?}"
    );
}
