//! `openonyx bench` (`SDD-05` REQ-05.6, `TEST-05.5`): benchmarking reproducible multi-modelo.
//!
//! Ejecuta un mismo `snapshot_dir + tick` (rebobinado a disco vía
//! [`crate::ops::reconcile_rewind_effects_report`]) frente a varios candidatos
//! (`provider:model` en vivo o `replay:<label>=<path.tr>` offline `ONYXTR2`), cada uno en un
//! *worktree* Git idéntico y desechable, y genera un informe comparativo
//! (`passed_verify`, `ticks`, `tool_calls`, tokens, coste, `wall_time_ms`).
//!
//! Invariantes: 0 dependencias nuevas; los candidatos corren **secuencialmente** (wall_time_ms
//! limpio, sin interferencia CPU/IO entre modelos); cada *worktree* se desmonta con guard RAII
//! (éxito, error o pánico → 0 `onyx/sub/bench-*` huérfanos y 0 mutaciones en el workspace raíz);
//! el store de snapshots se usa **solo lectura** (sin milestones, sin snapshot de salida).

use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSpec, EventBus, GraphView, HeadlessDriver, PlatformDriver, Reply,
    SurfaceBinding, Transcript, Verb, Vocab,
};
use onyx_workspace::{
    PendingCheckpoints, PermissionDecision, ProcessJob, SubagentNetworkPolicy, WorkspaceMode,
    WorkspacePolicy, WorkspaceToolBridge, WorkspaceTools, WorktreeHandle, WorktreeManager,
    clean_process_path, configure_process_isolation, ensure_sandbox_tmp_dir,
    prepare_sandboxed_shell_command, scrub_secrets, validate_job_id,
};
use serde::{Deserialize, Serialize};

use crate::context::SessionTelemetryHandle;
use crate::llm_profile::LlmProvider;
use crate::pack::PackSnapshotStore;
use crate::session::{SessionOptions, build_host_with_options};

/// Tope de ticks por candidato (igual que `Mode::Run`).
const MAX_BENCH_TICKS: usize = 256;
/// Tope de bytes de diagnóstico de `--verify` (igual que `[verify]`).
const VERIFY_DIAGNOSTIC_CAP_BYTES: usize = 4 * 1024;
/// Timeout por defecto de `--verify` (segundos) si no hay `--shell-timeout`.
const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 30;

// ─────────────────────────────────────────────────────────────────────────────
// Candidatos (`--models`)
// ─────────────────────────────────────────────────────────────────────────────

/// Un candidato del benchmark: un modelo LLM en vivo o un replay offline `ONYXTR2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BenchCandidateSpec {
    /// Modelo vivo `<provider>:<model>` (resuelve credenciales/base como cualquier sesión).
    Live {
        provider: String,
        model: String,
        label: String,
    },
    /// Replay offline `replay:<label>=<path.tr>` (o `replay:<path.tr>` → label = file stem).
    /// Sin red ni llamadas HTTP: las decisiones del agente salen de la transcripción y
    /// `WorkspaceToolBridge` ejecuta de verdad las herramientas en el *worktree*.
    Replay {
        label: String,
        path: std::path::PathBuf,
    },
}

impl BenchCandidateSpec {
    /// Nombre corto para la tabla/reportes (`gemini:gemini-2.5-pro` / `<label>`).
    pub fn display(&self) -> String {
        match self {
            Self::Live {
                provider, model, ..
            } => format!("{provider}:{model}"),
            Self::Replay { label, .. } => label.clone(),
        }
    }
}

/// Parsea una entrada de `--models`.
///
/// Orden de separación crítico (portable Windows+Linux): primero `split_once(':')` para
/// separar el prefijo `replay:`, y **luego** `split_once('=')` sobre el resto — así una ruta
/// tipo `C:\tmp\a.tr` o `S:\tmp\b.tr` conserva su `:` de letra de unidad intacta.
pub fn parse_candidate_spec(raw: &str) -> Result<BenchCandidateSpec, String> {
    let entry = raw.trim();
    if entry.is_empty() {
        return Err("empty candidate in --models".to_string());
    }
    if let Some(rest) = entry.strip_prefix("replay:") {
        // `replay:<label>=<path.tr>` o `replay:<path.tr>`.
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(
                "replay candidate needs a transcript path: `replay:<label>=<path.tr>`".to_string(),
            );
        }
        if let Some((label, path)) = rest.split_once('=') {
            let label = label.trim();
            let path = path.trim();
            if label.is_empty() || path.is_empty() {
                return Err(format!(
                    "malformed replay candidate (need `<label>=<path.tr>`): `{entry}`"
                ));
            }
            Ok(BenchCandidateSpec::Replay {
                label: label.to_string(),
                path: std::path::PathBuf::from(path),
            })
        } else {
            // Sin `=`: el label es el file stem del `.tr`.
            let path = std::path::PathBuf::from(rest);
            let label = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| "replay".to_string());
            Ok(BenchCandidateSpec::Replay { label, path })
        }
    } else {
        // Modelo vivo `<provider>:<model>`.
        let Some((provider, model)) = entry.split_once(':') else {
            return Err(format!(
                "unknown candidate `{entry}`: expected `<provider>:<model>` or `replay:<label>=<path.tr>`"
            ));
        };
        let provider = provider.trim();
        let model = model.trim();
        if provider.is_empty() || model.is_empty() {
            return Err(format!(
                "malformed live candidate `{entry}`: need `<provider>:<model>`"
            ));
        }
        Ok(BenchCandidateSpec::Live {
            provider: provider.to_string(),
            model: model.to_string(),
            label: entry.to_string(),
        })
    }
}

/// Parsea la lista completa de `--models` (al menos 1 candidato).
pub fn parse_candidates(raw: &str) -> Result<Vec<BenchCandidateSpec>, String> {
    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("--models requires at least one candidate".to_string());
    }
    parts
        .iter()
        .map(|p| parse_candidate_spec(p))
        .collect::<Result<Vec<_>, _>>()
}

// ─────────────────────────────────────────────────────────────────────────────
// Reporte
// ─────────────────────────────────────────────────────────────────────────────

/// Resultado de un candidato (una fila de la tabla / un objeto del JSON).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BenchResult {
    pub model: String,
    pub passed_verify: bool,
    pub ticks: u64,
    pub tool_calls: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_usd: f64,
    pub wall_time_ms: u64,
    pub verify_exit_code: Option<i32>,
    pub agent_state: String,
    pub error: Option<String>,
}

