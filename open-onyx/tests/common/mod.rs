//! Arnés compartido para tests de caja negra de `openonyx`: proveedor falso OpenAI-compatible
//! en loopback (solo `std::net`) y proceso `openonyx serve` hermético.
#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(pub PathBuf);
impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "onyx_sse_{prefix}_{}_{n}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(fs::canonicalize(&dir).unwrap())
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Respuesta SSE del proveedor falso para una petición.
#[derive(Clone)]
pub enum Step {
    /// Escribe cada cadena como evento `data: <json>\n\n` y opcionalmente `data: [DONE]`.
    Sse { events: Vec<String>, done: bool },
    /// Escribe bytes literales tras las cabeceras y cierra (para cortar a mitad de evento).
    Raw(String),
    /// Escribe cada cadena como evento `data: <json>\n\n` y corta la conexión con RST (no FIN),
    /// sin `[DONE]`: el cliente recibe los eventos y después un error de lectura
    /// (`ECONNRESET`), no un fin de stream. Solo Linux: usa `SO_LINGER` con tiempo 0.
    Reset { events: Vec<String> },
}

/// `SO_LINGER {on, 0}`: `close()` descarta lo pendiente y envía RST. Declarado a mano
/// (sin añadir `libc` como dependencia); solo Linux.
#[cfg(target_os = "linux")]
pub fn set_linger_zero(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Linger {
        l_onoff: i32,
        l_linger: i32,
    }
    unsafe extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, val: *const Linger, len: u32) -> i32;
    }
    const SOL_SOCKET: i32 = 1;
    const SO_LINGER: i32 = 13;
    let linger = Linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: `linger` es un `struct linger` válido y vive durante la llamada; el fd es de
    // un socket abierto que `stream` posee.
    let rc = unsafe {
        setsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_LINGER,
            &linger,
            std::mem::size_of::<Linger>() as u32,
        )
    };
    assert_eq!(rc, 0, "setsockopt(SO_LINGER) failed");
}

pub struct FakeOpenAi {
    port: u16,
    stop: Arc<AtomicBool>,
    bodies: Arc<Mutex<Vec<String>>>,
    handle: Option<thread::JoinHandle<()>>,
}

pub fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut r = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut len = 0usize;
    loop {
        line.clear();
        r.read_line(&mut line).ok()?;
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().ok()?;
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some((path, String::from_utf8_lossy(&body).into_owned()))
}

