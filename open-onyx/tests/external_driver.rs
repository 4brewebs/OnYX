//! `ExternalHarnessDriver` / `ExternalDriverRunner` (SDD-10, open-onyx/src/external_driver.rs)
//! probado como lo usaría una app: `openonyx serve` + proveedor OpenAI falso que pide
//! `spawn_subagent {driver:"fake"}` + un harness externo FALSO (script `sh` generado en el
//! test) declarado en `[[external_drivers]]` de `.onyx/config.toml`. Sin OpenCode ni red.
//!
//! Los gates puros (`WorkspacePolicy::external_driver_gate`) se prueban por la API pública de
//! `onyx_workspace`. Los rojos llevan `#[ignore = "BUG: FIX-NN …"]` (numeración de Lince desde FIX-82).

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{FakeOpenAi, Step, TempDir};

const KEY: &str = "onyx-test-dummy-key";

/// Secretos ficticios que el test pone en el entorno de `serve`.
const SECRETS: &[(&str, &str)] = &[
    ("ONYX_WEB_API_KEY", KEY),
    ("OPENAI_API_KEY", "sk-forja-dummy-openai"),
    ("ANTHROPIC_API_KEY", "sk-ant-forja-dummy"),
    ("GEMINI_API_KEY", "forja-dummy-gemini"),
    ("GITHUB_TOKEN", "ghp_forjadummytoken"),
    ("AWS_SECRET_ACCESS_KEY", "forja-dummy-aws"),
];

/// Escenario: `root/ws` (repo git con un commit), `root/ctrl` (contador, PID files, volcados),
/// `root/outside` (objetivos de escape), `root/home`, `root/bin/fake.sh` y `root/tmp`
/// (`TMPDIR` de `serve`: el tmp del sandbox del harness, `onyx-sandbox-<hash>`, se crea ahí
/// y se borra con el escenario en vez de acumularse en `/tmp`, FIX-96).
struct Scenario {
    tmp: TempDir,
}

impl Scenario {
    fn new(prefix: &str) -> Self {
        let tmp = TempDir::new(prefix);
        for d in ["ws", "ctrl", "outside", "home", "bin", "tmp"] {
            fs::create_dir_all(tmp.0.join(d)).unwrap();
        }
        let ws = tmp.0.join("ws");
        fs::write(ws.join("README.md"), "fixture\n").unwrap();
        fs::write(ws.join("claimed.txt"), "claimed original\n").unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args([
                    "-c",
                    "user.name=Forja",
                    "-c",
                    "user.email=forja@example.invalid",
                ])
                .args(args)
                .current_dir(&ws)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        Self { tmp }
    }
    fn ws(&self) -> PathBuf {
        self.tmp.0.join("ws")
    }
    fn ctrl(&self) -> PathBuf {
        self.tmp.0.join("ctrl")
    }
    fn outside(&self) -> PathBuf {
        self.tmp.0.join("outside")
    }
    fn runs(&self) -> usize {
        fs::read_to_string(self.ctrl().join("count"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Escribe el harness falso: cuenta la ejecución, consume el prompt y ejecuta `body`.
    /// `body` es shell con las variables `$CTRL`, `$OUT` y `$WS` ya fijadas (rutas absolutas).
    fn harness(&self, body: &str) -> PathBuf {
        let path = self.tmp.0.join("bin").join("fake.sh");
        let script = format!(
            "#!/bin/sh\nCTRL='{ctrl}'\nOUT='{out}'\nWS='{ws}'\necho \"run $$\" >> \"$CTRL/count\"\npwd > \"$CTRL/cwd\"\ncat > /dev/null\n{body}\n",
            ctrl = self.ctrl().display(),
            out = self.outside().display(),
            ws = self.ws().display(),
        );
        fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// `.onyx/config.toml` con el driver `fake` y, opcionalmente, reglas de permisos.
    fn config(&self, timeout_secs: u64, allowed_env: &[&str], permissions: &str) {
        let bin = self.tmp.0.join("bin").join("fake.sh");
        let env_list: Vec<String> = allowed_env.iter().map(|e| format!("\"{e}\"")).collect();
        let body = format!(
            "[[external_drivers]]\nid = \"fake\"\nbin = \"{}\"\nargs = []\nprompt_transport = \"stdin\"\ntimeout_secs = {timeout_secs}\nallowed_env = [{}]\n\n{permissions}\n",
            bin.display(),
            env_list.join(", ")
        );
        fs::create_dir_all(self.ws().join(".onyx")).unwrap();
        fs::write(self.ws().join(".onyx/config.toml"), body).unwrap();
    }
}

pub const ALLOW_FAKE: &str = "[permissions]\nallow = [\"spawn_subagent(fake)\"]\n";

struct Srv {
    child: Child,
    port: u16,
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn launch(sc: &Scenario, api_base: &str, extra: &[&str], trusted: bool) -> Srv {
    use std::io::{BufRead, BufReader};
    let home = sc.tmp.0.join("home");
    // umask 022 fija en el hijo, independiente de la umask del proceso de test (FIX-98):
    // `ensure_sandbox_tmp_dir` hace `create_dir_all` sin `chmod`, así que con umask 077 el
    // directorio nacería 0700 aunque el producto no lo ponga privado, y el rojo pasaría en
    // falso. Con 022 queda 0755, que es el modo del bug, tanto si el padre tiene 022, 077 o 000.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(r#"umask 022; exec "$0" "$@""#)
        .arg(env!("CARGO_BIN_EXE_openonyx"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("ONYX_TRUST_FILE", home.join("trusted.toml"))
        .env("FORJA_ALLOWED", "forja-allowed-value")
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("TMPDIR", sc.tmp.0.join("tmp"))
        .env("PWD", sc.ws());
    for (k, v) in SECRETS {
        cmd.env(k, v);
    }
    cmd.args([
        "serve",
        "--workspace",
        sc.ws().to_str().unwrap(),
        "--bind",
        "127.0.0.1:0",
        "--provider",
        "openai",
        "--model",
        "fake-model",
        "--api-base",
        api_base,
        "--sandbox",
        "off",
    ])
    .args(extra);
    if trusted {
        cmd.arg("--trust-workspace");
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn openonyx serve");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut port = None;
    while Instant::now() < deadline && port.is_none() {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            if let Some(rest) = line.trim().strip_prefix("URL:") {
                port = rest
                    .trim()
                    .trim_end_matches('/')
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.parse::<u16>().ok());
            }
        }
    }
    let Some(port) = port else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("serve did not print its URL");
    };
    Srv { child, port }
}

fn http(port: u16, method: &str, path: &str, body: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {KEY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or(out)
}

fn command(srv: &Srv, cmd: &str) -> String {
    http(
        srv.port,
        "POST",
        "/api/v1/command",
        &format!("{{\"command\":{}}}", common::json_string(cmd)),
    )
}

fn state(srv: &Srv) -> serde_json::Value {
    serde_json::from_str(&http(srv.port, "GET", "/api/v1/state", "")).unwrap_or_default()
}

fn tool_call_event(name: &str, args_json: &str) -> String {
    let args = args_json.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"call_1","type":"function","function":{{"name":"{name}","arguments":"{args}"}}}}]}},"finish_reason":null}}]}}"#
    )
}