/// Metadatos del benchmark (cabecera del JSON).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BenchMetadata {
    pub snapshot_dir: String,
    pub tick: u64,
    pub verify_cmd: Option<String>,
    pub max_steps: Option<i64>,
}

/// Reporte completo (`--out bench_report.json`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BenchReport {
    pub metadata: BenchMetadata,
    pub results: Vec<BenchResult>,
}

/// Tabla Markdown limpia con las columnas de SDD-05 §3.4.
pub fn render_markdown(results: &[BenchResult]) -> String {
    let mut out = String::new();
    out.push_str(
        "| model | passed_verify | ticks | tool_calls | input_tokens | cached_tokens | output_tokens | estimated_cost_usd | wall_time_ms |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|\n");
    for r in results {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.model,
            r.passed_verify,
            r.ticks,
            r.tool_calls,
            r.input_tokens,
            r.cached_tokens,
            r.output_tokens,
            format_cost(r.estimated_cost_usd),
            r.wall_time_ms,
        ));
    }
    out
}

fn format_cost(v: f64) -> String {
    if v == 0.0 {
        "0".to_string()
    } else {
        format!("{v:.4}")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Guard RAII del *worktree* del candidato
// ─────────────────────────────────────────────────────────────────────────────

/// Desmonta siempre el *worktree* del candidato al salir (éxito, error o pánico):
/// `remove_worktree` retira el *worktree* **y** la rama temporal `onyx/sub/<job_id>`.
pub struct BenchWorktreeGuard {
    mgr: WorktreeManager,
    handle: WorktreeHandle,
}

impl BenchWorktreeGuard {
    pub fn new(mgr: WorktreeManager, handle: WorktreeHandle) -> Self {
        Self { mgr, handle }
    }

    pub fn handle(&self) -> &WorktreeHandle {
        &self.handle
    }
}

impl Drop for BenchWorktreeGuard {
    fn drop(&mut self) {
        let _ = self.mgr.remove_worktree(&self.handle);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Driver de replay offline (`BenchTranscriptDriver`)
// ─────────────────────────────────────────────────────────────────────────────

/// Driver determinista para candidatos `replay:`: extrae **solo** los verbos de decisión del
/// agente del [`Transcript`] (`Say`/`Call`/`Done`/`Fail`) y, ante cada
/// [`onyx_soul_agents::pending_requests`], despacha el siguiente decisión **reenlazando**
/// `reply.agent`/`reply.request` a la petición actual, mientras el
/// [`WorkspaceToolBridge`] interno ejecuta de verdad las herramientas (`fs_read`, `fs_write`,
/// `fs_edit`, …) dentro del *worktree* aislado.
///
/// Dos transcripciones `ONYXTR2` distintas producen así cambios reales distintos en sus
/// respectivos *worktrees*, evaluables contra `--verify`. Tokens/coste = 0 (no hay LLM).
pub struct BenchTranscriptDriver {
    vocab: Vocab,
    inner: WorkspaceToolBridge,
    decisions: Vec<(Verb, String)>,
    next: usize,
    dispatched: BTreeSet<(u64, u64)>,
    queued: Vec<Reply>,
    /// Llamadas a herramienta emitidas por el agente durante la ejecución (métrica `tool_calls`).
    pub tool_calls: AtomicUsize,
}

impl BenchTranscriptDriver {
    pub fn new(vocab: Vocab, bridge: WorkspaceToolBridge, transcript: Transcript) -> Self {
        let decisions: Vec<(Verb, String)> = transcript
            .entries
            .iter()
            .map(|e| (e.reply.verb, e.reply.payload.clone()))
            .filter(|(v, _)| matches!(v, Verb::Say | Verb::Call | Verb::Done | Verb::Fail))
            .collect();
        Self {
            vocab,
            inner: bridge,
            decisions,
            next: 0,
            dispatched: BTreeSet::new(),
            queued: Vec::new(),
            tool_calls: AtomicUsize::new(0),
        }
    }
}

impl PlatformDriver for BenchTranscriptDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        // Primero el puente (limpia `ready_flag` y vuelca sus respuestas de tool/aprobación).
        self.inner.poll(bus);
        for reply in self.queued.drain(..) {
            bus.push(reply.to_event());
        }
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        // El puente ejecuta las herramientas pendientes (estado `Acting`) y responde
        // aprobaciones (`WaitingApproval`) dentro del *worktree*.
        self.inner.present(graph, surfaces);
        // Despacha el siguiente verbo de decisión a cada agente en `Thinking` (una vez por
        // `(agent, request)`), reenlazando el reply a la petición actual.
        for req in onyx_soul_agents::pending_requests(graph, &self.vocab) {
            if !self.dispatched.insert((req.agent.0, req.request)) {
                continue;
            }
            if self.next >= self.decisions.len() {
                continue;
            }
            let (verb, payload) = self.decisions[self.next].clone();
            self.next += 1;
            let reply = match verb {
                Verb::Say => Reply::say(req.agent, req.request, payload),
                Verb::Call => {
                    self.tool_calls.fetch_add(1, Ordering::SeqCst);
                    Reply::call(req.agent, req.request, payload)
                }
                Verb::Done => Reply::done(req.agent, req.request, payload),
                Verb::Fail => Reply::fail(req.agent, req.request, payload),
                _ => Reply::done(req.agent, req.request, "bench replay: unexpected verb"),
            };
            self.queued.push(reply);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers de proceso (privados en onyx-workspace → réplica local mínima)
// ─────────────────────────────────────────────────────────────────────────────

/// Lee una pipe hasta `cap` bytes en un hilo (evita deadlocks con salidas grandes).
fn read_pipe_capped<R: Read + Send + 'static>(
    reader: Option<R>,
    cap: usize,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::with_capacity(cap.min(8 * 1024));
        if let Some(r) = reader {
            let _ = r.take(cap as u64).read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).to_string()
    })
}

/// Trunca a `max_bytes` en un límite UTF-8 sin paniquear.
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Resultado de ejecutar `--verify` en el *worktree*.
enum VerifyOutcome {
    /// `exit 0`.
    Passed,
    /// `exit != 0` (con código y diagnóstico acotado+scrubbeado).
    Failed { code: i32, diag: String },
    /// Timeout (diagnóstico parcial).
    Timeout { secs: u64, diag: String },
    /// Fallo de `spawn`/sandbox (mensaje accionable).
    Spawn(String),
    /// Denegado por `[permissions].deny` (no se ejecutó).
    Blocked,
}

/// Ejecuta `command` dentro de `workspace` con la tubería aislada de `shell_exec`
/// (`prepare_sandboxed_shell_command` + `configure_process_isolation` + `ProcessJob` + deadline),
/// respetando `[permissions].deny` y aplicando `scrub_secrets` al diagnóstico si falla.
fn exec_bench_verify(policy: &WorkspacePolicy, command: &str, timeout: Duration) -> VerifyOutcome {
    // Gobernanza: una regla `deny` sobre `shell_exec` bloquea el comando sin ejecutarlo.
    let args_json = format!(
        r#"{{"command":{}}}"#,
        serde_json::to_string(command).unwrap_or_else(|_| "\"\"".to_string())
    );
    if policy.evaluate_permission("shell_exec", &args_json) == PermissionDecision::Deny {
        return VerifyOutcome::Blocked;
    }

    let sandbox_tmp = match ensure_sandbox_tmp_dir(&policy.root) {
        Ok(d) => d,
        Err(e) => return VerifyOutcome::Spawn(format!("sandbox tmp: {e}")),
    };
    let mut cmd =
        match prepare_sandboxed_shell_command(&policy.sandbox, &policy.root, &sandbox_tmp, command)
        {
            Ok(c) => c,
            Err(e) => return VerifyOutcome::Spawn(e),
        };
    cmd.current_dir(clean_process_path(&policy.root))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_isolation(&mut cmd, false);

    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return VerifyOutcome::Spawn(format!("failed to spawn verify command: {e}")),
    };
    let mut job = ProcessJob::assign(&child, policy.sandbox.limits());
    let out_handle = read_pipe_capped(child.stdout.take(), VERIFY_DIAGNOSTIC_CAP_BYTES);
    let err_handle = read_pipe_capped(child.stderr.take(), VERIFY_DIAGNOSTIC_CAP_BYTES);
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    if let Some(ref mut j) = job {
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
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    drop(job);
                    let partial_out = scrub_secrets(&out_handle.join().unwrap_or_default());
                    let partial_err = scrub_secrets(&err_handle.join().unwrap_or_default());
                    let diag = truncate_utf8(
                        &format!("stdout:\n{partial_out}\nstderr:\n{partial_err}"),
                        VERIFY_DIAGNOSTIC_CAP_BYTES,
                    );
                    return VerifyOutcome::Timeout {
                        secs: timeout.as_secs().max(1),
                        diag,
                    };
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                if let Some(ref mut j) = job {
                    j.terminate();
                }
                drop(job);
                return VerifyOutcome::Spawn(format!("wait failed: {e}"));
            }
        }
    };

    if let Some(ref mut j) = job {
        j.terminate();
    }
    drop(job);
    let stdout = scrub_secrets(&out_handle.join().unwrap_or_default());
    let stderr = scrub_secrets(&err_handle.join().unwrap_or_default());
    let code = status.code().unwrap_or(-1);
    if code == 0 {
        VerifyOutcome::Passed
    } else {
        let diag = truncate_utf8(
            &format!("stdout:\n{stdout}\nstderr:\n{stderr}"),
            VERIFY_DIAGNOSTIC_CAP_BYTES,
        );
        VerifyOutcome::Failed { code, diag }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// `run_bench`
// ─────────────────────────────────────────────────────────────────────────────

/// Argumentos de `openonyx bench` (reutiliza los flags CLI ya parseados en `CliArgs`).
pub struct BenchArgs<'a> {
    /// `CliArgs` completos (para resolver credenciales LLM por candidato en vivo).
    pub cli: &'a crate::CliArgs,
    pub workspace: std::path::PathBuf,
    /// Política raíz ya con los overrides CLI (`--sandbox`, `--sandbox-net`, `--shell-timeout`,
    /// `--auto-approve`, `--trust`); se deriva `for_worktree` por candidato.
    pub policy: WorkspacePolicy,
    pub snapshot_dir: Option<std::path::PathBuf>,
    pub tick: Option<u64>,
    /// `--models` (lista cruda; se valida aquí para mensajes accionables).
    pub models: Option<String>,
    pub verify: Option<String>,
    /// `--out <path.json>` (informe JSON pretty).
    pub out: Option<std::path::PathBuf>,
    /// `--goal`: si no hay agente activo en el tick, se spawnea un agente raíz con este goal.
    pub goal: Option<String>,
    pub max_steps: Option<i64>,
    pub auto_approve: bool,
    pub shell_timeout_secs: Option<u64>,
    /// Override del directorio base de *worktrees* (tests herméticos); `None` → temp.
    pub wt_base: Option<std::path::PathBuf>,
}

