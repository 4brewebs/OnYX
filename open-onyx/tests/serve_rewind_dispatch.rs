//! `openonyx serve` + `/rewind` en vivo: tras restaurar un milestone, el siguiente turno
//! reutiliza el mismo par `(agent, request)` que ya se despachó antes del rewind. Es una
//! petición NUEVA y debe llegar al proveedor (sitio del conjunto `dispatched` en
//! `DynamicLlmDriver`, `open-onyx/src/llm_driver.rs:152`).
//!
//! Caja negra: binario + proveedor falso OpenAI-compatible en loopback (solo `std::net`).
#![cfg(unix)]

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::{FINISH_STOP, FakeOpenAi, Serve, Step, TempDir, delta, run_goal};

fn text(t: &str) -> Step {
    Step::Sse {
        events: vec![delta(t), FINISH_STOP.to_string()],
        done: true,
    }
}

fn state_json(serve: &Serve) -> serde_json::Value {
    serde_json::from_str(&serve.state()).expect("state is JSON")
}

fn root_says(serve: &Serve) -> (String, Vec<String>) {
    let v = state_json(serve);
    let agent = &v["agents"][0];
    let says = agent["memory"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter(|e| e["kind"] == "say")
                .filter_map(|e| e["text"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (
        agent["state"].as_str().unwrap_or_default().to_string(),
        says,
    )
}

/// Envía un turno de continuación y espera a que el proveedor reciba una petición que
/// contenga `marker` y el agente vuelva a `done` con `expected` como último `say`
/// (tope 15 s). Devuelve el estado y los `say` observados al terminar o agotar el plazo.
fn continue_turn(
    serve: &Serve,
    fake: &FakeOpenAi,
    goal: &str,
    expected: &str,
) -> (bool, String, Vec<String>) {
    assert_eq!(serve.command(goal), r#"{"ok":true}"#);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reached = fake.requests().iter().any(|r| r.contains(goal));
        let (state, says) = root_says(serve);
        if reached && state == "done" && says.last().map(String::as_str) == Some(expected) {
            return (true, state, says);
        }
        if state == "failed" || Instant::now() >= deadline {
            return (reached, state, says);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// El pack de snapshots vive en `snaps/pack`, no en `snaps` directamente: `/rewind` crea una
/// rama hermana `pack.fork-t<N>` y, así, esa rama queda DENTRO del `TempDir` y se borra con
/// él (antes quedaba `/tmp/onyx_sse_*_snaps_*.fork-t2` para siempre). `--fork-dir` no sirve
/// aquí: solo afecta a `--resume`, no al `/rewind` en vivo.
fn serve_with_snapshots(fake: &FakeOpenAi, snaps: &TempDir) -> Serve {
    let pack = snaps.0.join("pack");
    Serve::start_with(
        &fake.base_url(),
        &["--snapshot-dir", pack.to_str().unwrap()],
    )
}

/// Control: tres turnos seguidos sin rewind llegan al proveedor y terminan.
#[test]
fn three_turns_without_rewind_reach_the_provider() {
    let fake = FakeOpenAi::start(vec![text("uno"), text("dos"), text("tres")]);
    let snaps = TempDir::new("snaps_ctrl");
    let serve = serve_with_snapshots(&fake, &snaps);
    assert_eq!(run_goal(&serve, "primero").0, "done");
    let (ok, state, says) = continue_turn(&serve, &fake, "segundo", "dos");
    assert!(ok, "turn 2: state={state} says={says:?}");
    let (ok, state, says) = continue_turn(&serve, &fake, "tercero", "tres");
    assert!(ok, "turn 3: state={state} says={says:?}");
    assert_eq!(fake.requests().len(), 3);
}

/// Rewind en vivo al milestone del turno 1 y nuevo turno: debe despacharse al proveedor.
#[test]
#[ignore = "BUG: FIX-01 DynamicLlmDriver::dispatched no se limpia tras /rewind; el turno nuevo reutiliza (agent, request) y nunca se envía al proveedor (open-onyx/src/llm_driver.rs:152)"]
fn new_turn_after_live_rewind_is_dispatched_to_the_provider() {
    let fake = FakeOpenAi::start(vec![text("uno"), text("dos"), text("tres")]);
    let snaps = TempDir::new("snaps_rewind");
    let serve = serve_with_snapshots(&fake, &snaps);

    assert_eq!(run_goal(&serve, "primero").0, "done");
    let tick_after_turn1 = state_json(&serve)["tick"].as_u64().unwrap();
    let (ok, state, says) = continue_turn(&serve, &fake, "segundo", "dos");
    assert!(ok, "turn 2: state={state} says={says:?}");

    assert_eq!(
        serve.command(&format!("/rewind {tick_after_turn1}")),
        r#"{"ok":true}"#
    );
    // El rewind se aplica en el bucle: espera a que «dos» desaparezca de la memoria.
    let deadline = Instant::now() + Duration::from_secs(10);
    while root_says(&serve).1.iter().any(|s| s == "dos") {
        assert!(Instant::now() < deadline, "rewind was not applied");
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(fake.requests().len(), 2);

    let (ok, state, says) = continue_turn(&serve, &fake, "tercero", "tres");
    let reqs = fake.requests().len();
    assert!(
        ok,
        "turn after rewind must reach the provider and finish: requests={reqs} state={state} says={says:?}"
    );
}
