//! Arnés compartido de los tests de caja negra de `openonyx` (Forja).
//!
//! - [`TempWorkspace`]: directorio temporal único con `HOME` propio, borrado en `Drop`.
//! - [`ServeGuard`]: lanza `openonyx serve` en `127.0.0.1:0` sin claves LLM reales
//!   (`--provider openai --api-base http://127.0.0.1:9/v1`, nunca contactado mientras no
//!   haya un objetivo), lee la URL y la API key efímera que imprime el binario y mata el
//!   proceso hijo en `Drop`.
//! - Cliente HTTP/1.1 mínimo sobre `std::net::TcpStream` (sin dependencias).
//! - [`Rng`]: generador determinista (xorshift64*) para fuzz con semillas fijas.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_openonyx");

static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// Directorio temporal único (`std::env::temp_dir()` + pid + contador + nanos).
pub struct TempWorkspace {
    pub root: PathBuf,
    pub ws: PathBuf,
    pub home: PathBuf,
}

impl TempWorkspace {
    pub fn new(tag: &str) -> Self {
        let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "forja_openonyx_{tag}_{}_{n}_{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let ws = root.join("ws");
        let home = root.join("home");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Self { root, ws, home }
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `Command` hermético: entorno vacío salvo `PATH`, `HOME` aislado y lo mínimo de Windows.
pub fn hermetic_command(home: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    for keep in ["SystemRoot", "SYSTEMROOT", "TEMP", "TMP", "TMPDIR"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    cmd.env("HOME", home);
    cmd.env("USERPROFILE", home);
    cmd.env("TERM", "dumb");
    cmd
}

/// Salida de un proceso con tiempo límite. `None` si hubo que matarlo.
pub struct Finished {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

/// Ejecuta `cmd` con `stdin` cerrado y lo mata si supera `limit` (devuelve `Err` con lo leído).
pub fn run_with_timeout(mut cmd: Command, limit: Duration) -> Result<Finished, String> {
    let start = Instant::now();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn openonyx");
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t_out = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        String::from_utf8_lossy(&s).into_owned()
    });
    let t_err = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err.read_to_end(&mut s);
        String::from_utf8_lossy(&s).into_owned()
    });
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Ok(Finished {
                code: status.code(),
                stdout: t_out.join().unwrap(),
                stderr: t_err.join().unwrap(),
                elapsed: start.elapsed(),
            });
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "timeout tras {limit:?}; stdout={:?} stderr={:?}",
                t_out.join().unwrap(),
                t_err.join().unwrap()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Proceso `openonyx serve` vivo; se mata en `Drop`.
pub struct ServeGuard {
    child: Child,
    pub addr: SocketAddr,
    pub api_key: String,
    pub ws: TempWorkspace,
    /// Banner impreso por `serve` al arrancar (stdout hasta la línea de la clave).
    pub banner: String,
}

#[derive(Default)]
pub struct ServeOpts<'a> {
    /// Dirección de `--bind` (por defecto `127.0.0.1:0`).
    pub bind: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub cors_origin: Option<&'a str>,
    pub prepare: Option<&'a dyn Fn(&Path)>,
}

impl ServeGuard {
    pub fn start(tag: &str) -> Self {
        Self::start_with(tag, ServeOpts::default())
    }