/// Ejecuta el benchmark completo y devuelve el reporte (tabla Markdown a stdout,
/// JSON a `--out`). Errores de validación: mensajes accionables, sin I/O previo.
pub fn run_bench(args: &BenchArgs) -> Result<BenchReport, String> {
    // 1) Validación accionable antes de cualquier I/O.
    let snapshot_dir = args.snapshot_dir.as_deref().ok_or_else(|| {
        "bench requires --snapshot-dir <dir> (the snapshot store to rewind from)".to_string()
    })?;
    let tick = args.tick.ok_or_else(|| {
        "bench requires --tick <n> (the snapshot tick to resume from)".to_string()
    })?;
    let models_raw = args.models.as_deref().ok_or_else(|| {
        "bench requires --models <list> (e.g. `gemini:gemini-2.5-pro,openai:gpt-4o,replay:label=run.tr`)".to_string()
    })?;
    let candidates = parse_candidates(models_raw)?;
    for c in &candidates {
        if let BenchCandidateSpec::Live { provider, .. } = c {
            LlmProvider::parse(provider).map_err(|e| format!("bench candidate: {e}"))?;
        }
    }

    // 2) Snapshot en el store (solo lectura).
    let store = PackSnapshotStore::new(snapshot_dir);
    if let Some(err) = store.corruption_error() {
        return Err(format!("bench: snapshot store is corrupt: {err}"));
    }
    let resolved_tick = store
        .resolve_tick_or_floor(tick)
        .ok_or_else(|| {
            format!(
                "bench: no snapshot for tick {tick} in `{}` (latest: {:?}) — run the session with --snapshot-dir first",
                snapshot_dir.display(),
                store.latest_tick()
            )
        })?;
    if resolved_tick != tick {
        eprintln!(
            "[bench] warning: no exact snapshot for tick {tick}; using nearest at tick {resolved_tick}"
        );
    }

    // 3) Directorio base de *worktrees* (temp por defecto; override en tests).
    let wt_base = args.wt_base.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("openonyx-bench-{}", std::process::id()))
    });
    std::fs::create_dir_all(&wt_base).map_err(|e| {
        format!(
            "bench: cannot create worktree base `{}`: {e}",
            wt_base.display()
        )
    })?;

    // 4) Candidatos en secuencia (wall_time_ms limpio).
    let mut results = Vec::with_capacity(candidates.len());
    for (idx, candidate) in candidates.iter().enumerate() {
        let label = candidate.display();
        eprintln!(
            "[bench] candidate {}/{}: {label}",
            idx + 1,
            candidates.len()
        );
        let t0 = Instant::now();
        let mut result = run_candidate(args, &wt_base, idx, candidate, resolved_tick)
            .unwrap_or_else(|e| BenchResult {
                model: label.clone(),
                passed_verify: false,
                ticks: 0,
                tool_calls: 0,
                input_tokens: 0,
                cached_tokens: 0,
                output_tokens: 0,
                estimated_cost_usd: 0.0,
                wall_time_ms: 0,
                verify_exit_code: None,
                agent_state: "unknown".to_string(),
                error: Some(e),
            });
        result.wall_time_ms = t0.elapsed().as_millis() as u64;
        results.push(result);
    }

    // 5) Informe.
    let report = BenchReport {
        metadata: BenchMetadata {
            snapshot_dir: snapshot_dir.display().to_string(),
            tick: resolved_tick,
            verify_cmd: args.verify.clone(),
            max_steps: args.max_steps,
        },
        results: results.clone(),
    };
    print!("{}", render_markdown(&results));
    if let Some(out) = &args.out {
        if let Some(parent) = out.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| format!("bench: cannot serialize report: {e}"))?;
        std::fs::write(out, json)
            .map_err(|e| format!("bench: cannot write report `{}`: {e}", out.display()))?;
        eprintln!("[bench] report written to {}", out.display());
    }

    // 6) Limpieza best-effort del base dir solo si lo creamos nosotros.
    if args.wt_base.is_none() {
        let _ = std::fs::remove_dir_all(&wt_base);
    }
    Ok(report)
}

