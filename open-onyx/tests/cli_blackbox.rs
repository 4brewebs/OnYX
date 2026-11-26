//! Caja negra de la CLI `openonyx` en los modos sin claves (`--help`, `--version`,
//! `replay`, `inspect`, `rewind --dry-run`, `doctor`, validación de `serve`).
//!
//! Invariantes: entrada inválida ⇒ código de salida distinto de cero, mensaje en
//! stderr y nunca un `panicked`; ningún comando se cuelga (tiempo límite).

mod common_web;

use common_web::{TempWorkspace, hermetic_command, run_with_timeout};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(30);

fn run(ws: &TempWorkspace, args: &[&str]) -> common::Finished {
    let mut cmd = hermetic_command(&ws.home);
    cmd.current_dir(&ws.ws).args(args);
    run_with_timeout(cmd, LIMIT).unwrap_or_else(|e| panic!("openonyx {args:?}: {e}"))
}

fn assert_clean_failure(out: &common::Finished, args: &[&str], must_mention: &[&str]) {
    assert!(
        matches!(out.code, Some(c) if c != 0),
        "openonyx {args:?} debía fallar; code={:?} stdout={} stderr={}",
        out.code,
        out.stdout,
        out.stderr
    );
    assert!(
        !out.stderr.contains("panicked"),
        "openonyx {args:?} hizo panic: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("RUST_BACKTRACE"),
        "openonyx {args:?}: {}",
        out.stderr
    );
    for m in must_mention {
        assert!(
            out.stderr.contains(m),
            "openonyx {args:?}: stderr debería mencionar {m:?}: {}",
            out.stderr
        );
    }
}

#[test]
fn help_and_version_succeed_and_list_every_subcommand() {
    let ws = TempWorkspace::new("cli_help");
    for flag in ["--help", "-h", "help"] {
        let out = run(&ws, &[flag]);
        assert_eq!(out.code, Some(0), "{flag}: {}", out.stderr);
        for sub in [
            "chat", "run", "replay", "inspect", "rewind", "bench", "doctor", "trust", "serve",
        ] {
            assert!(
                out.stdout.contains(&format!("openonyx {sub}")),
                "{flag}: falta `openonyx {sub}` en la ayuda"
            );
        }
    }
    for flag in ["--version", "-V"] {
        let out = run(&ws, &[flag]);
        assert_eq!(out.code, Some(0));
        assert_eq!(
            out.stdout.trim(),
            format!("openOnyx {}", env!("CARGO_PKG_VERSION"))
        );
    }
    // Ayuda por subcomando.
    for sub in ["chat", "inspect", "serve", "replay"] {
        let out = run(&ws, &[sub, "--help"]);
        assert_eq!(out.code, Some(0), "{sub} --help: {}", out.stderr);
        assert!(out.stdout.contains("Usage"), "{sub} --help");
    }
}

#[test]
fn usage_errors_exit_2_with_a_specific_message() {
    let ws = TempWorkspace::new("cli_usage");
    let cases: &[(&[&str], &str)] = &[
        (&["bogus-subcommand"], "Usage"),
        (&["chat", "--bogus-flag"], "unknown flag `--bogus-flag`"),
        (&["replay"], "replay requires <transcript>"),
        (&["inspect"], "inspect requires <snapshot_dir>"),
        (&["inspect", ".", "--tick", "abc"], "--tick must be a u64"),
        (&["inspect", ".", "--tick", "-1"], "--tick must be a u64"),
        (&["rewind", ".", "notanumber", "--dry-run"], "must be a u64"),
    ];
    for (args, msg) in cases {
        let out = run(&ws, args);
        assert_eq!(out.code, Some(2), "{args:?}: {}", out.stderr);
        assert_clean_failure(&out, args, &[msg]);
    }
    let out = run(&ws, &[]);
    assert_eq!(out.code, Some(2), "sin argumentos imprime uso y sale con 2");
}

#[test]
fn replay_rejects_empty_truncated_and_random_transcripts() {
    let ws = TempWorkspace::new("cli_replay");
    let d = &ws.ws;
    std::fs::write(d.join("empty.tr"), b"").unwrap();
    std::fs::write(d.join("magic_only.tr"), b"ONYXTR2").unwrap();
    std::fs::write(d.join("trunc.tr"), b"ONYXTR2\x00\x01\x02garbage").unwrap();
    std::fs::write(d.join("v1trunc.tr"), b"ONYXTR1\xff\xff\xff\xff").unwrap();
    let mut rng = common::Rng::new(0x5EED);
    let random: Vec<u8> = (0..4096).map(|_| rng.next_u64() as u8).collect();
    std::fs::write(d.join("random.tr"), &random).unwrap();
    std::fs::create_dir_all(d.join("a_dir.tr")).unwrap();
    for f in [
        "empty.tr",
        "magic_only.tr",
        "trunc.tr",
        "v1trunc.tr",
        "random.tr",
        "a_dir.tr",
    ] {
        let p = d.join(f);
        let args = [
            "replay",
            p.to_str().unwrap(),
            "--workspace",
            d.to_str().unwrap(),
        ];
        let out = run(&ws, &args);
        assert_clean_failure(&out, &args, &[]);
        assert!(!out.stderr.trim().is_empty(), "{f}: sin mensaje de error");
    }
    // Replay no debe mutar el workspace ante entradas rotas (0 escrituras).
    let mut names: Vec<String> = std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "a_dir.tr",
            "empty.tr",
            "magic_only.tr",
            "random.tr",
            "trunc.tr",
            "v1trunc.tr"
        ],
        "replay fallido creó ficheros en el workspace"
    );
}

