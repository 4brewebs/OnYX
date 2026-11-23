//! Ejecutor de harnesses CLI externos como drivers de subagentes (`SDD-10` Fase 1).
//!
//! Contrato exacto del SDD: el driver se lanza dentro del worktree Git aislado ya creado
//! por `SubagentManager` (reutilizando `WorktreeHandle`), sin `tokio` ni dependencias nuevas:
//! - Prompt **solo** por `stdin` en Fase 1 (`arg`/`temp_file` llegan en Fase 2, con error accionable).
//! - Dos hilos lectores acotados (`stdout`/`stderr`) para no bloquear el bucle de espera.
//! - `try_wait()` con *deadline* (`config.timeout`) y `cancel_flag`; `child.kill()` en ambos casos
//!   (el *process tree* completo llega en Fase 2).
//! - `clean_summary`: salida combinada con ANSI quitado + secretos borrados, acotado a 4 KB.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use onyx_soul_agents::{AgentView, ApprovalPolicy};
use onyx_workspace::{
    ExternalDriverConfig, PromptTransport, scrub_secrets, spawn_subagent_driver_from_args,
};

/// Resultado de una ejecución de un harness CLI externo dentro de un worktree (`SDD-10` §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalRunOutput {
    /// Código de salida del proceso (`-1` si no fue posible obtenerlo tras kill/wait).
    pub exit_code: i32,
    /// `true` si se alcanzó `config.timeout` y se mató el proceso.
    pub timed_out: bool,
    /// `true` si se levantó `cancel_flag` y se mató el proceso.
    pub cancelled: bool,
    /// `stdout + stderr` combinados: ANSI quitado + secretos borrados, máx. 4 KB.
    pub clean_summary: String,
    /// Ruta del log completo persistente (Fase 2: `.onyx/vault/logs/`); `None` en Fase 1.
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

/// Runner de serie para harnesses CLI (`SDD-10` Fase 1, contrato mínimo funcional).
///
/// Limites explícitos de Fase 1 (llegan en Fase 2, con errores accionables):
/// - Solo `PromptTransport::Stdin` (los wrappers `.cmd` de Windows rompen `arg`).
/// - Sin *process tree* kill, sin sandbox ni sanitización de entorno, sin PATH/PATHEXT.
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
        if self.config.prompt_transport != PromptTransport::Stdin {
            return Err(format!(
                "driver `{}`: prompt transport `{:?}` is available in SDD-10 Phase 2 (Phase 1 only supports `\"stdin\"`)",
                self.config.id, self.config.prompt_transport
            ));
        }

        let mut child = Command::new(&self.config.bin)
            .args(&self.config.args)
            .current_dir(worktree_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!(
                    "failed to spawn external driver `{}` (bin `{}`): {e}",
                    self.config.id, self.config.bin
                )
            })?;

        // Prompt por stdin; tolera que el hijo cierre la entrada temprano (BrokenPipe).
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(task_prompt.as_bytes());
            let _ = stdin.flush();
        }

        // Dos hilos lectores acotados: nunca dejan al bucle bloqueado en una pipe llena.
        let stdout_handle = read_pipe_capped(child.stdout.take(), PIPE_CAP_BYTES);
        let stderr_handle = read_pipe_capped(child.stderr.take(), PIPE_CAP_BYTES);

        let deadline = Instant::now() + self.config.timeout;
        let mut timed_out = false;
        let mut cancelled = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if cancel_flag.load(Ordering::Acquire) {
                        cancelled = true;
                        let _ = child.kill();
                        break child.wait().map_err(|e| {
                            format!("failed to reap external driver `{}`: {e}", self.config.id)
                        })?;
                    }
                    if Instant::now() >= deadline {
                        timed_out = true;
                        let _ = child.kill();
                        break child.wait().map_err(|e| {
                            format!("failed to reap external driver `{}`: {e}", self.config.id)
                        })?;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "failed to wait on external driver `{}`: {e}",
                        self.config.id
                    ));
                }
            }
        };
        let exit_code = status.code().unwrap_or(-1);

        let stdout = stdout_handle
            .join()
            .map_err(|_| "stdout reader thread panicked")?;
        let stderr = stderr_handle
            .join()
            .map_err(|_| "stderr reader thread panicked")?;
        let mut combined = stdout;
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
            full_log_path: None, // vault logs: SDD-10 Fase 2
        })
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
}
