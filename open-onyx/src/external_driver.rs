//! Ejecutor de harnesses CLI externos como drivers de subagentes (`SDD-10` Fase 2: ejecución segura).
//!
//! Contrato exacto del SDD: el driver se lanza dentro del worktree Git aislado ya creado
//! por `SubagentManager` (reutilizando `WorktreeHandle`), sin `tokio` ni dependencias nuevas:
//! - El binario se resuelve **explícitamente** contra `PATH`/`PATHEXT` antes de `Command::new`
//!   ([`onyx_workspace::resolve_process_binary`]); `PromptTransport::Arg` se rechaza para
//!   wrappers de shell (`.cmd`/`.bat`) porque su argv pasa por `cmd.exe` y reinterpreta metacaracteres.
//! - Transportes de prompt: `Stdin` (hilo escritor dedicado, tolera `BrokenPipe`), `Arg`
//!   (único elemento argv) y `TempFile` (guard RAII de archivo temporal fuera del índice Git,
//!   entregado por `prompt_flag`, placeholder `{prompt_file}` o argumento final).
//! - Los dos hilos lectores acotados (`stdout`/`stderr`) se lanzan **antes** de escribir stdin,
//!   para que ni un prompt grande ni una pipe llena bloqueen el bucle de espera.
//! - `configure_process_isolation` + `ProcessJob::assign` inmediato tras `spawn`; a timeout o
//!   cancelación se mata el **árbol** completo (`terminate()`; fallback `taskkill /F /T /PID`
//!   en Windows si `assign` falla) antes de capturar estado o desmontar el worktree.
//! - Entorno saneado (`env_clear()` + allowlist): sin `NODE_OPTIONS` salvo opt-in `allowed_env`,
//!   home aislado salvo `inherit_home`, sin hints `*_OFFLINE`/`NO_PROXY` con allowlist de red.
//! - `clean_summary`: salida combinada con ANSI quitado + secretos borrados, acotado a 4 KB.
//!   El `stdout`/`stderr` crudo se expone en la salida para que el worker selle el log del
//!   vault (`.onyx/vault/subagent-logs/<job_id>.log`) pasado por `scrub_secrets` (`SDD-10` Fase 3).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use onyx_soul_agents::{AgentView, ApprovalPolicy};
use onyx_workspace::{
    ExternalDriverConfig, ProcessJob, ProcessLimits, PromptTransport,
    apply_sanitized_env_for_external_harness, clean_process_path, configure_process_isolation,
    ensure_sandbox_tmp_dir, resolve_process_binary, scrub_secrets, spawn_subagent_driver_from_args,
};

/// Resultado de una ejecución de un harness CLI externo dentro de un worktree (`SDD-10` §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalRunOutput {
    /// Código de salida del proceso (`-1` si no fue posible obtenerlo tras kill/wait).
    pub exit_code: i32,
    /// `true` si se alcanzó `config.timeout` y se mató el árbol de procesos.
    pub timed_out: bool,
    /// `true` si se levantó `cancel_flag` y se mató el árbol de procesos.
    pub cancelled: bool,
    /// `stdout + stderr` combinados: ANSI quitado + secretos borrados, máx. 4 KB.
    /// Es el **log**, nunca la verdad: la propuesta solo se construye de Git
    /// (`capture_subagent_commit` + `changed_files_between`, `SDD-10` Fase 3).
    pub clean_summary: String,
    /// `stdout` crudo (acotado a `PIPE_CAP_BYTES`), antes de `strip_ansi`/`scrub_secrets`:
    /// lo usa el worker para sellar el log del vault (`SDD-10` Fase 3).
    pub stdout: String,
    /// `stderr` crudo (acotado a `PIPE_CAP_BYTES`), antes de `strip_ansi`/`scrub_secrets`.
    pub stderr: String,
    /// Ruta del log completo persistente (`.onyx/vault/subagent-logs/<job_id>.log`,
    /// `SDD-10` Fase 3); `None` si no se pudo escribir en el vault.
    pub full_log_path: Option<PathBuf>,
}