/// `job_id` determinista y válido para el worktree del candidato (≤64 chars).
fn bench_job_id(idx: usize, label: &str) -> String {
    let sanitized: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let prefix = format!("bench-{}-{idx}-", std::process::id());
    let budget = 64usize.saturating_sub(prefix.len());
    let label_part: String = sanitized.chars().take(budget.max(1)).collect();
    if label_part.is_empty() {
        format!("{prefix}candidate")
    } else {
        format!("{prefix}{label_part}")
    }
}

/// Ejecuta un candidato en su *worktree* (creación → rewind → host desde snapshot →
/// run live/replay → verify) y devuelve la métrica. El *worktree* se retira siempre (RAII).
fn run_candidate(
    args: &BenchArgs,
    wt_base: &std::path::Path,
    idx: usize,
    candidate: &BenchCandidateSpec,
    tick: u64,
) -> Result<BenchResult, String> {
    let label = candidate.display();
    let job_id = bench_job_id(idx, &label);
    validate_job_id(&job_id).map_err(|e| format!("bench: invalid worktree job id: {e}"))?;

    let mgr =
        WorktreeManager::with_base_dir_override(&args.workspace, Some(wt_base)).map_err(|e| {
            format!(
                "bench: worktree manager for `{}`: {e}",
                args.workspace.display()
            )
        })?;
    let handle = mgr
        .create_worktree(&job_id, 0)
        .map_err(|e| format!("bench: cannot create worktree `{job_id}`: {e}"))?;
    let guard = BenchWorktreeGuard::new(mgr, handle);
    let wt_path = guard.handle().worktree_path.clone();

    // Rebobina el *worktree* al estado del workspace en `tick` (no-op limpio si no hay
    // efectos posteriores: `reconcile` solo restaura lo que el ledger registró tras `tick`).
    let config = AgentConfig {
        default_max_steps: args.max_steps.unwrap_or(64),
        default_timeout_ticks: 128,
        ..AgentConfig::default()
    };
    let snapshot_dir = args.snapshot_dir.as_deref().expect("validated above");
    let _rewind =
        crate::ops::reconcile_rewind_effects_report(&wt_path, snapshot_dir, tick, &config, false)
            .map_err(|e| format!("bench: rewind to tick {tick}: {e}"))?;

    // Host fresco sobre el store (solo lectura: sin milestones ni snapshot de salida).
    let opts = SessionOptions {
        snapshot_dir: Some(snapshot_dir.to_path_buf()),
        save_transcript: None,
        save_step_snapshots: false,
        snapshot_every: None,
        max_snapshots: None,
    };
    let (mut host, ws_vocab) =
        build_host_with_options(1, config, &opts, None, Some(PendingCheckpoints::default()))
            .map_err(|e| {
                format!(
                    "bench: cannot build host over `{}`: {e}",
                    snapshot_dir.display()
                )
            })?;
    host.request_restore(&tick.to_string());
    host.step(&mut HeadlessDriver::new());
    host.set_tick(tick.saturating_add(1));

    // Herramientas re-enraizadas en el *worktree* (offline, auto-approve confinado).
    let wt_policy = args
        .policy
        .for_worktree(&wt_path, &SubagentNetworkPolicy::NativeOffline)
        .map_err(|e| format!("bench: cannot derive worktree policy: {e}"))?;
    let tools = WorkspaceTools::new(wt_policy.clone());
    let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
        .with_mode(WorkspaceMode::Build)
        .with_workspace_vocab(ws_vocab);

    // Si no hay agente activo en el tick, se spawnea un agente raíz con `--goal`.
    let has_active = host
        .agents()
        .iter()
        .any(|a| matches!(host.agent_state(*a), Some(s) if !s.is_terminal()));
    if !has_active {
        let goal = args.goal.as_deref().ok_or_else(|| {
            format!(
                "bench: no active agent at tick {tick} and no --goal given to spawn a root agent"
            )
        })?;
        let tool_names: Vec<&str> = WorkspaceTools::all_tool_names().to_vec();
        let mut spec = AgentSpec::new("openonyx", goal).with_tools(tool_names);
        if !args.auto_approve {
            spec = spec.with_approval_tools(WorkspaceTools::approval_required_names().to_vec());
        }
        if let Some(ms) = args.max_steps {
            spec = spec.with_max_steps(ms);
        }
        host.spawn(&spec);
    }

    let (settled, ticks, tool_calls, input_tokens, cached_tokens, output_tokens, cost, state) =
        match candidate {
            BenchCandidateSpec::Live {
                provider, model, ..
            } => {
                // Resuelve credenciales/base existentes igual que cualquier sesión.
                let mut cli = args.cli.clone();
                cli.provider = Some(LlmProvider::parse(provider)?);
                cli.model = Some(model.clone());
                let llm =
                    crate::resolve_llm_config(&cli, &args.workspace).map_err(|e| e.to_string())?;
                let telemetry = SessionTelemetryHandle::new(&llm.model, llm.api_base.is_some());
                let responder = crate::build_sync_responder(
                    &llm,
                    Some(telemetry.clone()),
                    &[],
                    &Arc::new(Vec::new()),
                );
                let mut driver = onyx_soul_agents::RecordingBridge::new(host.vocab(), responder)
                    .wrapping(ws_bridge);
                let settled = step_until_settled(&mut host, &mut driver, MAX_BENCH_TICKS);
                let snap = telemetry.snapshot();
                let tool_calls = driver
                    .transcript()
                    .entries
                    .iter()
                    .filter(|e| matches!(e.reply.verb, Verb::Call))
                    .count() as u64;
                let state = last_agent_state(&host);
                (
                    settled,
                    host.ticks().saturating_sub(tick.saturating_add(1)),
                    tool_calls,
                    snap.cumulative.input_tokens,
                    snap.cumulative.cached_input_tokens,
                    snap.cumulative.output_tokens,
                    snap.estimated_cost_usd,
                    state,
                )
            }
            BenchCandidateSpec::Replay { path, .. } => {
                let bytes = std::fs::read(path).map_err(|e| {
                    format!("bench: cannot read transcript `{}`: {e}", path.display())
                })?;
                let transcript = Transcript::from_bytes(&bytes)
                    .map_err(|e| format!("bench: invalid transcript `{}`: {e}", path.display()))?;
                let mut driver = BenchTranscriptDriver::new(host.vocab(), ws_bridge, transcript);
                let settled = step_until_settled(&mut host, &mut driver, MAX_BENCH_TICKS);
                let tool_calls = driver.tool_calls.load(Ordering::SeqCst) as u64;
                let state = last_agent_state(&host);
                (
                    settled,
                    host.ticks().saturating_sub(tick.saturating_add(1)),
                    tool_calls,
                    0,
                    0,
                    0,
                    0.0,
                    state,
                )
            }
        };

    // `--verify` sandboxed en el *worktree*; sin `--verify` → `agent_done` (documentado:
    // `passed_verify` = el agente terminó `Done` y la sesión se asentó).
    let timeout = Duration::from_secs(
        args.shell_timeout_secs
            .unwrap_or(DEFAULT_VERIFY_TIMEOUT_SECS)
            .max(1),
    );
    let (passed_verify, verify_exit_code, mut error) = match &args.verify {
        Some(cmd) => match exec_bench_verify(&wt_policy, cmd, timeout) {
            VerifyOutcome::Passed => (true, Some(0), None),
            VerifyOutcome::Failed { code, diag } => (
                false,
                Some(code),
                Some(format!("verify failed (exit {code}): {diag}")),
            ),
            VerifyOutcome::Timeout { secs, diag } => (
                false,
                None,
                Some(format!("verify timed out after {secs}s: {diag}")),
            ),
            VerifyOutcome::Spawn(e) => (false, None, Some(format!("verify spawn failed: {e}"))),
            VerifyOutcome::Blocked => (
                false,
                None,
                Some("verify blocked by [permissions].deny".to_string()),
            ),
        },
        None => (settled && state == "done", None, None),
    };
    if !settled {
        error = Some(match error {
            Some(e) => format!("bench: not settled after {MAX_BENCH_TICKS} ticks; also: {e}"),
            None => format!("bench: not settled after {MAX_BENCH_TICKS} ticks"),
        });
    }

    Ok(BenchResult {
        model: label,
        passed_verify,
        ticks,
        tool_calls,
        input_tokens,
        cached_tokens,
        output_tokens,
        estimated_cost_usd: cost,
        wall_time_ms: 0, // se rellena en `run_bench` (incluye worktree + rewind).
        verify_exit_code,
        agent_state: state,
        error,
    })
}

