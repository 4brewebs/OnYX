//! Caja negra de la TUI (`openonyx chat`) desde fuera del proceso.
//!
//! - Sin TTY: debe terminar enseguida con error, sin colgarse ni hacer panic.
//! - Con pseudo-terminal real vía `script(1)` de util-linux (sin dependencias nuevas;
//!   solo Linux y solo si `script` existe; si no, el test lo indica y no comprueba nada):
//!   arranque, `Esc`/`Ctrl+C` para salir, `F2`/`F5`, paleta `Ctrl+K` con filtrado y
//!   selección, entrada Unicode con borrado, terminales diminutas y restauración del
//!   terminal (pantalla alternativa, cursor y modo canónico) al salir.
//!
//! Ningún test pulsa `Enter` sobre un prompt: el proveedor apunta a `127.0.0.1:9` y no
//! hay LLM; los flujos con modelo quedan pendientes del servidor falso de Colorado.

mod common_web;

use common_web::{TempWorkspace, hermetic_command, run_with_timeout};
use std::time::Duration;

#[test]
fn chat_without_a_tty_fails_fast_without_panicking() {
    let ws = TempWorkspace::new("tui_notty");
    let mut cmd = hermetic_command(&ws.home);
    cmd.current_dir(&ws.ws).args([
        "chat",
        "--workspace",
        ws.ws.to_str().unwrap(),
        "--provider",
        "openai",
        "--api-base",
        "http://127.0.0.1:9/v1",
        "--model",
        "forja-fake",
    ]);
    let out = run_with_timeout(cmd, Duration::from_secs(20)).expect("chat sin TTY se colgó");
    assert!(matches!(out.code, Some(c) if c != 0), "code={:?}", out.code);
    assert!(!out.stderr.contains("panicked"), "{}", out.stderr);
    // Sin TTY no debe emitir secuencias de pantalla alternativa a un pipe.
    assert!(
        !out.stdout.contains("\x1b[?1049h"),
        "entró en pantalla alternativa sin TTY"
    );
}

#[cfg(target_os = "linux")]
mod pty {
    use super::common::{BIN, TempWorkspace};
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const ESC: &[u8] = b"\x1b";
    const CTRL_C: &[u8] = b"\x03";
    const CTRL_K: &[u8] = b"\x0b";
    const F2: &[u8] = b"\x1bOQ";
    const F5: &[u8] = b"\x1b[15~";
    const BACKSPACE: &[u8] = b"\x7f";