impl FakeOpenAi {
    /// Cada petición consume el siguiente paso; agotado el guion responde 500.
    pub fn start(script: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (stop2, bodies2) = (stop.clone(), bodies.clone());
        let handle = thread::spawn(move || {
            let mut script = script.into_iter();
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut s) = conn else { continue };
                let Some((path, body)) = read_request(&mut s) else {
                    continue;
                };
                bodies2.lock().unwrap().push(format!("{path} {body}"));
                match script.next() {
                    Some(Step::Sse { events, done }) => {
                        let _ = write!(
                            s,
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
                        );
                        for e in events {
                            let _ = write!(s, "data: {e}\n\n");
                            let _ = s.flush();
                        }
                        if done {
                            let _ = write!(s, "data: [DONE]\n\n");
                        }
                    }
                    Some(Step::Raw(raw)) => {
                        let _ = write!(
                            s,
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{raw}"
                        );
                    }
                    #[cfg(target_os = "linux")]
                    Some(Step::Reset { events }) => {
                        let _ = write!(
                            s,
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
                        );
                        for e in events {
                            let _ = write!(s, "data: {e}\n\n");
                            let _ = s.flush();
                        }
                        // Deja que el cliente lea los eventos antes del RST.
                        thread::sleep(Duration::from_millis(200));
                        set_linger_zero(&s);
                        continue; // `drop(s)` al final de la vuelta envía el RST.
                    }
                    #[cfg(not(target_os = "linux"))]
                    Some(Step::Reset { .. }) => panic!("Step::Reset requires Linux"),
                    None => {
                        let body = r#"{"error":"script exhausted"}"#;
                        let _ = write!(
                            s,
                            "HTTP/1.1 500 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                    }
                }
                let _ = s.flush();
                // Cierre TCP limpio (FIN), sin RST.
                let _ = s.shutdown(std::net::Shutdown::Write);
            }
        });
        Self {
            port,
            stop,
            bodies,
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn requests(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

impl Drop for FakeOpenAi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn delta(text: &str) -> String {
    serde_json_like_delta(text, None)
}

pub fn serde_json_like_delta(text: &str, finish: Option<&str>) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    match finish {
        Some(fr) => format!(
            r#"{{"choices":[{{"index":0,"delta":{{"content":"{escaped}"}},"finish_reason":"{fr}"}}]}}"#
        ),
        None => format!(
            r#"{{"choices":[{{"index":0,"delta":{{"content":"{escaped}"}},"finish_reason":null}}]}}"#
        ),
    }
}

pub const FINISH_STOP: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;

/// Proceso `openonyx serve` aislado.
pub struct Serve {
    child: Child,
    pub port: u16,
    _home: TempDir,
    pub ws: TempDir,
    stderr: Arc<Mutex<Vec<u8>>>,
}

/// Tope de stderr que se conserva para diagnóstico; el resto se lee y se descarta para que
/// el pipe nunca se llene y bloquee al servidor.
const STDERR_KEEP: usize = 64 * 1024;

/// Vacía `stderr` del hijo en un hilo (el hilo termina solo al cerrarse el pipe, es decir,
/// cuando el hijo muere) y conserva los primeros `STDERR_KEEP` bytes.
fn drain_stderr(stderr: std::process::ChildStderr) -> Arc<Mutex<Vec<u8>>> {
    let kept: Arc<Mutex<Vec<u8>>> = Arc::default();
    let sink = kept.clone();
    thread::spawn(move || {
        let mut stderr = stderr;
        let mut buf = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Ok(mut kept) = sink.lock() {
                let room = STDERR_KEEP.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    });
    kept
}

pub const API_KEY: &str = "forja-test-key";

impl Serve {
    pub fn start(api_base: &str) -> Self {
        Self::start_with(api_base, &[])
    }

    pub fn start_with(api_base: &str, extra: &[&str]) -> Self {
        let home = TempDir::new("home");
        let ws = TempDir::new("ws");
        fs::write(ws.0.join("README.md"), "fixture\n").unwrap();
        let git_ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&ws.0)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(git_ok, "git is required for openonyx serve fixtures");
        let mut child = Command::new(env!("CARGO_BIN_EXE_openonyx"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &home.0)
            .env("USERPROFILE", &home.0)
            .env("ONYX_TRUST_FILE", home.0.join("trusted.toml"))
            .env("OPENAI_API_KEY", "test-key")
            .env("ONYX_PROVIDER", "openai")
            .env("ONYX_MODEL", "fake-model")
            .args([
                "serve",
                "--workspace",
                ws.0.to_str().unwrap(),
                "--bind",
                "127.0.0.1:0",
                "--api-key",
                API_KEY,
                "--provider",
                "openai",
                "--model",
                "fake-model",
                "--api-base",
                api_base,
                "--trust-workspace",
                "--sandbox",
                "off",
                "--auto-approve",
            ])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn openonyx serve");
        let stdout = child.stdout.take().unwrap();
        let stderr = drain_stderr(child.stderr.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut port = None;
        let mut seen = Vec::new();
        while Instant::now() < deadline && port.is_none() {
            if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
                if let Some(rest) = line.trim().strip_prefix("URL:") {
                    let addr = rest
                        .trim()
                        .trim_start_matches("http://")
                        .trim_end_matches('/');
                    port = addr.rsplit(':').next().and_then(|p| p.parse::<u16>().ok());
                }
                seen.push(line);
            }
        }
        let Some(port) = port else {
            let _ = child.kill();
            let _ = child.wait();
            let err =
                String::from_utf8_lossy(&stderr.lock().map(|e| e.clone()).unwrap_or_default())
                    .into_owned();
            panic!("serve did not print its URL; stdout: {seen:#?}; stderr: {err}");
        };
        Self {
            child,
            port,
            _home: home,
            ws,
            stderr,
        }
    }

    pub fn http(&self, method: &str, path: &str, body: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {API_KEY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out.split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or(out)
    }

    pub fn command(&self, cmd: &str) -> String {
        let body = format!("{{\"command\":{}}}", json_string(cmd));
        self.http("POST", "/api/v1/command", &body)
    }

    pub fn state(&self) -> String {
        self.http("GET", "/api/v1/state", "")
    }

    /// Lo que el servidor ha escrito en stderr hasta ahora (tope `STDERR_KEEP`).
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().map(|e| e.clone()).unwrap_or_default())
            .into_owned()
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn json_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Envía un objetivo y espera a que el agente raíz llegue a `done` o `failed` (tope 20 s).
/// Devuelve `(state, textos say/done de la memoria)`.
pub fn run_goal(serve: &Serve, goal: &str) -> (String, Vec<String>) {
    assert_eq!(serve.command(goal), r#"{"ok":true}"#);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = String::new();
    while Instant::now() < deadline {
        last = serve.state();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&last) {
            if let Some(agent) = v["agents"].as_array().and_then(|a| a.first()) {
                let state = agent["state"].as_str().unwrap_or_default().to_string();
                if state == "done" || state == "failed" {
                    let texts = agent["memory"]
                        .as_array()
                        .map(|m| {
                            m.iter()
                                .filter_map(|e| e["text"].as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    return (state, texts);
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("agent did not reach done/failed within 20s; last state: {last}");
}