fn last_agent_state(host: &AgentHost) -> String {
    host.agents()
        .last()
        .and_then(|a| host.agent_state(*a))
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| "none".to_string())
}

/// `run_session`-equivalente sin efectos colaterales (no guarda snapshots ni transcript):
/// ticks hasta que el host se asienta o se agota `max_ticks`.
///
/// El orden `step` → `is_settled()` es importante: un `spawn` recién pedido aún no crea
/// el agente hasta el primer `step`, así que comprobar *antes* de ejecutar vería
/// erróneamente un host asentado (solo agentes terminales restaurados del snapshot).
fn step_until_settled(
    host: &mut AgentHost,
    driver: &mut dyn PlatformDriver,
    max_ticks: usize,
) -> bool {
    for _ in 0..max_ticks {
        host.step(driver);
        if host.is_settled() {
            return true;
        }
    }
    host.is_settled()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests (TEST-05.5)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    use onyx_soul_agents::{AgentView, ConceptId, Logged, LoggedCommand};

    /// RAII de directorio temporal (auto-`remove_dir_all` en `Drop`).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir()
                .join(format!("openonyx-benchtest-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@local")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@local")
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_git_repo(dir: &Path) {
        run_git_in(dir, &["init", "-q"]);
        // Sin conversión CRLF en el repo de pruebas: los asserts comparan bytes exactos
        // y `git worktree add` / `read-tree` aplican `autocrlf` de la config global.
        run_git_in(dir, &["config", "core.autocrlf", "false"]);
    }

    fn git_commit_all(dir: &Path, msg: &str) {
        run_git_in(dir, &["add", "-A"]);
        run_git_in(dir, &["commit", "-q", "-m", msg]);
    }

    fn git_head_sha(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// *Worktrees* `onyx/sub/*` registrados en el repo (0 = sin residuos).
    fn onyx_worktree_count(dir: &Path) -> usize {
        let out = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.starts_with("branch ") && line.contains("onyx/sub/"))
            .count()
    }

    /// Ramas `onyx/sub/*` residuales (0 = limpias).
    fn onyx_sub_branch_count(dir: &Path) -> usize {
        let out = Command::new("git")
            .args([
                "for-each-ref",
                "--format",
                "%(refname)",
                "refs/heads/onyx/sub/",
            ])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    }

    #[test]
    fn parse_candidates_live_list() {
        let v = parse_candidates("gemini:gemini-2.5-pro,openai:gpt-4o,anthropic:claude-sonnet-4-5")
            .unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(
            v[0],
            BenchCandidateSpec::Live {
                provider: "gemini".to_string(),
                model: "gemini-2.5-pro".to_string(),
                label: "gemini:gemini-2.5-pro".to_string(),
            }
        );
        assert_eq!(v[1].display(), "openai:gpt-4o");
        assert_eq!(v[2].display(), "anthropic:claude-sonnet-4-5");
    }

    #[test]
    fn parse_candidates_replay_windows_paths_keep_colon() {
        let v = parse_candidates(r"replay:model-pass=C:\tmp\a.tr,replay:model-fail=S:\tmp\b.tr")
            .unwrap();
        assert_eq!(v.len(), 2);
        match &v[0] {
            BenchCandidateSpec::Replay { label, path } => {
                assert_eq!(label, "model-pass");
                assert_eq!(path, &std::path::PathBuf::from(r"C:\tmp\a.tr"));
            }
            other => panic!("expected Replay, got {other:?}"),
        }
        match &v[1] {
            BenchCandidateSpec::Replay { label, path } => {
                assert_eq!(label, "model-fail");
                assert_eq!(path, &std::path::PathBuf::from(r"S:\tmp\b.tr"));
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn parse_candidates_replay_bare_path_uses_file_stem_label() {
        let v = parse_candidates(r"replay:C:\tmp\myrun.tr").unwrap();
        match &v[0] {
            BenchCandidateSpec::Replay { label, path } => {
                assert_eq!(label, "myrun");
                assert_eq!(path, &std::path::PathBuf::from(r"C:\tmp\myrun.tr"));
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn parse_candidates_errors_are_actionable() {
        assert!(parse_candidates("").is_err());
        assert!(parse_candidates("   ,  ").is_err());
        let e = parse_candidates("no-colon-here").unwrap_err();
        assert!(e.contains("unknown candidate"), "{e}");
        let e = parse_candidates("gemini:").unwrap_err();
        assert!(e.contains("malformed live candidate"), "{e}");
        let e = parse_candidates("replay:").unwrap_err();
        assert!(e.contains("replay candidate needs"), "{e}");
        // `replay:=foo.tr` (label vacío) y `replay:lbl=` (path vacío): el mensaje
        // interpola el candidato tal y como se escribió (sin literal `{entry}`).
        let e = parse_candidates("replay:=foo.tr").unwrap_err();
        assert!(e.contains("malformed replay candidate"), "{e}");
        assert!(e.contains("replay:=foo.tr"), "{e}");
        let e = parse_candidates("replay:lbl=").unwrap_err();
        assert!(e.contains("malformed replay candidate"), "{e}");
        assert!(e.contains("replay:lbl="), "{e}");
    }

    #[test]
    fn bench_requires_snapshot_dir_tick_and_models() {
        let cli = crate::CliArgs::simple(crate::Mode::Bench { tick: None });
        let tmp = std::env::temp_dir();
        let policy = WorkspacePolicy::new(&tmp).unwrap();
        let mut base = BenchArgs {
            cli: &cli,
            workspace: tmp.clone(),
            policy,
            snapshot_dir: None,
            tick: None,
            models: None,
            verify: None,
            out: None,
            goal: None,
            max_steps: None,
            auto_approve: true,
            shell_timeout_secs: None,
            wt_base: None,
        };
        let e = run_bench(&base).unwrap_err();
        assert!(e.contains("--snapshot-dir"), "{e}");
        base.snapshot_dir = Some(tmp.join("no-such-snap-store"));
        let e = run_bench(&base).unwrap_err();
        assert!(e.contains("--tick"), "{e}");
        base.tick = Some(1);
        let e = run_bench(&base).unwrap_err();
        assert!(e.contains("--models"), "{e}");
    }

    #[test]
    fn bench_unknown_provider_is_actionable() {
        let cli = crate::CliArgs::simple(crate::Mode::Bench { tick: None });
        let snap = TempDir::new("bench-badprovider");
        let policy = WorkspacePolicy::new(&snap.0).unwrap();
        let args = BenchArgs {
            cli: &cli,
            workspace: snap.0.clone(),
            policy,
            snapshot_dir: Some(snap.0.join("snaps")),
            tick: Some(1),
            models: Some("bogus:model-x".to_string()),
            verify: None,
            out: None,
            goal: None,
            max_steps: None,
            auto_approve: true,
            shell_timeout_secs: None,
            wt_base: None,
        };
        let e = run_bench(&args).unwrap_err();
        assert!(e.contains("unsupported provider"), "{e}");
    }

    #[test]
    fn markdown_table_renders_all_columns_and_rows() {
        let results = vec![
            BenchResult {
                model: "good".into(),
                passed_verify: true,
                ticks: 7,
                tool_calls: 2,
                input_tokens: 0,
                cached_tokens: 0,
                output_tokens: 0,
                estimated_cost_usd: 0.0,
                wall_time_ms: 1234,
                verify_exit_code: Some(0),
                agent_state: "done".into(),
                error: None,
            },
            BenchResult {
                model: "bad".into(),
                passed_verify: false,
                ticks: 9,
                tool_calls: 3,
                input_tokens: 0,
                cached_tokens: 0,
                output_tokens: 0,
                estimated_cost_usd: 0.0,
                wall_time_ms: 999,
                verify_exit_code: Some(1),
                agent_state: "done".into(),
                error: None,
            },
        ];
        let md = render_markdown(&results);
        assert!(md.contains(
            "| model | passed_verify | ticks | tool_calls | input_tokens | cached_tokens | output_tokens | estimated_cost_usd | wall_time_ms |"
        ));
        assert!(md.contains("| good | true | 7 | 2 | 0 | 0 | 0 | 0 | 1234 |"));
        assert!(md.contains("| bad | false | 9 | 3 | 0 | 0 | 0 | 0 | 999 |"));
    }

    #[test]
    fn report_json_round_trips_with_all_keys() {
        let report = BenchReport {
            metadata: BenchMetadata {
                snapshot_dir: ".snaps".into(),
                tick: 10,
                verify_cmd: Some("grep -q fixed answer.txt".into()),
                max_steps: Some(16),
            },
            results: vec![BenchResult {
                model: "m".into(),
                passed_verify: true,
                ticks: 1,
                tool_calls: 1,
                input_tokens: 5,
                cached_tokens: 2,
                output_tokens: 3,
                estimated_cost_usd: 0.1234,
                wall_time_ms: 42,
                verify_exit_code: Some(0),
                agent_state: "done".into(),
                error: None,
            }],
        };
        let json = serde_json::to_string_pretty(&report).unwrap();
        let back: BenchReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        // Todas las claves de SDD-05 §3.4 presentes en el JSON.
        for key in [
            "passed_verify",
            "ticks",
            "tool_calls",
            "input_tokens",
            "cached_tokens",
            "output_tokens",
            "estimated_cost_usd",
            "wall_time_ms",
        ] {
            assert!(json.contains(key), "missing key {key}");
        }
    }

    /// Transcripción `ONYXTR2` sintética: un comando (goal) + `fs_write answer.txt` + `Done`.
    /// `ONYXTR2` se fuerza porque hay `commands` (a `to_bytes` le basta uno). Los args de
    /// `fs_write` se construyen con `serde_json` para escapar correctamente el newline de
    /// `content` (un `\n` crudo dentro de un string JSON es inválido y la herramienta
    /// fallaría al parsear los args).
    fn synth_transcript_bytes(goal: &str, content: &str) -> Vec<u8> {
        let write_args =
            serde_json::json!({ "path": "answer.txt", "content": content }).to_string();
        let mut tr = Transcript::default();
        tr.commands.push(LoggedCommand {
            poll: 0,
            command: goal.to_string(),
        });
        tr.entries.push(Logged {
            poll: 0,
            reply: Reply::call_structured(ConceptId(0), 1, "fs_write", write_args),
        });
        tr.entries.push(Logged {
            poll: 1,
            reply: Reply::done(ConceptId(0), 2, "done"),
        });
        tr.to_bytes()
    }

    /// TEST-05.5 (E2E): repo Git con `answer.txt = "broken\n"`, sesión de snapshots que deja
    /// un efecto posterior al tick (edición "stale"), dos transcripciones `ONYXTR2`
    /// (`model-good` / `model-bad`), `run_bench` con `--verify` portable + `--out`.
    /// Aserciones: good pasa verify / bad falla; JSON completo; raíz 100% intacta;
    /// 0 *worktrees* ni ramas `onyx/sub/bench-*` residuales; `reconcile` rebobina.
    #[test]
    fn bench_e2e_two_replay_transcripts_verify_and_leave_root_intact() {
        const SNAP_TICK: u64 = 2;
        let repo = TempDir::new("bench-e2e-repo");
        init_git_repo(&repo.0);
        fs::write(repo.0.join("answer.txt"), "broken\n").unwrap();
        git_commit_all(&repo.0, "initial");
        let head_before = git_head_sha(&repo.0);

        let snap_dir = TempDir::new("bench-e2e-snaps");
        let wt_base = TempDir::new("bench-e2e-wt");
        let trs = TempDir::new("bench-e2e-trs");
        let out_json = trs.0.join("bench_report.json");

        // ── 1) Sesión de snapshots: fs_write "stale_after_snapshot" + 2 snapshots ──
        let config = AgentConfig {
            default_max_steps: 16,
            default_timeout_ticks: 48,
            ..AgentConfig::default()
        };
        let opts = SessionOptions {
            snapshot_dir: Some(snap_dir.0.clone()),
            save_step_snapshots: false,
            ..SessionOptions::default()
        };
        let (mut host, ws_vocab) = build_host_with_options(
            1,
            config.clone(),
            &opts,
            None,
            Some(PendingCheckpoints::default()),
        )
        .unwrap();
        let root_policy = WorkspacePolicy::new(&repo.0).unwrap();
        let tools = WorkspaceTools::new(root_policy.clone());
        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode(WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab);
        // Dos `Say` (t1, t2) antes del `fs_write` (t3): el efecto queda registrado con
        // `tick = 3 > SNAP_TICK = 2`, de modo que `reconcile` lo vea "huérfano" y
        // restablezca el worktree al árbol pre-escritura (`"broken\n"`).
        let mut driver = onyx_soul_agents::RecordingBridge::new(host.vocab(), |req: &AgentView| {
            if req.memory.iter().any(|(k, _)| k == "observe") {
                Some(Reply::done(req.agent, req.request, "baseline recorded"))
            } else {
                let says = req.memory.iter().filter(|(k, _)| k == "say").count();
                match says {
                    0 => Some(Reply::say(req.agent, req.request, "inspecting repo")),
                    1 => Some(Reply::say(req.agent, req.request, "planning fix")),
                    _ => Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"answer.txt","content":"stale_after_snapshot\n"}"#,
                    )),
                }
            }
        })
        .wrapping(ws_bridge);
        let spec = crate::session::make_spec("record baseline", true, &[]);
        host.spawn(&spec);
        assert!(host.run_until_settled(&mut driver, 32));
        assert_eq!(
            host.result(host.agents()[0]).as_deref(),
            Some("baseline recorded")
        );

        // Dos snapshots con el mismo grafo final: tick 2 (bench) y tick final (latest).
        let final_tick = host.ticks();
        assert!(
            final_tick > SNAP_TICK,
            "la sesión debe correr más de {SNAP_TICK} ticks"
        );
        host.request_snapshot(&SNAP_TICK.to_string());
        host.step(&mut HeadlessDriver::new());
        host.request_snapshot(&final_tick.to_string());
        host.step(&mut HeadlessDriver::new());

        let store = PackSnapshotStore::new(&snap_dir.0);
        assert!(store.has_tick(SNAP_TICK));
        assert_eq!(store.latest_tick(), Some(final_tick));
        // La sesión escribió en la raíz (estado "stale"): es el punto de partida del bench.
        assert_eq!(
            fs::read_to_string(repo.0.join("answer.txt")).unwrap(),
            "stale_after_snapshot\n"
        );

        // ── 2) `reconcile` rebobina un worktree al árbol del tick (edición posterior) ──
        let mgr = WorktreeManager::with_base_dir_override(&repo.0, Some(&wt_base.0)).unwrap();
        let h = mgr.create_worktree("bench-recheck-0", 0).unwrap();
        let report = crate::ops::reconcile_rewind_effects_report(
            &h.worktree_path,
            &snap_dir.0,
            SNAP_TICK,
            &config,
            false,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(h.worktree_path.join("answer.txt")).unwrap(),
            "broken\n",
            "reconcile debe restaurar el worktree al árbol del tick ({:?})",
            report.lines
        );
        mgr.remove_worktree(&h).unwrap();

        // ── 3) Transcripciones ONYXTR2 (una por "modelo") ──
        let good_tr = trs.0.join("model-good.tr");
        let bad_tr = trs.0.join("model-bad.tr");
        fs::write(
            &good_tr,
            synth_transcript_bytes("fix answer.txt", "fixed\n"),
        )
        .unwrap();
        fs::write(
            &bad_tr,
            synth_transcript_bytes("fix answer.txt", "still_broken\n"),
        )
        .unwrap();
        // Verificación: cabecera ONYXTR2 (hay comandos) y decodifican.
        for p in [&good_tr, &bad_tr] {
            let bytes = fs::read(p).unwrap();
            assert!(bytes.starts_with(b"ONYXTR2\n"), "debe ser ONYXTR2");
            assert!(Transcript::from_bytes(&bytes).is_ok());
        }

        // ── 4) Bench: 2 candidatos replay + --verify portable + --out ──
        let verify_cmd = if cfg!(windows) {
            "findstr /C:fixed answer.txt"
        } else {
            "grep -q fixed answer.txt"
        };
        let cli = crate::CliArgs::simple(crate::Mode::Bench {
            tick: Some(SNAP_TICK),
        });
        let args = BenchArgs {
            cli: &cli,
            workspace: repo.0.clone(),
            policy: root_policy,
            snapshot_dir: Some(snap_dir.0.clone()),
            tick: Some(SNAP_TICK),
            models: Some(format!(
                "replay:model-good={},replay:model-bad={}",
                good_tr.display(),
                bad_tr.display()
            )),
            verify: Some(verify_cmd.to_string()),
            out: Some(out_json.clone()),
            goal: Some("fix answer.txt".to_string()),
            max_steps: None,
            auto_approve: true,
            shell_timeout_secs: Some(60),
            wt_base: Some(wt_base.0.clone()),
        };
        let report = run_bench(&args).unwrap();

        // ── 5) Aserciones por candidato ──
        let r_good = report
            .results
            .iter()
            .find(|r| r.model == "model-good")
            .unwrap();
        assert!(
            r_good.passed_verify,
            "good debe pasar verify: {:?}",
            r_good.error
        );
        assert_eq!(r_good.verify_exit_code, Some(0));
        assert!(r_good.tool_calls >= 1, "good ejecutó fs_write");
        assert!(r_good.ticks > 0);
        assert!(r_good.error.is_none(), "{:?}", r_good.error);
        assert!(r_good.input_tokens == 0 && r_good.estimated_cost_usd == 0.0);

        let r_bad = report
            .results
            .iter()
            .find(|r| r.model == "model-bad")
            .unwrap();
        assert!(!r_bad.passed_verify, "bad debe fallar verify");
        assert_eq!(r_bad.verify_exit_code, Some(1));
        assert!(r_bad.tool_calls >= 1, "bad ejecutó fs_write");
        assert!(r_bad.ticks > 0);

        // ── 6) JSON `--out` parseable con los dos registros y todas las claves ──
        let json = fs::read_to_string(&out_json).unwrap();
        let parsed: BenchReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.results.len(), 2);
        assert!(
            parsed
                .results
                .iter()
                .any(|r| r.model == "model-good" && r.passed_verify)
        );
        assert!(
            parsed
                .results
                .iter()
                .any(|r| r.model == "model-bad" && !r.passed_verify)
        );
        assert_eq!(parsed.metadata.tick, SNAP_TICK);
        assert_eq!(parsed.metadata.verify_cmd.as_deref(), Some(verify_cmd));
        for key in [
            "passed_verify",
            "ticks",
            "tool_calls",
            "estimated_cost_usd",
            "wall_time_ms",
            "verify_exit_code",
            "agent_state",
        ] {
            assert!(json.contains(key), "missing key {key}");
        }
        // Tabla Markdown con ambas filas/columnas.
        let md = render_markdown(&report.results);
        assert!(md.contains("| model-good | true |"));
        assert!(md.contains("| model-bad | false |"));
        assert!(md.contains("passed_verify"));

        // ── 7) Raíz 100% intacta y 0 residuos en el repo ──
        assert_eq!(
            fs::read_to_string(repo.0.join("answer.txt")).unwrap(),
            "stale_after_snapshot\n",
            "el bench no debe tocar la raíz"
        );
        assert_eq!(git_head_sha(&repo.0), head_before, "HEAD no se mueve");
        assert_eq!(onyx_worktree_count(&repo.0), 0, "0 worktrees residuales");
        assert_eq!(
            onyx_sub_branch_count(&repo.0),
            0,
            "0 ramas onyx/sub/* residuales"
        );
        // El base dir de worktrees quedó sin contenido (todos desmontados por el guard
        // RAII). `resolve_worktree_base_dir` anida el *worktree* base en
        // `<override>/<workspace_name>`, así que la aserción recorre también esa
        // subcarpeta (que debe estar vacía).
        let mut remaining = 0usize;
        for entry in fs::read_dir(&wt_base.0).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                remaining += fs::read_dir(entry.path()).unwrap().count();
            } else {
                remaining += 1;
            }
        }
        assert_eq!(
            remaining, 0,
            "no puede quedar ningún worktree en el base dir"
        );
    }
}