/// Driver de harness CLI externo (`SDD-10` §3, contrato exacto del SDD).
///
/// Implementaciones: [`ExternalDriverRunner`] (Serie). El trait vive en `open-onyx`
/// (no en `onyx-workspace`) porque arrastra `std::process` y el ciclo de vida del worktree,
/// que son responsabilidad del binario, no de la librería de políticas.
pub trait ExternalHarnessDriver: Send + Sync {
    /// `id` del driver (p. ej. `"opencode"`); identifica el bloque `[[external_drivers]]`.
    fn id(&self) -> &str;

    /// Configuración declarada del driver (bin, args, timeout, transporte, allowlists).
    fn config(&self) -> &ExternalDriverConfig;

    /// Ejecuta el harness dentro de `worktree_path` con el prompt dado.
    /// Bloqueante hasta timeout/cancel/exito; nunca muta `HEAD` ni el repo principal.
    fn run_in_worktree(
        &self,
        worktree_path: &Path,
        task_prompt: &str,
        cancel_flag: &Arc<AtomicBool>,
    ) -> Result<ExternalRunOutput, String>;
}

/// Runner de serie para harnesses CLI (`SDD-10` Fase 2: ejecución segura).
///
/// Garantías antes y durante el `spawn()`:
/// - Binario resuelto explícito contra `PATH`/`PATHEXT` (nunca adivina `Command::new`).
/// - `PromptTransport::Arg` rechazado en wrappers `.cmd`/`.bat` (injección vía `cmd.exe`).
/// - `configure_process_isolation` + entorno saneado + `current_dir` limpia.
/// - `ProcessJob` asignado **inmediatamente** tras `spawn`: el árbol muere entero a
///   timeout/cancel, y el worktree se puede desmontar sin handles retenidos.
pub struct ExternalDriverRunner {
    config: ExternalDriverConfig,
}

impl ExternalDriverRunner {
    pub fn new(config: ExternalDriverConfig) -> Self {
        Self { config }
    }
}

const PIPE_CAP_BYTES: usize = 1 << 20; // 1 MiB por stream (los dos juntos acotan la RAM)
const CLEAN_SUMMARY_MAX_BYTES: usize = 4096;

impl ExternalHarnessDriver for ExternalDriverRunner {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn config(&self) -> &ExternalDriverConfig {
        &self.config
    }

    fn run_in_worktree(
        &self,
        worktree_path: &Path,
        task_prompt: &str,
        cancel_flag: &Arc<AtomicBool>,
    ) -> Result<ExternalRunOutput, String> {
        // Resolución explícita de `bin` contra `PATH`/`PATHEXT` **antes** de `Command::new`:
        // si es un wrapper de shell (`.cmd`/`.bat`), `Arg` es inyectable y se rechaza.
        let resolved = resolve_process_binary(&self.config.bin).ok_or_else(|| {
            format!(
                "driver `{}`: binary `{}` was not resolved against PATH{} (install it or set an absolute path in `bin`)",
                self.config.id,
                self.config.bin,
                if cfg!(windows) { "/PATHEXT" } else { "" }
            )
        })?;
        if resolved.is_shell_wrapper && self.config.prompt_transport == PromptTransport::Arg {
            return Err(format!(
                "driver `{}`: prompt transport `\"arg\"` is rejected for shell wrappers (`{}`): their argv goes through `cmd.exe` and would reinterpret metacharacters; use `\"stdin\"` or `\"temp_file\"`",
                self.config.id,
                resolved.path.display()
            ));
        }

        let sandbox_tmp = ensure_sandbox_tmp_dir(worktree_path)?;

        // argv según el transporte del prompt (el prompt nunca se concatena a una línea de shell).
        let mut argv: Vec<String> = self.config.args.clone();
        let mut stdin_piped = false;
        let mut prompt_file: Option<TempPromptFile> = None;
        match self.config.prompt_transport {
            PromptTransport::Stdin => stdin_piped = true,
            PromptTransport::Arg => argv.push(task_prompt.to_string()),
            PromptTransport::TempFile => {
                let guard = TempPromptFile::create(&self.config.id, task_prompt)
                    .map_err(|e| format!("driver `{}`: {e}", self.config.id))?;
                let prompt_path = guard.path.to_string_lossy().into_owned();
                if let Some(flag) = self.config.prompt_flag.clone() {
                    argv.push(flag);
                    argv.push(prompt_path);
                } else if argv.iter().any(|a| a.contains("{prompt_file}")) {
                    for a in argv.iter_mut() {
                        *a = a.replace("{prompt_file}", &prompt_path);
                    }
                } else {
                    argv.push(prompt_path);
                }
                prompt_file = Some(guard);
            }
        }

        let mut cmd = Command::new(&resolved.path);
        cmd.args(&argv)
            .current_dir(clean_process_path(worktree_path))
            .stdin(if stdin_piped {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_isolation(&mut cmd, true);
        apply_sanitized_env_for_external_harness(
            &mut cmd,
            self.config.network_policy.derived_network_policy(),
            worktree_path,
            &sandbox_tmp,
            &self.config.allowed_env,
            self.config.inherit_home,
        );

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "failed to spawn external driver `{}` (bin `{}`): {e}",
                self.config.id,
                resolved.path.display()
            )
        })?;