    pub fn start_with(tag: &str, opts: ServeOpts<'_>) -> Self {
        let ws = TempWorkspace::new(tag);
        if let Some(prep) = opts.prepare {
            prep(&ws.ws);
        }
        let mut cmd = hermetic_command(&ws.home);
        cmd.current_dir(&ws.ws).args([
            "serve",
            "--bind",
            opts.bind.unwrap_or("127.0.0.1:0"),
            "--workspace",
            ws.ws.to_str().unwrap(),
            "--provider",
            "openai",
            "--api-base",
            "http://127.0.0.1:9/v1",
            "--model",
            "forja-fake",
        ]);
        if let Some(k) = opts.api_key {
            cmd.args(["--api-key", k]);
        }
        if let Some(o) = opts.cors_origin {
            cmd.args(["--cors-origin", o]);
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn openonyx serve");
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        // Drenar stderr siempre para que el hijo nunca se bloquee escribiendo.
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = BufReader::new(stderr).read_to_end(&mut sink);
        });
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let _ = tx.send(line);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut addr: Option<SocketAddr> = None;
        let mut key: Option<String> = opts.api_key.map(str::to_string);
        let mut seen = Vec::new();
        while Instant::now() < deadline && (addr.is_none() || key.is_none()) {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    if let Some(rest) = line.trim().strip_prefix("URL:") {
                        let url = rest.trim();
                        let hostport = url.trim_start_matches("http://").trim_end_matches('/');
                        // Bind público (`0.0.0.0`/`[::]`): el cliente entra por loopback.
                        addr = hostport.parse::<SocketAddr>().ok().map(|mut a| {
                            if a.ip().is_unspecified() {
                                a.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
                            }
                            a
                        });
                    }
                    if let Some(rest) = line.trim().strip_prefix("API Key:") {
                        if let Some(tok) = rest.split_whitespace().next() {
                            if tok.starts_with("onyx_") {
                                key = Some(tok.to_string());
                            }
                        }
                    }
                    seen.push(line);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = child.try_wait().unwrap() {
                        panic!("openonyx serve terminó antes de escuchar ({status}): {seen:?}");
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Seguir drenando stdout en segundo plano.
        std::thread::spawn(move || while rx.recv().is_ok() {});
        let (Some(addr), Some(api_key)) = (addr, key) else {
            let _ = child.kill();
            panic!("no se pudo leer URL/API key del banner de serve: {seen:?}");
        };
        let guard = Self {
            child,
            addr,
            api_key,
            ws,
            banner: seen.join("\n"),
        };
        guard.wait_listening();
        guard
    }

    fn wait_listening(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(r) = self.try_request("GET /api/v1/health HTTP/1.1\r\nHost: x\r\n\r\n") {
                if r.status == 200 {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("serve no respondió a /api/v1/health en 10 s");
    }

    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn connect(&self) -> TcpStream {
        let s = TcpStream::connect_timeout(&self.addr, Duration::from_secs(5)).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
        s
    }

    /// Envía bytes crudos y lee la respuesta completa (el servidor cierra tras responder).
    pub fn try_request_bytes(&self, raw: &[u8]) -> std::io::Result<Response> {
        let mut s = self.connect();
        s.write_all(raw)?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf)?;
        Ok(Response::parse(&buf))
    }

    pub fn try_request(&self, raw: &str) -> std::io::Result<Response> {
        self.try_request_bytes(raw.as_bytes())
    }

    pub fn request(&self, raw: &str) -> Response {
        self.try_request(raw).expect("petición HTTP")
    }

    /// `GET path` con cabeceras extra.
    pub fn get(&self, path: &str, extra: &[(&str, &str)]) -> Response {
        self.request(&build_request("GET", path, extra, None))
    }

    pub fn post(&self, path: &str, extra: &[(&str, &str)], body: &str) -> Response {
        self.request(&build_request("POST", path, extra, Some(body)))
    }

    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    /// Login con la API key y devuelve el par `nombre=valor` de la cookie de sesión.
    pub fn login_cookie(&self) -> String {
        let body = format!(r#"{{"api_key":"{}"}}"#, self.api_key);
        let r = self.post(
            "/api/v1/auth/login",
            &[("Sec-Fetch-Site", "same-origin")],
            &body,
        );
        assert_eq!(r.status, 200, "login: {}", r.text());
        let sc = r.header("set-cookie").expect("Set-Cookie en login");
        sc.split(';').next().unwrap().trim().to_string()
    }

    /// Comprueba que el servidor sigue vivo y atendiendo peticiones nuevas.
    pub fn assert_still_serving(&mut self, ctx: &str) {
        assert!(self.is_alive(), "{ctx}: el proceso serve ha muerto");
        let r = self
            .try_request("GET /api/v1/health HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap_or_else(|e| panic!("{ctx}: health falló: {e}"));
        assert_eq!(r.status, 200, "{ctx}: health no responde 200");
    }
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn build_request(
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: Option<&str>,
) -> String {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
            b.len()
        ));
    } else {
        req.push_str("\r\n");
    }
    req
}

/// Respuesta HTTP/1.1 parseada (cabeceras en minúsculas, repetidas conservadas).
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub raw_len: usize,
}

impl Response {
    pub fn parse(buf: &[u8]) -> Self {
        let Some(split) = find(buf, b"\r\n\r\n") else {
            return Self {
                status: 0,
                headers: Vec::new(),
                body: buf.to_vec(),
                raw_len: buf.len(),
            };
        };
        let head = String::from_utf8_lossy(&buf[..split]).into_owned();
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        Self {
            status,
            headers,
            body: buf[split + 4..].to_vec(),
            raw_len: buf.len(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn headers_all(&self, name: &str) -> Vec<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .filter(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("cuerpo no es JSON ({e}): {}", self.text()))
    }
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// xorshift64* determinista: mismas semillas ⇒ mismas entradas en todas las plataformas.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.next_u64() % den < num
    }
}