const FINISH_TOOLS: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

/// Guion del modelo: pide `spawn_subagent` con el driver `fake` y luego contesta texto.
fn spawn_script() -> Vec<Step> {
    let text = || Step::Sse {
        events: vec![common::delta("ok"), common::FINISH_STOP.to_string()],
        done: true,
    };
    vec![
        Step::Sse {
            events: vec![
                tool_call_event(
                    "spawn_subagent",
                    r#"{"goal":"edit claimed.txt","name":"ext","driver":"fake"}"#,
                ),
                FINISH_TOOLS.to_string(),
            ],
            done: true,
        },
        text(),
        text(),
        text(),
    ]
}

/// Espera a que el primer subagente termine (`done`/`failed`) y devuelve su snapshot.
fn wait_job(srv: &Srv, secs: u64) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last = serde_json::Value::Null;
    while Instant::now() < deadline {
        last = state(srv);
        if let Some(job) = last["subagents"].as_array().and_then(|a| a.first()) {
            let st = job["status"].as_str().unwrap_or_default();
            if st == "done" || st == "failed" {
                return job.clone();
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("subagent job did not finish; last state: {last}");
}

fn touched(job: &serde_json::Value) -> Vec<String> {
    job["touched_files"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn wait_until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn pid_alive(pid_file: &Path) -> Option<bool> {
    let pid = fs::read_to_string(pid_file).ok()?.trim().to_string();
    Some(Path::new(&format!("/proc/{pid}")).exists())
}

/// Cuerpo de harness «con compuerta»: espera a que el test cree `$CTRL/go`, ejecuta `body` y
/// escribe `$CTRL/finished` justo antes de salir.
///
/// Los controles que exigen la notificación «[subagent `ext-1` finished …]» NO pueden leer
/// `/api/v1/state` mientras el job termina: `GET /state` llama a `SubagentHandle::list_summaries`
/// → `SubagentManager::status` → `poll_completions` (open-onyx/src/subagent.rs:767), que drena
/// el canal de finalizaciones y DESCARTA las propuestas; si gana esa carrera al `poll` del driver
/// (subagent.rs:1794) el root no recibe nunca la notificación (FIX-99, ver
/// `state_publication_must_not_swallow_the_completion_notification`). La compuerta permite sondear
/// el estado mientras el job está bloqueado y dejar de hacerlo cuando se libera.
fn gated(body: &str) -> String {
    format!(
        "while [ ! -e \"$CTRL/go\" ]; do sleep 0.05; done\n{body}\necho done > \"$CTRL/finished\""
    )
}

/// Espera a que el harness esté corriendo (bloqueado en la compuerta): sondear `/state` ahora es
/// inocuo porque el job aún no ha terminado.
fn wait_harness_started(sc: &Scenario) {
    assert!(
        wait_until(30, || sc.runs() == 1),
        "harness never started (runs={})",
        sc.runs()
    );
}

/// Libera la compuerta, espera a que el harness termine (fichero `finished`, no `/state`) y deja
/// pasar 3 s SIN tocar `/state` para que el hilo principal drene la finalización y la ingiera.
fn release_job_quietly(sc: &Scenario) {
    fs::write(sc.ctrl().join("go"), "").unwrap();
    assert!(
        wait_until(30, || sc.ctrl().join("finished").exists()),
        "harness did not finish"
    );
    std::thread::sleep(Duration::from_secs(3));
}

/// Lanza `serve`, pide el `spawn_subagent` y espera a que el harness (con compuerta) arranque.
fn start_gated_job(sc: &Scenario) -> (Srv, FakeOpenAi) {
    let fake = FakeOpenAi::start(spawn_script());
    let srv = launch(sc, &fake.base_url(), &[], true);
    assert_eq!(command(&srv, "delegate"), r#"{"ok":true}"#);
    wait_harness_started(sc);
    (srv, fake)
}

/// Ejecuta un turno que lanza el harness y espera al job.
fn run_job(sc: &Scenario, extra: &[&str], secs: u64) -> (Srv, FakeOpenAi, serde_json::Value) {
    let fake = FakeOpenAi::start(spawn_script());
    let srv = launch(sc, &fake.base_url(), extra, true);
    assert_eq!(command(&srv, "delegate"), r#"{"ok":true}"#);
    let job = wait_job(&srv, secs);
    (srv, fake, job)
}

fn root_memory(srv: &Srv) -> Vec<(String, String, u64)> {
    let st = state(srv);
    st["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|ag| ag["is_root"] == true))
        .and_then(|ag| ag["memory"].as_array())
        .map(|m| {
            m.iter()
                .map(|e| {
                    (
                        e["kind"].as_str().unwrap_or_default().to_string(),
                        e["text"].as_str().unwrap_or_default().to_string(),
                        e["tick"].as_u64().unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn notifications(srv: &Srv) -> Vec<(String, u64)> {
    root_memory(srv)
        .into_iter()
        .filter(|(_, text, _)| text.starts_with("[subagent `ext-1` finished"))
        .map(|(_, text, tick)| (text, tick))
        .collect()
}

// ───────────── 1) Resultado: disco, no stdout; un único cambio en un tick ─────────────

/// (b) El harness dice que editó `claimed.txt` pero toca `touched.txt`: la propuesta y la
/// notificación al grafo salen del disco (Git), una sola vez, y el root no se modifica sin
/// `subagent_merge`.
#[cfg(unix)]
#[test]
fn proposal_comes_from_disk_not_from_stdout_and_lands_once() {
    let sc = Scenario::new("ext_lie");
    sc.harness(&gated(
        "echo 'DONE: I edited claimed.txt'; echo real > touched.txt",
    ));
    sc.config(30, &[], ALLOW_FAKE);
    let (srv, _fake) = start_gated_job(&sc);
    release_job_quietly(&sc);
    let job = wait_job(&srv, 30);
    assert_eq!(job["status"], "done", "{job}");
    assert_eq!(touched(&job), vec!["touched.txt".to_string()]);
    assert!(
        wait_until(10, || !notifications(&srv).is_empty()),
        "no notification; state: {}",
        state(&srv)
    );
    std::thread::sleep(Duration::from_millis(500));
    let notes = notifications(&srv);
    assert_eq!(notes.len(), 1, "exactly one change in the graph: {notes:?}");
    assert!(notes[0].0.contains("touched: touched.txt"), "{notes:?}");
    assert!(!notes[0].0.contains("touched: claimed.txt"), "{notes:?}");
    assert_eq!(
        fs::read_to_string(sc.ws().join("claimed.txt")).unwrap(),
        "claimed original\n",
        "root must not change before subagent_merge"
    );
    assert!(!sc.ws().join("touched.txt").exists());
    assert_eq!(sc.runs(), 1);
}

/// (a) Dice que terminó pero no toca nada: propuesta vacía.
#[cfg(unix)]
#[test]
fn harness_that_claims_success_without_changes_yields_empty_proposal() {
    let sc = Scenario::new("ext_nothing");
    sc.harness("echo 'All files edited successfully: claimed.txt, main.rs'");
    sc.config(30, &[], ALLOW_FAKE);
    let (_srv, _fake, job) = run_job(&sc, &[], 30);
    assert!(touched(&job).is_empty(), "{job}");
    assert_eq!(sc.runs(), 1);
}

// ───────────── 2) Muerte a medias: timeout, parciales, árbol de procesos ─────────────

const HANG: &str = "echo partial > partial.txt\n(sleep 300 & echo $! > \"$CTRL/pid_bg\")\nsh -c 'sleep 300' & echo $! > \"$CTRL/pid_child\"\necho $$ > \"$CTRL/pid_main\"\nsleep 300";

fn kill_pid_file(f: &Path) {
    if let Ok(pid) = fs::read_to_string(f) {
        let _ = Command::new("kill").args(["-9", pid.trim()]).status();
    }
}

/// Control: al vencer el timeout, el job falla y muere el árbol del grupo de procesos (hijo,
/// nieto en segundo plano).
#[cfg(target_os = "linux")]
#[test]
fn timeout_kills_harness_and_same_group_descendants() {
    let sc = Scenario::new("ext_timeout");
    sc.harness(HANG);
    sc.config(2, &[], ALLOW_FAKE);
    let (_srv, _fake, job) = run_job(&sc, &[], 30);
    assert_eq!(job["status"], "failed", "{job}");
    assert!(
        job["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("timeout"),
        "{job}"
    );
    for f in ["pid_main", "pid_bg", "pid_child"] {
        let p = sc.ctrl().join(f);
        assert!(
            wait_until(5, || pid_alive(&p) == Some(false)),
            "{f} still alive after timeout"
        );
    }
}

/// Tras un timeout no se propone nada parcial y el worktree se descarta.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "BUG: FIX-82 tras timeout del harness externo, capture_subagent_commit recoge lo escrito a medias (touched_files=[partial.txt]) y el worktree queda montado: solo se omite la captura si cancelled (open-onyx/src/subagent.rs:1485-1503)"]
fn timeout_proposes_nothing_partial_and_discards_worktree() {
    let sc = Scenario::new("ext_partial");
    sc.harness(HANG);
    sc.config(2, &[], ALLOW_FAKE);
    let (_srv, _fake, job) = run_job(&sc, &[], 30);
    let wt = PathBuf::from(fs::read_to_string(sc.ctrl().join("cwd")).unwrap().trim());
    assert!(
        touched(&job).is_empty(),
        "partial work proposed after timeout: {job}"
    );
    assert!(
        !wt.exists(),
        "worktree must be discarded after timeout: {}",
        wt.display()
    );
}

/// (e) Un nieto con `setsid` (otra sesión y otro grupo) sobrevive al kill del árbol.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "BUG: FIX-83 un nieto lanzado con setsid sobrevive al timeout: ProcessJob en Unix mata solo el grupo de procesos (-pgid), y setsid crea otro (onyx-workspace/src/sandbox.rs:1824-1850; open-onyx/src/external_driver.rs:194, :306-324)"]
fn timeout_kills_setsid_grandchild() {
    let sc = Scenario::new("ext_setsid");
    sc.harness(
        "setsid sh -c 'echo $$ > \"$0/pid_setsid\"; exec sleep 300' \"$CTRL\" </dev/null >/dev/null 2>&1 &\necho $$ > \"$CTRL/pid_main\"\nsleep 300",
    );
    sc.config(2, &[], ALLOW_FAKE);
    let (_srv, _fake, job) = run_job(&sc, &[], 30);
    assert_eq!(job["status"], "failed", "{job}");
    let p = sc.ctrl().join("pid_setsid");
    let dead = wait_until(5, || pid_alive(&p) == Some(false));
    kill_pid_file(&p);
    assert!(dead, "setsid grandchild survived the harness timeout");
}

/// (e) Variante: el nieto `setsid` conserva el stdout del harness. El runner espera EOF de
/// la pipe tras matar el grupo y el job no termina nunca.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "BUG: FIX-84 un nieto setsid que hereda stdout bloquea el join del lector tras el timeout y el job queda en running para siempre (open-onyx/src/external_driver.rs:263-268, :380-397)"]
fn setsid_grandchild_holding_stdout_does_not_hang_the_job() {
    let sc = Scenario::new("ext_setsid_pipe");
    sc.harness("setsid sh -c 'echo $$ > \"$0/pid_setsid\"; exec sleep 300' \"$CTRL\" &\nsleep 300");
    sc.config(2, &[], ALLOW_FAKE);
    let fake = FakeOpenAi::start(spawn_script());
    let srv = launch(&sc, &fake.base_url(), &[], true);
    assert_eq!(command(&srv, "delegate"), r#"{"ok":true}"#);
    let finished = wait_until(15, || {
        state(&srv)["subagents"]
            .as_array()
            .and_then(|a| a.first())
            .is_some_and(|j| j["status"] == "failed" || j["status"] == "done")
    });
    kill_pid_file(&sc.ctrl().join("pid_setsid"));
    assert!(
        finished,
        "job still running 15 s after a 2 s timeout (it only finishes once the test kills the setsid grandchild)"
    );
}

// ───────────── 3) Rewind / restore: sin relanzar y sin respuesta tardía ─────────────

fn text_step(t: &str) -> Step {
    Step::Sse {
        events: vec![common::delta(t), common::FINISH_STOP.to_string()],
        done: true,
    }
}

/// Turno 1 (texto, milestone) y turno 2 (`spawn_subagent`). Devuelve el tick del turno 1.
/// `snaps` es un `TempDir`; el pack va en `snaps/pack` para que la rama que crea `/rewind`
/// (`pack.fork-t<N>`, hermana del pack) quede DENTRO del `TempDir` y se borre con él (antes
/// quedaba `/tmp/onyx_sse_ext_snaps_*.fork-t2`). `--fork-dir` no sirve: solo afecta a
/// `--resume`, no al `/rewind` en vivo.
fn two_turns(sc: &Scenario, snaps: &Path) -> (Srv, FakeOpenAi, u64) {
    let pack = snaps.join("pack");
    let mut script = vec![text_step("uno")];
    script.extend(spawn_script());
    let fake = FakeOpenAi::start(script);
    let srv = launch(
        sc,
        &fake.base_url(),
        &["--snapshot-dir", pack.to_str().unwrap()],
        true,
    );
    assert_eq!(command(&srv, "primero"), r#"{"ok":true}"#);
    assert!(
        wait_until(20, || root_memory(&srv)
            .iter()
            .any(|(k, t, _)| k == "say" && t == "uno")
            && state(&srv)["agents"][0]["state"] == "done"),
        "turn 1 did not finish"
    );
    let tick1 = state(&srv)["tick"].as_u64().unwrap();
    assert_eq!(command(&srv, "delegate"), r#"{"ok":true}"#);
    (srv, fake, tick1)
}

fn rewind_applied(srv: &Srv) -> bool {
    !root_memory(srv)
        .iter()
        .any(|(k, t, _)| k == "call" && t.starts_with("spawn_subagent"))
}

fn settled(srv: &Srv) -> bool {
    let st = state(srv);
    st["agents"].as_array().is_some_and(|a| {
        a.iter()
            .all(|ag| ag["state"] == "done" || ag["state"] == "idle")
    })
}

/// Control: tras terminar el job, `/rewind` al turno 1 NO relanza el harness.
#[cfg(unix)]
#[test]
fn rewind_after_job_does_not_relaunch_harness() {
    let sc = Scenario::new("ext_rewind");
    sc.harness(&gated("echo real > touched.txt"));
    sc.config(30, &[], ALLOW_FAKE);
    let snaps = TempDir::new("ext_snaps");
    let (srv, _fake, tick1) = two_turns(&sc, &snaps.0);
    wait_harness_started(&sc);
    release_job_quietly(&sc);
    wait_job(&srv, 30);
    assert!(
        wait_until(20, || !notifications(&srv).is_empty() && settled(&srv)),
        "no notification or root not settled; state: {}",
        state(&srv)
    );
    assert_eq!(command(&srv, &format!("/rewind {tick1}")), r#"{"ok":true}"#);
    assert!(
        wait_until(10, || rewind_applied(&srv)),
        "rewind not applied"
    );
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(sc.runs(), 1, "harness relaunched after rewind");
}

/// Notificaciones «[subagent `<id>` finished …]» recibidas por el root para ese `id`.
fn notifications_of(srv: &Srv, id: &str) -> usize {
    let prefix = format!("[subagent `{id}` finished");
    root_memory(srv)
        .iter()
        .filter(|(_, text, _)| text.starts_with(&prefix))
        .count()
}

/// Un intento de FIX-99: tres jobs externos (el máximo concurrente) que terminan, escalonados
/// unos milisegundos, mientras el bucle principal está ocupado con ticks. Devuelve
/// `Some((id, estado))` del primer job que sale `done` en `/state` sin que el root reciba su
/// notificación.
///
/// La ventana de la carrera es de ~0,3 ms de CPU por tick (el worker envía la finalización
/// DESPUÉS del `poll` de un tick y ANTES de su `present`/su comando `/subagents`), así que el test:
/// 1. manda comandos `/subagents` sin pausa desde 8 clientes (casi siempre hay un tick en curso);
/// 2. satura la CPU con hilos propios (tantos como CPUs) para que el reparto de turnos sea
///    irregular;
/// 3. libera los tres jobs con 15 ms de diferencia, para muestrear tres fases distintas del tick.
#[cfg(unix)]
fn swallowed_completion() -> Option<(String, String)> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    const JOBS: [&str; 3] = ["ext-1", "ext-2", "ext-3"];
    let spawn = || Step::Sse {
        events: vec![
            tool_call_event(
                "spawn_subagent",
                r#"{"goal":"edit claimed.txt","name":"ext","driver":"fake"}"#,
            ),
            FINISH_TOOLS.to_string(),
        ],
        done: true,
    };
    let mut script = vec![spawn(), spawn(), spawn()];
    script.extend((0..6).map(|_| text_step("ok")));
    let sc = Scenario::new("ext_swallow");
    sc.harness(
        "J=$(basename \"$PWD\")\nwhile [ ! -e \"$CTRL/go-$J\" ]; do sleep 0.02; done\necho real > touched.txt\necho done > \"$CTRL/finished-$J\"",
    );
    sc.config(30, &[], ALLOW_FAKE);
    let fake = FakeOpenAi::start(script);
    let srv = launch(&sc, &fake.base_url(), &[], true);
    assert_eq!(command(&srv, "delegate"), r#"{"ok":true}"#);
    assert!(
        wait_until(30, || sc.runs() == 3),
        "the three harnesses never started (runs={})",
        sc.runs()
    );
    let stop = Arc::new(AtomicBool::new(false));
    let clients = (0..8).map(|_| {
        let (port, stop) = (srv.port, stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = http(
                    port,
                    "POST",
                    "/api/v1/command",
                    r#"{"command":"/subagents"}"#,
                );
            }
        })
    });
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    let burners = (0..cpus).map(|_| {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut x = 0u64;
            while !stop.load(Ordering::Relaxed) {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(x);
            }
        })
    });
    let threads: Vec<_> = clients.chain(burners).collect();
    for id in JOBS {
        fs::write(sc.ctrl().join(format!("go-{id}")), "").unwrap();
        std::thread::sleep(Duration::from_millis(15));
    }
    let finished = wait_until(30, || {
        JOBS.iter()
            .all(|id| sc.ctrl().join(format!("finished-{id}")).exists())
    });
    std::thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::Relaxed);
    for th in threads {
        th.join().unwrap();
    }
    assert!(finished, "harnesses did not finish");
    assert!(
        wait_until(30, || state(&srv)["subagents"].as_array().is_some_and(
            |a| a.len() == 3 && a.iter().all(|j| j["status"] == "done")
        )),
        "jobs not done: {}",
        state(&srv)
    );
    wait_until(10, || JOBS.iter().all(|id| notifications_of(&srv, id) > 0));
    JOBS.iter()
        .find(|id| notifications_of(&srv, id) == 0)
        .map(|id| (id.to_string(), state(&srv).to_string()))
}

/// FIX-99: un job externo puede terminar como `done` en `/state` sin que el root reciba nunca
/// la notificación «[subagent `ext-N` finished …]» (ni se ingiera la propuesta en el grafo).
///
/// Causa (open-onyx @ 3f49d57): `/state` lo publica el propio bucle principal al final de cada
/// tick (`WebSurface::present`, web.rs:992-998 → `publish_snapshot` → `SubagentHandle::
/// list_summaries`, web.rs:1252-1255), y `/subagents` hace lo mismo (command.rs:1195-1199).
/// `list_summaries` llama a `SubagentManager::status`, que ejecuta
/// `let _ = self.poll_completions()` (subagent.rs:766-784, ficha de Lince): drena el canal de
/// finalizaciones, marca el job como `done` y DESCARTA la propuesta. `SubagentManagerDriver::poll`
/// (subagent.rs:1794)
/// es el único que la convierte en `IngressEvent::Command(encode_proposal_command(..))`. Si el
/// worker envía la finalización (subagent.rs:1519-1524) después del `poll` de un tick y antes
/// de su `present`, este la consume y el `poll` del tick siguiente no encuentra nada. Con el
/// bucle ocioso no hay ventana (el tick empieza por `poll`); con ticks en curso (comandos web,
/// turnos del root, máquina cargada) sí. Lo mismo ocurre con `subagent_status`, `spawn_job`
/// (:611), `prepare_merge_preview` (:933) y el resto de llamadas que descartan
/// `poll_completions()` (:790, :1088, :1117).
///
/// Es una carrera: hasta 40 intentos de 3 jobs cada uno, con su propio `serve`; basta un solo
/// job perdido. Con el producto corregido el test tarda ~40 s y pasa.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-99 SubagentManager::status (llamado desde publish_snapshot, web.rs:1252, y desde /subagents, command.rs:1199) drena y descarta las finalizaciones (`let _ = self.poll_completions()`, subagent.rs:766-784): si ocurre entre el envío del worker y el poll del driver, el root nunca recibe «[subagent … finished]» y la propuesta no se ingiere (subagent.rs:725-756, :1794)"]
fn state_publication_must_not_swallow_the_completion_notification() {
    for attempt in 1..=40 {
        if let Some((id, st)) = swallowed_completion() {
            panic!(
                "attempt {attempt}: job {id} is done in /state but the root was never notified; state: {st}"
            );
        }
    }
}

/// Respuesta tardía: el root asienta mientras el harness sigue corriendo; `/rewind` al turno 1
/// (antes del `spawn_subagent`); cuando el harness termina, su resultado no debe entrar en la
/// línea temporal rebobinada.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-85 la finalización tardía de un job externo lanzado antes de /rewind se inyecta en el grafo rebobinado (notificación «[subagent … finished]» y propuesta) aunque ese spawn ya no existe: SubagentManager no sabe del rewind (open-onyx/src/subagent.rs:1788-1815; session.rs:406-466)"]
fn late_completion_after_rewind_is_not_applied() {
    let sc = Scenario::new("ext_late");
    sc.harness("sleep 4; echo late > touched.txt; echo done > \"$CTRL/finished\"");
    sc.config(30, &[], ALLOW_FAKE);
    let snaps = TempDir::new("ext_snaps_late");
    let (srv, _fake, tick1) = two_turns(&sc, &snaps.0);
    assert!(
        wait_until(10, || !rewind_applied(&srv) && settled(&srv)),
        "root must settle while the harness runs"
    );
    assert!(
        !sc.ctrl().join("finished").exists(),
        "harness finished too early"
    );
    assert_eq!(command(&srv, &format!("/rewind {tick1}")), r#"{"ok":true}"#);
    assert!(
        wait_until(10, || rewind_applied(&srv)),
        "rewind not applied"
    );
    assert!(wait_until(20, || sc.ctrl().join("finished").exists()));
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(sc.runs(), 1);
    let notes = notifications(&srv);
    assert!(
        notes.is_empty(),
        "late completion applied to the rewound timeline: {notes:?}"
    );
}

// ───────────── 4) Escape del worktree ─────────────

fn escape_case(
    prefix: &str,
    body: &str,
    setup: impl FnOnce(&Scenario),
) -> (Scenario, serde_json::Value) {
    let sc = Scenario::new(prefix);
    setup(&sc);
    sc.harness(body);
    sc.config(30, &[], ALLOW_FAKE);
    let (_srv, _fake, job) = run_job(&sc, &[], 30);
    (sc, job)
}

/// Control: lo escrito DENTRO del worktree queda en la propuesta y no en el root.
#[cfg(unix)]
#[test]
fn writes_inside_worktree_stay_in_proposal() {
    let (sc, job) = escape_case("ext_inside", "mkdir -p sub; echo in > sub/in.txt", |_| {});
    assert_eq!(touched(&job), vec!["sub/in.txt".to_string()]);
    assert!(!sc.ws().join("sub").exists());
}

#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-86 el harness externo escribe por ruta absoluta fuera del worktree (incluido el workspace root, saltándose subagent_merge): sin contenedor no hay confinamiento de sistema de ficheros, solo cwd (open-onyx/src/external_driver.rs:164-182)"]
fn harness_cannot_write_outside_worktree_by_absolute_path() {
    let (sc, _job) = escape_case(
        "ext_abs",
        "echo pwned > \"$OUT/abs.txt\"; echo pwned > \"$WS/claimed.txt\"",
        |_| {},
    );
    let mut breaches = Vec::new();
    if sc.outside().join("abs.txt").exists() {
        breaches.push("wrote <outside>/abs.txt by absolute path");
    }
    if fs::read_to_string(sc.ws().join("claimed.txt")).unwrap() != "claimed original\n" {
        breaches.push("overwrote <workspace root>/claimed.txt, bypassing subagent_merge");
    }
    assert!(breaches.is_empty(), "{breaches:?}");
}

#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-87 el harness externo escribe fuera del worktree con rutas relativas `..` (sin confinamiento de FS fuera de strict-container) (open-onyx/src/external_driver.rs:164-182)"]
fn harness_cannot_write_outside_worktree_by_dotdot() {
    // worktree = <root>/.onyx-wt/ws/ext-1 → ../../../outside = <root>/outside
    let (sc, _job) = escape_case(
        "ext_dotdot",
        "echo pwned > ../../../outside/dotdot.txt",
        |_| {},
    );
    assert!(
        !sc.outside().join("dotdot.txt").exists(),
        "wrote outside via .."
    );
}

#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-88 el harness externo modifica un fichero de fuera del worktree a través de un symlink creado en el worktree (open-onyx/src/external_driver.rs:164-182)"]
fn harness_cannot_write_outside_worktree_by_symlink() {
    let (sc, job) = escape_case(
        "ext_symlink",
        "ln -s \"$OUT/sym_target.txt\" link; echo pwned > link",
        |sc| fs::write(sc.outside().join("sym_target.txt"), "orig\n").unwrap(),
    );
    assert_eq!(
        fs::read_to_string(sc.outside().join("sym_target.txt")).unwrap(),
        "orig\n",
        "outside file changed through symlink; job: {job}"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-89 el harness externo modifica un fichero de fuera del worktree a través de un hardlink creado en el worktree (open-onyx/src/external_driver.rs:164-182)"]
fn harness_cannot_write_outside_worktree_by_hardlink() {
    let (sc, job) = escape_case(
        "ext_hardlink",
        "ln \"$OUT/hard.txt\" hl && echo pwned > hl",
        |sc| fs::write(sc.outside().join("hard.txt"), "orig\n").unwrap(),
    );
    assert_eq!(
        fs::read_to_string(sc.outside().join("hard.txt")).unwrap(),
        "orig\n",
        "outside file changed through hardlink; job: {job}"
    );
}

// ───────────── 5) Entorno ─────────────

/// (f) El harness vuelca su entorno: ningún secreto ficticio del proceso `serve` le llega;
/// lo que está en `allowed_env` sí.
#[cfg(target_os = "linux")]
#[test]
fn harness_env_has_no_secrets_and_keeps_allowed_env() {
    let sc = Scenario::new("ext_env");
    // Entorno INICIAL del proceso (no el que `sh` reconstruye): /proc/$$/environ.
    sc.harness("tr '\\000' '\\n' < /proc/$$/environ > \"$CTRL/env.txt\"");
    sc.config(30, &["FORJA_ALLOWED"], ALLOW_FAKE);
    let (_srv, _fake, _job) = run_job(&sc, &[], 30);
    let env = fs::read_to_string(sc.ctrl().join("env.txt")).unwrap();
    for (k, v) in SECRETS {
        assert!(!env.contains(v), "secret {k} reached the harness:\n{env}");
    }
    assert!(env.contains("FORJA_ALLOWED=forja-allowed-value"), "{env}");
}

/// Colorado P0-2/P0-3: el harness no recibe `PWD` del padre (OpenCode usaría `PWD` antes que
/// `cwd` y trabajaría en el root) y su `HOME` no es el del usuario.
#[cfg(target_os = "linux")]
#[test]
fn harness_gets_isolated_home_and_no_inherited_pwd() {
    let sc = Scenario::new("ext_home");
    sc.harness("tr '\\000' '\\n' < /proc/$$/environ > \"$CTRL/env.txt\"");
    sc.config(30, &[], ALLOW_FAKE);
    let (_srv, _fake, _job) = run_job(&sc, &[], 30);
    let env = fs::read_to_string(sc.ctrl().join("env.txt")).unwrap();
    let home = sc.tmp.0.join("home");
    assert!(
        !env.lines().any(|l| l == format!("HOME={}", home.display())),
        "harness inherited the user HOME:\n{env}"
    );
    assert!(
        !env.lines().any(|l| l.starts_with("PWD=")),
        "harness inherited PWD:\n{env}"
    );
    let cwd = fs::read_to_string(sc.ctrl().join("cwd")).unwrap();
    assert!(cwd.contains(".onyx-wt"), "cwd must be the worktree: {cwd}");
}

/// Colorado P0-2: `XDG_CACHE_HOME` del usuario pasa al harness (está en la allowlist de SO),
/// así que comparte la caché real del usuario pese al `HOME` aislado.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "BUG: FIX-90 XDG_CACHE_HOME del usuario llega al harness externo (SAFE_OS_KEYS) aunque HOME esté aislado: comparte caché/estado del usuario (onyx-workspace/src/sandbox.rs:921-922)"]
fn harness_does_not_inherit_user_xdg_cache_home() {
    let sc = Scenario::new("ext_xdg");
    sc.harness("tr '\\000' '\\n' < /proc/$$/environ > \"$CTRL/env.txt\"");
    sc.config(30, &[], ALLOW_FAKE);
    let (_srv, _fake, _job) = run_job(&sc, &[], 30);
    let env = fs::read_to_string(sc.ctrl().join("env.txt")).unwrap();
    let xdg = sc.tmp.0.join("home").join("xdg-cache");
    assert!(
        !env.contains(&xdg.display().to_string()),
        "user XDG_CACHE_HOME reached the harness:\n{env}"
    );
}

/// Colorado P0-14: ficheros del plano de control no ignorados del root (`.env.test`,
/// `.onyx/vault/x`) se materializan en el worktree y el harness los lee.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-91 el worktree del harness nace del árbol vivo con git add -A y materializa ficheros del plano de control no ignorados (.env*, .onyx/vault/**): el harness lee secretos del root (onyx-workspace/src/worktree.rs:299-303 → state.rs:272-290, sin excluir el plano de control)"]
fn control_plane_files_are_not_materialized_in_worktree() {
    let sc = Scenario::new("ext_cp_files");
    fs::write(sc.ws().join(".env.test"), "SECRET=forja-dummy-dotenv\n").unwrap();
    fs::create_dir_all(sc.ws().join(".onyx/vault")).unwrap();
    fs::write(sc.ws().join(".onyx/vault/x"), "vault-secret\n").unwrap();
    sc.harness(
        "for f in .env.test .onyx/vault/x; do test -e \"$f\" && echo \"$f\" >> \"$CTRL/seen\"; done; true",
    );
    sc.config(30, &[], ALLOW_FAKE);
    let (_srv, _fake, _job) = run_job(&sc, &[], 30);
    let seen = fs::read_to_string(sc.ctrl().join("seen")).unwrap_or_default();
    assert!(
        seen.is_empty(),
        "control-plane files visible to the harness: {seen:?}"
    );
}

/// Lecturas fuera del worktree (Colorado §4.3): el harness lee la clave SSH, las credenciales
/// del usuario (en su `HOME` real, `root/home`) y un fichero cualquiera de `outside/`. Los tres
/// ficheros son ficticios. Vuelca lo leído en `ctrl/` y lee también un fichero DEL worktree
/// (`README.md`) como sonda positiva.
const READ_OUTSIDE: &str = r#"for f in "$OUT/../home/.ssh/id_forja" "$OUT/../home/.aws/credentials" "$OUT/secret.txt"; do
  cat "$f" >> "$CTRL/read_outside.txt" 2>/dev/null
done
cat README.md > "$CTRL/read_inside.txt""#;

fn read_outside_case(prefix: &str) -> Scenario {
    let sc = Scenario::new(prefix);
    let home = sc.tmp.0.join("home");
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::create_dir_all(home.join(".aws")).unwrap();
    fs::write(home.join(".ssh/id_forja"), "FORJA-DUMMY-PRIVATE-KEY\n").unwrap();
    fs::write(
        home.join(".aws/credentials"),
        "FORJA-DUMMY-AWS-CREDENTIALS\n",
    )
    .unwrap();
    fs::write(
        sc.outside().join("secret.txt"),
        "FORJA-DUMMY-OUTSIDE-SECRET\n",
    )
    .unwrap();
    sc.harness(READ_OUTSIDE);
    sc.config(30, &[], ALLOW_FAKE);
    let (_srv, _fake, _job) = run_job(&sc, &[], 30);
    sc
}

/// Control (§4.3): la sonda funciona: el harness lee `README.md` de su worktree.
#[cfg(unix)]
#[test]
fn harness_can_read_files_inside_its_worktree() {
    let sc = read_outside_case("ext_read_in");
    assert_eq!(
        fs::read_to_string(sc.ctrl().join("read_inside.txt")).unwrap(),
        "fixture\n"
    );
}

/// §4.3: el `HOME` falso es solo una variable de entorno; con rutas absolutas el harness lee
/// `~/.ssh/*`, `~/.aws/*` y cualquier fichero del host con los permisos del usuario que corre
/// `serve`. Misma causa raíz que FIX-86..89 (sin jail de FS), pero en lectura: exfiltración,
/// no escritura. El fichero `ctrl/read_outside.txt` no debe existir o debe estar vacío.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-97 el harness externo LEE ficheros fuera del worktree por ruta absoluta (~/.ssh, ~/.aws, cualquier fichero del usuario): HOME falso es solo entorno y no hay jail de FS (open-onyx/src/external_driver.rs:164-182; onyx-workspace/src/sandbox.rs:962)"]
fn harness_cannot_read_user_files_outside_worktree() {
    let sc = read_outside_case("ext_read_out");
    let read = fs::read_to_string(sc.ctrl().join("read_outside.txt")).unwrap_or_default();
    assert!(
        read.is_empty(),
        "harness read files outside its worktree:\n{read}"
    );
}

/// Tmp del sandbox del harness (Colorado §4.6): `HOME` del harness = `$TMPDIR/onyx-sandbox-<hash>`.
const SANDBOX_HOME: &str = r#"echo "$HOME" > "$CTRL/sandbox_home"
echo "state of the harness" > "$HOME/marker.txt""#;

/// Ejecuta un job, lo descarta (`/discard`) y devuelve el `HOME` del sandbox que usó el harness.
fn sandbox_home_after_discard(prefix: &str) -> (Scenario, PathBuf) {
    let sc = Scenario::new(prefix);
    sc.harness(SANDBOX_HOME);
    sc.config(30, &[], ALLOW_FAKE);
    let (srv, _fake, job) = run_job(&sc, &[], 30);
    let id = job["job_id"].as_str().expect("job_id").to_string();
    let home = PathBuf::from(
        fs::read_to_string(sc.ctrl().join("sandbox_home"))
            .unwrap()
            .trim(),
    );
    assert_eq!(command(&srv, &format!("/discard {id}")), r#"{"ok":true}"#);
    let discarded = wait_until(15, || {
        state(&srv)["subagents"]
            .as_array()
            .and_then(|a| a.first())
            .is_some_and(|j| j["status"] == "discarded")
    });
    assert!(discarded, "job was not discarded: {}", state(&srv));
    (sc, home)
}

/// Control (§4.6): el `HOME` del harness es un directorio del sandbox (no el del usuario), vive
/// bajo el `TMPDIR` de `serve`, es escribible y el harness guardó ahí su estado.
#[cfg(unix)]
#[test]
fn harness_home_is_a_sandbox_dir_under_serve_tmpdir() {
    let sc = Scenario::new("ext_sbx_home");
    sc.harness(SANDBOX_HOME);
    sc.config(30, &[], ALLOW_FAKE);
    let (_srv, _fake, _job) = run_job(&sc, &[], 30);
    let home = PathBuf::from(
        fs::read_to_string(sc.ctrl().join("sandbox_home"))
            .unwrap()
            .trim(),
    );
    assert!(
        home.starts_with(sc.tmp.0.join("tmp")) && home.to_string_lossy().contains("onyx-sandbox-"),
        "sandbox home: {}",
        home.display()
    );
    assert_ne!(home, sc.tmp.0.join("home"));
    assert!(home.join("marker.txt").is_file());
}

/// §4.6: el directorio del sandbox (el `HOME` del harness, con su estado y posibles credenciales)
/// no se borra al descartar el job: `ensure_sandbox_tmp_dir` lo crea y nadie lo limpia.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-96 el tmp del sandbox del harness (onyx-sandbox-<hash>, su HOME) no se borra al descartar el job: ensure_sandbox_tmp_dir lo crea y remove_worktree no lo limpia (onyx-workspace/src/sandbox.rs:1152-1165; worktree.rs:522)"]
fn sandbox_tmp_is_removed_when_job_is_discarded() {
    let (_sc, home) = sandbox_home_after_discard("ext_sbx_rm");
    assert!(
        !home.exists(),
        "sandbox tmp {} survives /discard (marker: {:?})",
        home.display(),
        fs::read_to_string(home.join("marker.txt")).ok()
    );
}

/// §4.6: el nombre es previsible (`onyx-sandbox-<fnv(worktree)>`) y se crea con los permisos por
/// defecto (umask del proceso, normalmente 0755): otro usuario del host puede listar y leer el
/// estado del harness y precrear el directorio o un symlink con ese nombre.
///
/// El test exige 0700 exacto. `launch` fija `umask 022` en el `serve` (independiente de la umask
/// del padre), para que el directorio nazca 0755 mientras el producto no haga `chmod 0700`. Sin
/// eso, con umask 077 el rojo pasaría en falso.
#[cfg(unix)]
#[test]
#[ignore = "BUG: FIX-98 el tmp del sandbox del harness se crea con los permisos por defecto (0755 con umask 022) y nombre previsible: otros usuarios del host leen el HOME del harness (onyx-workspace/src/sandbox.rs:1152-1165)"]
fn sandbox_tmp_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let sc = Scenario::new("ext_sbx_mode");
    sc.harness(SANDBOX_HOME);
    sc.config(30, &[], ALLOW_FAKE);
    let (_srv, _fake, _job) = run_job(&sc, &[], 30);
    let home = PathBuf::from(
        fs::read_to_string(sc.ctrl().join("sandbox_home"))
            .unwrap()
            .trim(),
    );
    let mode = fs::metadata(&home).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode,
        0o700,
        "sandbox tmp {} has mode {mode:o}, expected exactly 700",
        home.display()
    );
}

// ───────────── 6) Gates ─────────────

/// Workspace no confiado: los `[[external_drivers]]` del proyecto se ignoran y el harness no
/// se lanza.
#[cfg(unix)]
#[test]
fn untrusted_workspace_never_launches_project_harness() {
    let sc = Scenario::new("ext_untrusted");
    sc.harness("echo real > touched.txt");
    sc.config(30, &[], ALLOW_FAKE);
    let fake = FakeOpenAi::start(spawn_script());
    let srv = launch(&sc, &fake.base_url(), &[], false);
    let st = state(&srv);
    assert_eq!(st["untrusted"], true, "{st}");
    assert!(
        !st["drivers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == "fake"),
        "{st}"
    );
    assert_eq!(command(&srv, "delegate"), r#"{"ok":true}"#);
    assert!(wait_until(10, || fake.requests().len() >= 2));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(sc.runs(), 0);
}

/// Sin regla de permisos: la llamada queda en Ask (aprobación pendiente) y no se lanza.
#[cfg(unix)]
#[test]
fn external_driver_without_rule_asks_and_does_not_run() {
    let sc = Scenario::new("ext_ask");
    sc.harness("echo real > touched.txt");
    sc.config(30, &[], "");
    let fake = FakeOpenAi::start(spawn_script());
    let srv = launch(&sc, &fake.base_url(), &[], true);
    assert_eq!(command(&srv, "delegate"), r#"{"ok":true}"#);
    assert!(
        wait_until(10, || !state(&srv)["pending_approval"].is_null()),
        "expected a pending approval: {}",
        state(&srv)
    );
    let pending = state(&srv)["pending_approval"].to_string();
    assert!(pending.contains("spawn_subagent"), "{pending}");
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(sc.runs(), 0);
}

/// `deny spawn_subagent(fake)` gana a cualquier `allow`, también vía serve.
#[cfg(unix)]
#[test]
fn deny_rule_beats_allow_and_harness_does_not_run() {
    let sc = Scenario::new("ext_deny");
    sc.harness("echo real > touched.txt");
    sc.config(
        30,
        &[],
        "[permissions]\ndeny = [\"spawn_subagent(fake)\"]\nallow = [\"spawn_subagent(fake)\", \"spawn_subagent(*)\", \"*\"]\n",
    );
    let fake = FakeOpenAi::start(spawn_script());
    let srv = launch(&sc, &fake.base_url(), &[], true);
    assert_eq!(command(&srv, "delegate"), r#"{"ok":true}"#);
    assert!(wait_until(10, || fake.requests().len() >= 2));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(sc.runs(), 0);
    let mem = root_memory(&srv);
    assert!(
        mem.iter().any(|(_, t, _)| t.contains("deny rule")),
        "{mem:?}"
    );
}

mod gate {
    use onyx_workspace::{
        PermissionDecision, PermissionRules, WorkspacePolicy, WorkspaceTrustReport,
        WorkspaceTrustStatus,
    };

    fn policy(deny: &[&str], allow: &[&str], ask: &[&str]) -> WorkspacePolicy {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let dir = std::env::temp_dir();
        WorkspacePolicy::new(dir)
            .unwrap()
            .with_permissions(PermissionRules {
                deny: v(deny),
                allow: v(allow),
                ask: v(ask),
            })
    }

    #[test]
    fn no_rule_is_ask() {
        assert_eq!(
            policy(&[], &[], &[]).external_driver_gate("fake").0,
            PermissionDecision::Ask
        );
    }

    #[test]
    fn specific_allow_is_allow_and_generic_ask_loses_to_it() {
        assert_eq!(
            policy(&[], &["spawn_subagent(fake)"], &["spawn_subagent(*)"])
                .external_driver_gate("fake")
                .0,
            PermissionDecision::Allow
        );
    }

    #[test]
    fn deny_beats_every_allow() {
        let p = policy(
            &["spawn_subagent(fake)"],
            &["spawn_subagent(fake)", "spawn_subagent(*)", "*"],
            &[],
        );
        let (d, why) = p.external_driver_gate("fake");
        assert_eq!(d, PermissionDecision::Deny);
        assert!(why.contains("deny rule"), "{why}");
        assert_ne!(p.external_driver_gate("other").0, PermissionDecision::Deny);
    }

    #[test]
    fn untrusted_workspace_is_deny_even_with_allow() {
        let report = WorkspaceTrustReport {
            status: WorkspaceTrustStatus::Untrusted {
                has_config: true,
                has_sprout: false,
            },
            config_trusted: false,
            ..WorkspaceTrustReport::no_project_config()
        };
        let p = policy(&[], &["*", "spawn_subagent(fake)"], &[]).with_trust_report(report);
        let (d, why) = p.external_driver_gate("fake");
        assert_eq!(d, PermissionDecision::Deny);
        assert!(why.contains("trusted workspace"), "{why}");
    }
}