        // Asignación inmediata al `ProcessJob`: ningún intervalo sin cobertura entre
        // `spawn` y el bucle de espera (los nietos heredan el job/grupo de procesos).
        let mut job = ProcessJob::assign(&child, ProcessLimits::default());

        // Hilos lectores acotados **antes** de escribir stdin: un prompt grande o una pipe
        // llena nunca pueden bloquear el bucle de espera.
        let stdout_handle = read_pipe_capped(child.stdout.take(), PIPE_CAP_BYTES);
        let stderr_handle = read_pipe_capped(child.stderr.take(), PIPE_CAP_BYTES);

        // Escritor de stdin en hilo dedicado: tolera `BrokenPipe` si el hijo cierra la
        // entrada temprano o muere; al terminarse cierra el lado de lectura del hijo.
        let stdin_writer = if let Some(mut stdin) = child.stdin.take() {
            let prompt = task_prompt.to_string();
            Some(thread::spawn(move || {
                let _ = stdin.write_all(prompt.as_bytes());
                let _ = stdin.flush();
            }))
        } else {
            None
        };

        let deadline = Instant::now() + self.config.timeout;
        let mut timed_out = false;
        let mut cancelled = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if cancel_flag.load(Ordering::Acquire) {
                        cancelled = true;
                        kill_process_tree(&mut job, &mut child);
                        break child.wait().map_err(|e| {
                            format!("failed to reap external driver `{}`: {e}", self.config.id)
                        })?;
                    }
                    if Instant::now() >= deadline {
                        timed_out = true;
                        kill_process_tree(&mut job, &mut child);
                        break child.wait().map_err(|e| {
                            format!("failed to reap external driver `{}`: {e}", self.config.id)
                        })?;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    kill_process_tree(&mut job, &mut child);
                    drop(job);
                    drop(prompt_file);
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    if let Some(h) = stdin_writer {
                        let _ = h.join();
                    }
                    return Err(format!(
                        "failed to wait on external driver `{}`: {e}",
                        self.config.id
                    ));
                }
            }
        };

        // Termina explícitamente el `ProcessJob` RAII antes de capturar estado o desmontar el
        // worktree: ningún nieto huérfano retiene handles ni writes de las pipes (crítico en
        // Windows, donde un nieto vivo con la pipe abierta bloquearía a los hilos lectores).
        if let Some(mut j) = job.take() {
            j.terminate();
        }
        drop(job);

        let exit_code = status.code().unwrap_or(-1);

        let stdout = stdout_handle
            .join()
            .map_err(|_| "stdout reader thread panicked")?;
        let stderr = stderr_handle
            .join()
            .map_err(|_| "stderr reader thread panicked")?;
        if let Some(h) = stdin_writer {
            let _ = h.join();
        }
        drop(prompt_file); // RAII: elimina el archivo del prompt (si existía).

        let mut combined = stdout.clone();
        if !stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&stderr);
        }
        let clean_summary = truncate_utf8_chars(
            &scrub_secrets(&strip_ansi(&combined)),
            CLEAN_SUMMARY_MAX_BYTES,
        );

        Ok(ExternalRunOutput {
            exit_code,
            timed_out,
            cancelled,
            clean_summary,
            stdout,
            stderr,
            // El worker (`run_subagent_worker`) sella el log crudo `scrub_secrets` en
            // `.onyx/vault/subagent-logs/<job_id>.log` y rellena esta ruta (`SDD-10` Fase 3).
            full_log_path: None,
        })
    }
}