#[test]
fn inspect_and_dry_run_rewind_fail_cleanly_on_missing_or_corrupt_snapshots() {
    let ws = TempWorkspace::new("cli_snap");
    let d = &ws.ws;
    let corrupt = d.join("corrupt");
    std::fs::create_dir_all(&corrupt).unwrap();
    std::fs::write(
        corrupt.join("snapshots.onyxpack"),
        b"ONYXPACK\x00\x00junkjunkjunk",
    )
    .unwrap();
    std::fs::write(corrupt.join("tick-00000003.bin"), b"\x00\x01not a snapshot").unwrap();
    let missing = d.join("does-not-exist");
    let cs = corrupt.to_str().unwrap();
    let ms = missing.to_str().unwrap();
    let wsd = d.to_str().unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec!["inspect", ms],
        vec!["inspect", cs],
        vec!["inspect", cs, "--json"],
        vec!["inspect", cs, "--list"],
        vec!["inspect", cs, "--tick", "3"],
        vec!["rewind", ms, "3", "--dry-run", "--workspace", wsd],
        vec!["rewind", cs, "3", "--dry-run", "--workspace", wsd],
        vec![
            "rewind",
            cs,
            "18446744073709551615",
            "--dry-run",
            "--workspace",
            wsd,
        ],
    ];
    for args in &cases {
        let out = run(&ws, args);
        assert_clean_failure(&out, args, &[]);
    }
    // `--dry-run` no toca el workspace.
    assert!(!d.join(".onyx").exists(), "rewind --dry-run creó .onyx/");
}

#[test]
fn doctor_reports_bad_transcripts_as_errors_and_exits_non_zero() {
    let ws = TempWorkspace::new("cli_doctor");
    let d = &ws.ws;
    std::fs::write(d.join("bad.tr"), b"definitely not a transcript").unwrap();
    let bad = d.join("bad.tr");
    let args = [
        "doctor",
        "--workspace",
        d.to_str().unwrap(),
        "--transcript",
        bad.to_str().unwrap(),
    ];
    let out = run(&ws, &args);
    assert_clean_failure(&out, &args, &["error"]);
    assert!(
        out.stdout.contains("[ERR]"),
        "doctor marca el transcript con [ERR]: {}",
        out.stdout
    );
    assert!(out.stdout.contains("bad.tr"), "{}", out.stdout);
}

#[test]
fn serve_refuses_unsafe_configurations_before_binding() {
    let ws = TempWorkspace::new("cli_serve");
    let wsd = ws.ws.to_str().unwrap().to_string();
    let base = [
        "serve",
        "--workspace",
        wsd.as_str(),
        "--provider",
        "openai",
        "--api-base",
        "http://127.0.0.1:9/v1",
    ];
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["--bind", "0.0.0.0:0"], "without an explicit API key"),
        (
            vec!["--bind", "0.0.0.0:0", "--api-key", "short-key-15chr"],
            "shorter than 16",
        ),
        (
            vec!["--bind", "0.0.0.0:0", "--api-key", "                      "],
            "--api-key must not be empty",
        ),
        (vec!["--bind", "nonsense"], "invalid --bind address"),
        (vec!["--bind", "127.0.0.1"], "invalid --bind address"),
        (
            vec!["--bind", "127.0.0.1:0", "--cors-origin", "*"],
            "wildcard",
        ),
        (
            vec!["--bind", "127.0.0.1:0", "--cors-origin", "localhost:5173"],
            "invalid --cors-origin",
        ),
        (
            vec!["--bind", "127.0.0.1:0", "--cors-origin", "file:///x"],
            "invalid --cors-origin",
        ),
    ];
    for (extra, msg) in cases {
        let args: Vec<&str> = base.iter().copied().chain(extra.iter().copied()).collect();
        let out = run(&ws, &args);
        assert_clean_failure(&out, &args, &[msg]);
        assert!(
            !out.stdout.contains("API Key:"),
            "{args:?} llegó a arrancar: {}",
            out.stdout
        );
    }
}

#[test]
fn serve_without_any_provider_key_fails_with_actionable_message() {
    let ws = TempWorkspace::new("cli_nokey");
    let args = [
        "serve",
        "--bind",
        "127.0.0.1:0",
        "--workspace",
        ws.ws.to_str().unwrap(),
    ];
    let out = run(&ws, &args);
    assert_clean_failure(&out, &args, &["GEMINI_API_KEY"]);
}

/// BUG: los errores de E/S se imprimen con `Debug` (`Error: Os { code: 2, kind: NotFound,
/// message: "No such file or directory" }`) sin decir qué ruta falló.
#[test]
#[ignore = "BUG: FIX-92 replay/doctor con ruta inexistente imprimen `Os { code: 2, .. }` sin la ruta"]
fn missing_paths_are_named_in_the_error_message() {
    let ws = TempWorkspace::new("cli_missing");
    let missing = ws.ws.join("forja-missing.tr");
    let ms = missing.to_str().unwrap();
    let wsd = ws.ws.to_str().unwrap();
    let missing_ws = ws.ws.join("forja-missing-ws");
    let mws = missing_ws.to_str().unwrap();
    for args in [
        vec!["replay", ms, "--workspace", wsd],
        vec!["doctor", "--workspace", mws],
    ] {
        let out = run(&ws, &args);
        assert_clean_failure(&out, &args, &["forja-missing"]);
    }
}