    fn script_available() -> bool {
        Command::new("script")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    struct PtyRun {
        code: Option<i32>,
        raw: String,
        text: String,
    }

    /// Quita secuencias CSI/OSC/SS3 para buscar texto renderizado.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            match it.next() {
                Some('[') => {
                    // Un salto de posición separa palabras: lo marcamos con un espacio.
                    for d in it.by_ref() {
                        if ('@'..='~').contains(&d) {
                            if d == 'H' {
                                out.push('\n');
                            }
                            break;
                        }
                    }
                }
                Some(']') => {
                    for d in it.by_ref() {
                        if d == '\x07' {
                            break;
                        }
                    }
                }
                Some('O') => {
                    it.next();
                }
                _ => {}
            }
        }
        out
    }

    /// Ejecuta `chat` en un pty `cols`x`rows`, envía `keys` (pausa, bytes) y espera la salida.
    fn run_chat(cols: u16, rows: u16, keys: &[(u64, &[u8])]) -> Option<PtyRun> {
        if !script_available() {
            eprintln!("SKIP: `script` (util-linux) no disponible; prueba pty no ejecutada");
            return None;
        }
        let ws = TempWorkspace::new("tui_pty");
        let inner = format!(
            "stty cols {cols} rows {rows}; {BIN} chat --workspace {} --provider openai \
             --api-base http://127.0.0.1:9/v1 --model forja-fake; rc=$?; \
             echo; echo FORJA_STTY: $(stty -a); exit $rc",
            ws.ws.display()
        );
        let mut child = Command::new("script")
            .args(["-qfec", &inner, "/dev/null"])
            .current_dir(&ws.ws)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &ws.home)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn script");
        let mut stdout = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        });
        let mut stdin = child.stdin.take().unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        for (pause, bytes) in keys {
            stdin.write_all(bytes).unwrap();
            stdin.flush().unwrap();
            std::thread::sleep(Duration::from_millis(*pause));
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let code = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break st.code();
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                let raw = reader.join().unwrap();
                panic!(
                    "la TUI no salió en 20 s tras las teclas; salida: {}",
                    strip_ansi(&raw)
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        drop(stdin);
        let raw = reader.join().unwrap();
        let text = strip_ansi(&raw);
        Some(PtyRun { code, raw, text })
    }

    fn assert_restored(run: &PtyRun) {
        assert!(
            !run.raw.contains("panicked"),
            "panic en la TUI: {}",
            run.text
        );
        assert!(
            run.raw.contains("\x1b[?1049h"),
            "no entró en pantalla alternativa"
        );
        let leave = run
            .raw
            .rfind("\x1b[?1049l")
            .expect("no salió de la pantalla alternativa");
        let enter = run.raw.rfind("\x1b[?1049h").unwrap();
        assert!(
            leave > enter,
            "la última transición debe ser salir de la pantalla alternativa"
        );
        assert!(
            run.raw[leave..].contains("\x1b[?25h"),
            "cursor no restaurado tras salir"
        );
        let stty = run
            .text
            .lines()
            .find_map(|l| l.split("FORJA_STTY:").nth(1))
            .expect("línea stty tras salir");
        let words: Vec<&str> = stty.split_whitespace().collect();
        assert!(
            words.contains(&"icanon"),
            "modo raw no restaurado (falta icanon): {stty}"
        );
        assert!(words.contains(&"echo"), "eco no restaurado: {stty}");
    }

    #[test]
    fn esc_in_chat_quits_cleanly_and_restores_the_terminal() {
        let Some(run) = run_chat(100, 30, &[(500, ESC)]) else {
            return;
        };
        assert_eq!(run.code, Some(0), "{}", run.text);
        assert!(run.text.contains("openOnyx"), "cabecera: {}", run.text);
        assert!(run.text.contains("[BUILD]"), "{}", run.text);
        assert!(run.text.contains("Esc:Quit"), "{}", run.text);
        assert_restored(&run);
    }

    #[test]
    fn ctrl_c_quits_cleanly_and_restores_the_terminal() {
        let Some(run) = run_chat(100, 30, &[(500, CTRL_C)]) else {
            return;
        };
        assert_eq!(run.code, Some(0), "{}", run.text);
        assert_restored(&run);
    }

    #[test]
    fn function_keys_switch_views_and_esc_goes_back_before_quitting() {
        let keys: &[(u64, &[u8])] = &[
            (600, F2),
            (600, ESC), // F2 → Chat (no sale)
            (600, F5),
            (600, ESC), // F5 → Chat
            (600, ESC), // Chat → salir
        ];
        let Some(run) = run_chat(220, 32, keys) else {
            return;
        };
        assert_eq!(run.code, Some(0), "{}", run.text);
        assert!(
            run.text.contains("Agents (F2)"),
            "F2 no mostró Agents: {}",
            run.text
        );
        assert!(
            run.text.contains("Timeline (F5)"),
            "F5 no mostró Timeline: {}",
            run.text
        );
        assert!(
            run.text.contains("Back"),
            "en vistas efímeras Esc vuelve: {}",
            run.text
        );
        assert_restored(&run);
    }

    #[test]
    fn palette_filters_selects_and_closes() {
        let keys: &[(u64, &[u8])] = &[
            (700, CTRL_K),
            (300, b"t"),
            (300, b"i"),
            (600, b"m"),
            (700, b"\r"), // única coincidencia: F5 Timeline
            (600, ESC),   // Timeline → Chat
            (600, CTRL_K),
            (600, ESC), // cerrar paleta sin acción
            (600, ESC), // salir
        ];
        let Some(run) = run_chat(120, 32, keys) else {
            return;
        };
        assert_eq!(run.code, Some(0), "{}", run.text);
        assert!(
            run.text.contains("Ctrl+K Palette"),
            "paleta no abierta: {}",
            run.text
        );
        assert!(
            run.text.contains("tim"),
            "query visible en el título: {}",
            run.text
        );
        assert!(
            run.text.contains("Timeline (F5)"),
            "Enter no cambió a Timeline: {}",
            run.text
        );
        assert_restored(&run);
    }

    #[test]
    fn unicode_input_and_editing_do_not_break_the_tui() {
        let keys: &[(u64, &[u8])] = &[
            (400, "héllo 中文 🦀 e\u{301}".as_bytes()),
            (300, BACKSPACE),
            (300, BACKSPACE),
            (300, b"\x1b[H"),    // Home
            (300, b"\x1b[3~"),   // Delete al inicio
            (300, b"\x1b[F"),    // End
            (300, &[b'x'; 600]), // pegado largo
            (500, BACKSPACE),
            (600, ESC),
        ];
        let Some(run) = run_chat(80, 24, keys) else {
            return;
        };
        assert_eq!(run.code, Some(0), "{}", run.text);
        assert!(
            run.text.contains('中') && run.text.contains('文') && run.text.contains('🦀'),
            "CJK/emoji no renderizados: {}",
            run.text
        );
        assert_restored(&run);
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for (cols, rows) in [(1u16, 1u16), (10, 3), (20, 5), (44, 9)] {
            let keys: &[(u64, &[u8])] =
                &[(300, CTRL_K), (300, ESC), (300, F2), (300, ESC), (500, ESC)];
            let Some(run) = run_chat(cols, rows, keys) else {
                return;
            };
            assert_eq!(run.code, Some(0), "{cols}x{rows}: {}", run.text);
            assert_restored(&run);
        }
    }

    #[test]
    fn huge_terminal_renders_and_quits() {
        let Some(run) = run_chat(300, 100, &[(300, CTRL_K), (400, ESC), (500, ESC)]) else {
            return;
        };
        assert_eq!(run.code, Some(0), "{}", run.text);
        assert!(run.text.contains("Ctrl+K Palette"), "{}", run.text);
        assert_restored(&run);
    }
}

/// Mismo origen que el hallazgo de `cli_blackbox`: `main` imprime el `io::Error` con
/// `Debug` (`Error: Os { code: 6, kind: Uncategorized, message: "No such device or
/// address" }`) y el usuario no sabe que falta un terminal interactivo.
#[test]
#[ignore = "BUG: FIX-93 chat sin TTY imprime `Os { code: 6, .. }` en vez de pedir un terminal interactivo"]
fn chat_without_a_tty_explains_that_a_terminal_is_required() {
    let ws = TempWorkspace::new("tui_notty_msg");
    let mut cmd = hermetic_command(&ws.home);
    cmd.current_dir(&ws.ws).args([
        "chat",
        "--workspace",
        ws.ws.to_str().unwrap(),
        "--provider",
        "openai",
        "--api-base",
        "http://127.0.0.1:9/v1",
    ]);
    let out = run_with_timeout(cmd, Duration::from_secs(20)).expect("chat sin TTY se colgó");
    let lower = out.stderr.to_ascii_lowercase();
    assert!(
        lower.contains("tty") || lower.contains("terminal"),
        "el error debería decir que hace falta un terminal: {}",
        out.stderr
    );
}