/// Mata el **árbol** completo del proceso hijo (hijo + nietos): `ProcessJob::terminate()`
/// (Job Object en Windows, grupo de procesos POSIX en unix) o, si la asignación al job no fue
/// posible, `taskkill /F /T /PID` como fallback en Windows (patrón de `WorkspaceTools`).
/// La llamada `child.kill()` final es redundante pero barata y cubre la ventana de race.
///
/// `pub(crate)`: también lo usa la probe `--version` de `openonyx doctor` (`SDD-10` Fase 4).
pub(crate) fn kill_process_tree(job: &mut Option<ProcessJob>, child: &mut Child) {
    if let Some(mut j) = job.take() {
        j.terminate();
    } else {
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &child.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = child.kill();
        }
    }
    let _ = child.kill();
}

/// Guard RAII del archivo temporal del prompt (`PromptTransport::TempFile`): se crea fuera del
/// índice Git (en `temp_dir()`) y se elimina al salir de ámbito, incluso en error.
struct TempPromptFile {
    path: PathBuf,
}

impl TempPromptFile {
    fn create(driver_id: &str, content: &str) -> std::io::Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let safe_id: String = driver_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let name = format!(
            "onyx-extdrv-{safe_id}-{}-{}.prompt",
            std::process::id(),
            seq
        );
        let path = std::env::temp_dir().join(name);
        fs::write(&path, content)?;
        Ok(Self { path })
    }
}

impl Drop for TempPromptFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Hook dinámico de aprobación por argumentos (`SDD-10` Fase 1): fuerza `WaitingApproval`
/// **solo** para `spawn_subagent` con un driver externo no-nativo; el resto de herramientas y
/// el driver `native` siguen el flujo actual (`NeverApprove` efectivo).
/// Stateless: la decisión depende únicamente de `tool` + `args` (el gate de trust/reglas
/// definitivo lo evalúa `WorkspacePolicy::external_driver_gate` en la superficie).
#[derive(Copy, Clone, Debug, Default)]
pub struct ExternalDriverApprovalPolicy;

impl ApprovalPolicy for ExternalDriverApprovalPolicy {
    fn requires_approval(&self, _view: &AgentView, tool: &str, args: &str) -> bool {
        tool == "spawn_subagent"
            && spawn_subagent_driver_from_args(args)
                .as_deref()
                .is_some_and(|d| !d.eq_ignore_ascii_case("native"))
    }
}

/// Lee una pipe hasta EOF en un hilo, almacenando a lo sumo `cap` bytes (patrón `read_pipe_capped`
/// de `onyx-workspace`; se duplica localmente porque es privado allí y el runner vive en `open-onyx`).
fn read_pipe_capped<R: Read + Send + 'static>(
    pipe: Option<R>,
    cap: usize,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let Some(mut reader) = pipe else {
            return String::new();
        };
        let mut collected = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if collected.len() < cap {
                        let take = (cap - collected.len()).min(n);
                        collected.extend_from_slice(&chunk[..take]);
                    }
                }
            }
        }
        String::from_utf8_lossy(&collected).into_owned()
    })
}

/// Quita secuencias ANSI sin dependencias: CSI (`ESC [ ... <final-letter>`), OSC
/// (`ESC ] ... BEL` o `ESC ] ... ESC \`) y escapes de carácter simple (`ESC M`, `ESC 7`, ...).
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI (`ESC [ ... final-letter` en `@..~`): colores, movimiento de cursor, etc.
            Some('[') => {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('@'..='~').contains(&c2) {
                        break;
                    }
                }
            }
            // OSC (`ESC ] ... BEL` o `ESC ] ... ESC \`): títulos de terminal, hiperenlaces.
            Some(']') => {
                chars.next();
                let mut pending_escape = false;
                for c2 in chars.by_ref() {
                    if pending_escape {
                        pending_escape = false;
                        if c2 == '\\' {
                            break; // ST: fin de la secuencia.
                        }
                        continue; // `ESC` + otro carácter: se trata como contenido.
                    }
                    if c2 == '\u{07}' {
                        break;
                    }
                    pending_escape = c2 == '\u{1b}';
                }
            }
            // Escape de carácter simple: se descartan `ESC` + el carácter siguiente.
            Some(_) => {
                chars.next();
            }
            None => {} // escape colgado al final: se descarta.
        }
    }
    out
}

fn truncate_utf8_chars(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[truncated]", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use onyx_workspace::SubagentNetworkPolicy;

    #[test]
    fn approval_policy_only_fires_for_external_spawn_subagent() {
        let policy = ExternalDriverApprovalPolicy;
        let view = || onyx_soul_agents::AgentView {
            agent: onyx_soul_agents::ConceptId(0),
            request: 1,
            name: "test".to_string(),
            goal: "test".to_string(),
            tools: Vec::new(),
            memory: Vec::new(),
            pending_approval: None,
            props: Vec::new(),
        };
        // sin driver / driver nativo → no aprueba (flujo actual intacto).
        assert!(!policy.requires_approval(&view(), "spawn_subagent", r#"{"goal":"x"}"#));
        assert!(!policy.requires_approval(
            &view(),
            "spawn_subagent",
            r#"{"goal":"x","driver":"native"}"#
        ));
        // driver externo → sí fuerza aprobación.
        assert!(policy.requires_approval(
            &view(),
            "spawn_subagent",
            r#"{"goal":"x","driver":"opencode"}"#
        ));
        // otras herramientas → nunca.
        assert!(!policy.requires_approval(&view(), "fs_write", r#"{"path":"a.rs"}"#));
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m plain"), "red plain");
        assert_eq!(strip_ansi("a\u{1b}[1;32mgreen\u{1b}[0m"), "agreen");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{07}body"), "body");
        assert_eq!(strip_ansi("\u{1b}]8;;url\u{1b}\\link\u{1b}[0m"), "link");
        assert_eq!(strip_ansi("no escapes \u{1b}M here"), "no escapes  here");
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("onyx_extdrv_{prefix}_{pid}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn base_config(id: &str, bin: &str) -> ExternalDriverConfig {
        ExternalDriverConfig {
            id: id.to_string(),
            bin: bin.to_string(),
            args: Vec::new(),
            prompt_transport: PromptTransport::Stdin,
            prompt_flag: None,
            timeout: Duration::from_secs(60),
            allowed_env: Vec::new(),
            inherit_home: false,
            network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(Vec::new()),
        }
    }

    fn no_cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// Criterio de inyección (`SDD-10` Fase 2): un prompt hostil (`%PATH%&echo pwned|`)
    /// entregado por stdin a un wrapper `.cmd` llega **literal** a la salida del wrapper;
    /// `cmd.exe` no lo reinterpreta porque nunca pasa por una línea de comando.
    #[cfg(windows)]
    #[test]
    fn stdin_prompt_to_cmd_wrapper_is_not_interpreted() {
        let dir = temp_dir("cmdinj");
        let wrapper = dir.join("wrapper.cmd");
        let out_file = dir.join("out.txt");
        // `more` sin argumentos lee el **stdin** byte a byte (a diferencia de `type`,
        // que no admite entrada estándar) y lo volca a `out.txt`.
        fs::write(
            &wrapper,
            format!("@echo off\r\nmore > \"{}\"\r\n", out_file.display()),
        )
        .unwrap();

        let mut cfg = base_config("cmdinj", wrapper.to_str().unwrap());
        cfg.timeout = Duration::from_secs(30);
        let runner = ExternalDriverRunner::new(cfg);
        let prompt = r"%PATH%&echo pwned|";
        let out = runner
            .run_in_worktree(&dir, prompt, &no_cancel())
            .expect("el wrapper .cmd debe ejecutarse con prompt por stdin");
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out && !out.cancelled);

        // El wrapper recibió el prompt byte a byte: `type` lo volcó literal a `out.txt`.
        let captured = fs::read_to_string(&out_file).expect("el wrapper debe escribir out.txt");
        assert_eq!(captured.trim_end_matches(['\r', '\n']), prompt);
        // Nada del payload interpretado aparece en la salida combinada.
        assert!(
            !out.clean_summary.contains("pwned"),
            "{}",
            out.clean_summary
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `PromptTransport::Arg` sobre un wrapper `.cmd` se rechaza **antes** del `spawn`
    /// (criterio de inyección `SDD-10` Fase 2).
    #[cfg(windows)]
    #[test]
    fn arg_transport_rejected_for_shell_wrapper_before_spawn() {
        let dir = temp_dir("argrej");
        let wrapper = dir.join("wrapper.cmd");
        fs::write(&wrapper, "@echo off\r\n").unwrap();

        let mut cfg = base_config("argrej", wrapper.to_str().unwrap());
        cfg.prompt_transport = PromptTransport::Arg;
        cfg.timeout = Duration::from_secs(30);
        let runner = ExternalDriverRunner::new(cfg);
        let err = runner
            .run_in_worktree(&dir, "prompt", &no_cancel())
            .expect_err("Arg sobre .cmd debe rechazarse antes de spawn");
        assert!(err.contains("rejected"), "{err}");
        assert!(err.contains("arg"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Fuga de entorno (`SDD-10` Fase 2): el hijo **no** ve `ONYX_TEST_SECRET` ni
    /// `NODE_OPTIONS` (salvo opt-in `allowed_env`); sí ve `PATH` y el passthrough declarado.
    #[test]
    fn sanitized_env_hides_secrets_and_node_options() {
        // Edition 2024: `set_var`/`remove_var` son `unsafe` (mutan el proceso entero).
        unsafe {
            std::env::set_var("ONYX_TEST_SECRET", "super-secret-value");
            std::env::set_var("NODE_OPTIONS", "--require=/evil/inject.js");
            std::env::set_var("ONYX_PASSTHROUGH_PROBE", "visible-to-child");
        }
        let result = run_env_probe();
        unsafe {
            std::env::remove_var("ONYX_TEST_SECRET");
            std::env::remove_var("NODE_OPTIONS");
            std::env::remove_var("ONYX_PASSTHROUGH_PROBE");
        }
        let out = result.expect("el probe debe ejecutarse");

        // Los **valores** de las variables sensibles nunca llegan al hijo.
        assert!(
            !out.clean_summary.contains("super-secret-value"),
            "{}",
            out.clean_summary
        );
        assert!(
            !out.clean_summary.contains("--require"),
            "{}",
            out.clean_summary
        );
        if cfg!(windows) {
            // Ausente → texto literal; definida → su valor. `PATH` presente → expansión
            // de substring (literal `~0,2` si estuviera ausente).
            assert!(
                out.clean_summary.contains("SECRET=[%ONYX_TEST_SECRET%]"),
                "ONYX_TEST_SECRET debe estar ausente en el hijo (literal `%VAR%`): {}",
                out.clean_summary
            );
            assert!(
                out.clean_summary.contains("NODE=[%NODE_OPTIONS%]"),
                "NODE_OPTIONS debe estar ausente en el hijo (sin opt-in allowed_env): {}",
                out.clean_summary
            );
            assert!(
                !out.clean_summary.contains("OFF=[true"),
                "no debe inyectar hints offline con allowlist: {}",
                out.clean_summary
            );
            assert!(
                out.clean_summary.contains("PTH=[visible-to-child]"),
                "allowed_env debe hacer visible el passthrough: {}",
                out.clean_summary
            );
            assert!(
                !out.clean_summary.contains("~0,2"),
                "PATH base debe estar definida en el hijo: {}",
                out.clean_summary
            );
        } else {
            assert!(
                !out.clean_summary.contains("ONYX_TEST_SECRET"),
                "ONYX_TEST_SECRET debe estar ausente en el hijo: {}",
                out.clean_summary
            );
            assert!(
                !out.clean_summary.contains("NODE_OPTIONS"),
                "NODE_OPTIONS debe estar ausente en el hijo: {}",
                out.clean_summary
            );
            assert!(
                !out.clean_summary.contains("CARGO_NET_OFFLINE"),
                "no debe inyectar hints offline con allowlist: {}",
                out.clean_summary
            );
            assert!(out.clean_summary.contains("PATH="), "{}", out.clean_summary);
            assert!(
                out.clean_summary
                    .contains("ONYX_PASSTHROUGH_PROBE=visible-to-child"),
                "allowed_env debe hacer visible el passthrough: {}",
                out.clean_summary
            );
        }
    }

    fn run_env_probe() -> Result<ExternalRunOutput, String> {
        let dir = temp_dir("envprobe");
        let mut cfg = env_probe_config(&dir);
        cfg.allowed_env = vec!["ONYX_PASSTHROUGH_PROBE".to_string()];
        // Allowlist no-vacía → `Unrestricted` derivado (ver `derived_network_policy`).
        cfg.network_policy =
            SubagentNetworkPolicy::ExternalHarnessAllowlist(vec!["probe.api.test".to_string()]);
        cfg.timeout = Duration::from_secs(30);
        let runner = ExternalDriverRunner::new(cfg);
        let out = runner.run_in_worktree(&dir, "prompt", &no_cancel());
        let _ = fs::remove_dir_all(&dir);
        out
    }

    /// Config del probe de entorno: el hijo expande `%VAR%` (sin `if`, cuyo condición
    /// falsa traga la cadena `&` en `cmd`). Variable ausente → texto literal entre
    /// corchetes; definida → su valor.
    #[cfg(windows)]
    fn env_probe_config(_dir: &Path) -> ExternalDriverConfig {
        let mut c = base_config("envprobe", "cmd");
        c.args = vec!["/Q".into(), "/C".into()];
        c.args.push(
            "echo SECRET=[%ONYX_TEST_SECRET%] & echo NODE=[%NODE_OPTIONS%] & echo OFF=[%CARGO_NET_OFFLINE%] & echo PTH=[%ONYX_PASSTHROUGH_PROBE%] & echo P2=[%PATH:~0,2%]"
                .into(),
        );
        c
    }

    #[cfg(not(windows))]
    fn env_probe_config(dir: &Path) -> ExternalDriverConfig {
        let script = dir.join("probe.sh");
        fs::write(
            &script,
            "env | grep -E '^(ONYX_TEST_SECRET|NODE_OPTIONS|ONYX_PASSTHROUGH_PROBE|CARGO_NET_OFFLINE|PATH)=' || true\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut c = base_config("envprobe", "sh");
        c.args = vec![script.display().to_string()];
        c
    }

    /// Timeout + pipes grandes + nieto que retiene la pipe de `stdout`: sin deadlock,
    /// el árbol muere entero y los lectores terminan (criterio de aceptación `SDD-10` Fase 2).
    #[test]
    fn timeout_kills_process_tree_with_large_output_and_grandchild() {
        let dir = temp_dir("timeout");
        let mut cfg = timeout_config(&dir);
        // Timeout corto: el hijo (y su nieto) viven mucho más que 3 s.
        cfg.timeout = Duration::from_secs(3);

        let runner = ExternalDriverRunner::new(cfg);
        let started = Instant::now();
        let out = runner
            .run_in_worktree(&dir, "prompt", &no_cancel())
            .expect("timeout no debe producir error, solo terminar el árbol");
        let elapsed = started.elapsed();
        let _ = fs::remove_dir_all(&dir);

        assert!(out.timed_out, "debe reportar timeout");
        assert!(!out.cancelled);
        // Muerte del árbol + reap del hijo debe ser rápida (sin esperar a `ping -n 300`).
        assert!(
            elapsed < Duration::from_secs(15),
            "el kill del árbol tardó demasiado: {elapsed:?} (¿deadlock en las pipes?)"
        );
        // El hijo escribió ~145 KB antes de morir: la salida capturada se trunca a 4 KB.
        assert!(
            out.clean_summary.contains("truncated"),
            "debe capturar y truncar la salida grande: {}",
            out.clean_summary
        );
    }

    /// Script de stress: volca ~145 KB por `stdout` y luego deja vivir a un **nieto** que
    /// hereda la pipe (Windows: `ping`; unix: `sleep &`): matar solo al hijo dejaría la pipe
    /// abierta → deadlock en el lector. El kill del árbol es lo que hace el test verde.
    #[cfg(windows)]
    fn timeout_config(dir: &Path) -> ExternalDriverConfig {
        let script = dir.join("bigout.cmd");
        fs::write(
            &script,
            "@echo off\r\nfor /L %%i in (1,1,1800) do @echo 01234567890123456789012345678901234567890123456789012345678901234567890123456789\r\nping -n 300 127.0.0.1\r\n",
        )
        .unwrap();
        base_config("timeout", script.to_str().unwrap())
    }

    #[cfg(not(windows))]
    fn timeout_config(dir: &Path) -> ExternalDriverConfig {
        let script = dir.join("bigout.sh");
        fs::write(
            &script,
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 1800 ]; do\n  printf '01234567890123456789012345678901234567890123456789012345678901234567890123456789\\n'\n  i=$((i+1))\ndone\nsleep 300 &\nwait\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        base_config("timeout", script.to_str().unwrap())
    }

    /// `PromptTransport::TempFile`: el archivo del prompt se entrega por placeholder
    /// `{prompt_file}` y se elimina al terminar (guard RAII).
    #[test]
    fn temp_file_transport_placeholder_and_raii_cleanup() {
        let dir = temp_dir("tmpfile");
        let mut cfg = if cfg!(windows) {
            let mut c = base_config("tmpfile", "cmd");
            c.prompt_transport = PromptTransport::TempFile;
            c.args = vec!["/Q".into(), "/C".into(), "type {prompt_file}".into()];
            c
        } else {
            let mut c = base_config("tmpfile", "sh");
            c.prompt_transport = PromptTransport::TempFile;
            c.args = vec!["-c".into(), "cat {prompt_file}".into()];
            c
        };
        cfg.timeout = Duration::from_secs(30);

        // Buscamos el archivo temporal antes y después para verificar la RAII.
        let before: Vec<PathBuf> = list_prompt_files("tmpfile");
        let prompt = "hola-temp-file-77";
        let runner = ExternalDriverRunner::new(cfg);
        let out = runner
            .run_in_worktree(&dir, prompt, &no_cancel())
            .expect("temp_file debe ejecutarse");
        assert_eq!(out.exit_code, 0);
        assert!(
            out.clean_summary.contains("hola-temp-file-77"),
            "{}",
            out.clean_summary
        );
        let after: Vec<PathBuf> = list_prompt_files("tmpfile");
        let created: Vec<_> = after.into_iter().filter(|p| !before.contains(p)).collect();
        assert!(
            created.is_empty(),
            "el archivo del prompt debe eliminarse al terminar: {created:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn list_prompt_files(driver_id: &str) -> Vec<PathBuf> {
        let prefix = format!("onyx-extdrv-{driver_id}-");
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(std::env::temp_dir()) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    out.push(e.path());
                }
            }
        }
        out
    }
}
