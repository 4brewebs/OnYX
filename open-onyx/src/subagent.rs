//! Gestor asíncrono de subagentes en *worktrees* Git aislados (`SDD-09` Fase 2).
//!
//! Implementación pura sobre `std::thread` y `std::sync::mpsc` (**0 `tokio`**, **0 cambios en `onyx-core`**):
//! - Cada subagente arranca en un hilo dedicado con su propio `AgentHost`, snapshot clonado del
//!   contexto de workspace (`AgentGraph::export`), semilla determinista `blake3(root_seed, spawn_tick, job_id)`
//!   y `WorkspaceTools` confinado a su `worktree_path` (`SandboxMode::WorkspaceWrite`,
//!   `SubagentNetworkPolicy::NativeOffline`, `ProcessJob` RAII).
//! - `root` nunca entra en `WaitingChildren`: recibe `job_id` inmediatamente y sigue avanzando ticks.
//! - Al descartar (`subagent_discard`), fusionar (`subagent_merge`) o soltar (`Drop`), el hilo
//!   worker se une (`join()`) y todos los `ProcessJob` RAII + servicios Sprout se terminan y sueltan
//!   **antes** de invocar `WorktreeManager::remove_worktree`, evitando bloqueos de archivos en Windows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use onyx_soul_agents::{
    AgentConfig, AgentGraph, AgentHost, AgentSpec, AgentState, AttrRegistry, ConceptId, EventBus,
    Graph, GraphView, HeadlessDriver, IngressEvent, MemStore, OnyxEvent, PlatformDriver,
    RecordingBridge, Reply, Responder, SnapshotStore, SurfaceBinding, Value, Vocab,
    pending_approvals, pending_tool_calls,
};
use onyx_workspace::{
    ImpactAssessment, MergeDeltaVerdict, PermissionDecision, SubagentNetworkPolicy,
    SubagentProposal, WorkspaceLedgerSystem, WorkspaceMode, WorkspaceModeHandle, WorkspacePolicy,
    WorkspaceToolBridge, WorkspaceTools, WorkspaceVocab, WorktreeHandle, WorktreeManager,
    WorktreeMergeOutcome, apply_impact_escalation, classify_merge_delta, is_async_subagent_tool,
    is_control_plane_rel_path, scrub_secrets, validate_job_id,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::external_driver::{ExternalDriverRunner, ExternalHarnessDriver};

/// Fábrica thread-safe para construir un `Responder` independiente por cada hilo de subagente.
pub type ResponderFactory = Arc<dyn Fn() -> Box<dyn Responder> + Send + Sync>;

/// Decisión efectiva para la aprobación de `subagent_merge` con un ticket sellado
/// (`SDD-10` Fase 4): única fuente de verdad compartida por [`SubagentManagerDriver`]
/// (auto-aprobación) y por las superficies TUI/Web/CLI (¿mostrar el prompt `y/n`?).
///
/// - El delta toca la superficie de **gobernanza** (`sprout.toml`, `.github/workflows/**`)
///   → `Ask` obligatorio, aunque la política estética permita el merge (`SDD-10` Fase 3).
/// - En el resto, la decisión del policy + escalado por impacto (Scent).
///
/// Los sitios que la consumen ya han descartado el `Deny` estático y el
/// [`MergePreviewState::ControlPlaneDenied`], por lo que aquí solo entran `Allow`/`Ask`.
pub(crate) fn effective_merge_decision(
    static_dec: PermissionDecision,
    ticket: &MergePreviewTicket,
) -> PermissionDecision {
    if classify_merge_delta(&ticket.changed_files) == MergeDeltaVerdict::Ask {
        PermissionDecision::Ask
    } else {
        apply_impact_escalation(static_dec, ticket.impact.as_ref())
    }
}

/// Backend de ejecución de un job de subagente (`SDD-10` Fase 1):
/// - [`WorkerBackend::Native`]: motor OnYX en proceso (snapshot del grafo + semilla derivada).
/// - [`WorkerBackend::External`]: harness CLI externo (`opencode`, `claude`, ...) ejecutado
///   dentro del mismo worktree Git aislado; solo el disco es la verdad (captura `git commit-tree`).
pub enum WorkerBackend {
    Native {
        graph_snapshot: Vec<u8>,
        seed: u64,
    },
    External {
        driver: Arc<dyn ExternalHarnessDriver>,
    },
}

/// Techo por defecto de subagentes activos simultáneos para proteger CPU/RAM (p. ej. VPS).
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 3;
/// Variable de entorno opcional para configurar `--max-subagents` por defecto.
pub const ONYX_MAX_SUBAGENTS_ENV: &str = "ONYX_MAX_SUBAGENTS";
/// Prefijo interno reservado para inyectar propuestas completadas como `IngressEvent::Command`
/// grabado en `ONYXTR2` y reproducido offline sin invocar hilos ni LLMs.
pub const SUBAGENT_PROPOSAL_CMD_PREFIX: &str = "/__onyx_subagent_proposal ";

const DEFAULT_SUBAGENT_MAX_STEPS: i64 = 32;
const MAX_SUBAGENT_MAX_STEPS: i64 = 128;

/// Default del campo `driver` en el wire de propuestas: `"native"` (compat con `SDD-09`).
fn default_native_driver() -> String {
    "native".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SubagentProposalWire {
    job_id: String,
    /// Backend que ejecutó el job: `"native"` o el `id` del harness CLI externo (`SDD-10`).
    #[serde(default = "default_native_driver")]
    driver: String,
    spawn_tick: u64,
    branch_name: String,
    base_commit: String,
    sub_commit: String,
    ok: bool,
    summary: String,
    touched_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SubagentProposalEnvelope {
    proposal: SubagentProposalWire,
    cancelled: bool,
}

/// Serializa una [`SubagentProposal`] como comando interno determinista grabable en `ONYXTR2`.
pub fn encode_proposal_command(proposal: &SubagentProposal, cancelled: bool) -> String {
    let env = SubagentProposalEnvelope {
        proposal: SubagentProposalWire {
            job_id: proposal.job_id.clone(),
            driver: proposal.driver.clone(),
            spawn_tick: proposal.spawn_tick,
            branch_name: proposal.branch_name.clone(),
            base_commit: proposal.base_commit.clone(),
            sub_commit: proposal.sub_commit.clone(),
            ok: proposal.ok,
            summary: proposal.summary.clone(),
            touched_files: proposal.touched_files.clone(),
        },
        cancelled,
    };
    let payload = serde_json::to_string(&env).unwrap_or_else(|_| "{}".to_string());
    format!("{SUBAGENT_PROPOSAL_CMD_PREFIX}{payload}")
}

/// Decodifica un comando interno `/__onyx_subagent_proposal {...}` inyectado en frontera de tick.
pub fn decode_proposal_command(raw_cmd: &str) -> Option<(SubagentProposal, bool)> {
    let payload = raw_cmd.trim().strip_prefix(SUBAGENT_PROPOSAL_CMD_PREFIX)?;
    let env: SubagentProposalEnvelope = serde_json::from_str(payload).ok()?;
    Some((
        SubagentProposal {
            job_id: env.proposal.job_id,
            driver: env.proposal.driver,
            spawn_tick: env.proposal.spawn_tick,
            branch_name: env.proposal.branch_name,
            base_commit: env.proposal.base_commit,
            sub_commit: env.proposal.sub_commit,
            ok: env.proposal.ok,
            summary: env.proposal.summary,
            touched_files: env.proposal.touched_files,
        },
        env.cancelled,
    ))
}

/// Deriva una semilla determinista por subagente usando `blake3(root_seed, spawn_tick, job_id)`
/// (`SDD-09` §4 Fase 2).
pub fn derive_subagent_seed(root_seed: u64, spawn_tick: u64, job_id: &str) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"onyx-subagent-seed-v1");
    hasher.update(&root_seed.to_le_bytes());
    hasher.update(&spawn_tick.to_le_bytes());
    hasher.update(job_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(bytes)
}

/// Exporta una captura serializada (`AgentGraph::export`) del conocimiento no-agente del grafo
/// raíz (`ws.file`, `ws.plan`, `ws.mode`, `ws.services`, etc.) excluyendo agentes vivos y su
/// memoria privada para evitar que el host hijo quede bloqueado esperando al agente raíz o
/// colisione por `ConceptId`.
pub fn snapshot_context_graph(graph: &dyn GraphView, vocab: &Vocab) -> Vec<u8> {
    let agents: Vec<ConceptId> = graph.query(&[vocab.state]);
    let mut excluded: BTreeSet<u64> = agents.iter().map(|id| id.0).collect();
    for &agent in &agents {
        for mem in graph.related(agent, vocab.rel_memory) {
            excluded.insert(mem.0);
        }
    }

    let mut clone_graph = AgentGraph::new();
    let mut id_map: HashMap<ConceptId, ConceptId> = HashMap::new();
    let source_ids: Vec<ConceptId> = graph
        .query(&[])
        .into_iter()
        .filter(|id| !excluded.contains(&id.0))
        .collect();

    for &old_id in &source_ids {
        let new_id = clone_graph.create();
        id_map.insert(old_id, new_id);
    }

    for &old_id in &source_ids {
        let Some(&new_id) = id_map.get(&old_id) else {
            continue;
        };
        for (attr_name, val) in graph.props(old_id) {
            let attr = clone_graph.attr(&attr_name);
            let mapped = match val {
                Value::Concept(target) => match id_map.get(&target) {
                    Some(&mapped_target) => Value::Concept(mapped_target),
                    None => continue,
                },
                other => other,
            };
            clone_graph.set(new_id, attr, mapped);
        }
        for (rel_name, target) in graph.links(old_id) {
            if let Some(&mapped_target) = id_map.get(&target) {
                let rel = clone_graph.attr(&rel_name);
                clone_graph.relate(new_id, rel, mapped_target);
            }
        }
    }

    clone_graph.export()
}

/// Estado del ciclo de vida de un subagente asíncrono gestionado por [`SubagentManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentJobState {
    Running,
    Done(SubagentProposal),
    Failed(SubagentProposal),
    Merged(SubagentProposal),
    Discarded,
    Cancelled,
}

impl SubagentJobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done(_) => "done",
            Self::Failed(_) => "failed",
            Self::Merged(_) => "merged",
            Self::Discarded => "discarded",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn proposal(&self) -> Option<&SubagentProposal> {
        match self {
            Self::Done(p) | Self::Failed(p) | Self::Merged(p) => Some(p),
            Self::Running | Self::Discarded | Self::Cancelled => None,
        }
    }
}

/// Vista compacta serializable de un job para `subagent_status` y paneles UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentJobSummary {
    pub job_id: String,
    /// Backend de ejecución: `"native"` o el `id` del harness CLI externo (`SDD-10`).
    pub driver: String,
    pub name: String,
    pub goal: String,
    pub spawn_tick: u64,
    pub branch_name: String,
    pub worktree_path: PathBuf,
    pub status: &'static str,
    pub ok: Option<bool>,
    pub touched_files: Vec<String>,
    pub summary: String,
}

/// Ticket sellado durante la vista previa 3-way para cerrar ventanas TOCTOU al fusionar.
#[derive(Debug, Clone)]
pub struct MergePreviewTicket {
    #[allow(dead_code)]
    pub job_id: String,
    /// Clave `(agent_id, request_id)` de la solicitud de aprobación que selló este ticket.
    /// Permite a los redibujados periódicos de `TuiDriver`/`WebDriver` (`pump_idle`/SSE) y
    /// al propio `SubagentManagerDriver` reutilizarlo mientras el usuario decide, sin
    /// re-sellar `root_now_tree` ni relanzar `git write-tree` en cada frame. `None` en
    /// previews sin solicitud asociada. La guarda anti-TOCTOU de `apply_clean_merge_checked`
    /// sigue validando el árbol en el momento de aplicar (`SDD-10` Paso 0).
    pub approval_key: Option<(u64, u64)>,
    pub root_now_commit: String,
    pub root_now_tree: String,
    pub sub_commit: String,
    pub merged_tree: String,
    pub changed_files: Vec<String>,
    pub unified_diff: String,
    pub impact: Option<ImpactAssessment>,
}

/// Estado de la reconciliación 3-way previa a la aprobación y aplicación en disco.
/// `Clean` lleva el ticket completo por valor (diff, archivos e impacto) porque los
/// drivers lo renderizan antes de pedir la aprobación; el sobredimensionado respecto a
/// `NoOp`/`Conflict` es intencional (`SDD-10` Paso 0 añade `approval_key` al ticket).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum MergePreviewState {
    NoOp,
    Clean(MergePreviewTicket),
    Conflict {
        #[allow(dead_code)]
        conflicting_files: Vec<String>,
        #[allow(dead_code)]
        details: String,
    },
    /// `SDD-10` Fase 3: el delta `<root_now_commit>..<merged_tree>` toca el *Control Plane*
    /// (`.onyx/config.toml`, `.env*`, `.git/hooks/**`, ...). El merge se **niega** sin tocar
    /// el disco de `root`; el worktree se conserva para inspección o `subagent_discard`.
    ControlPlaneDenied {
        #[allow(dead_code)]
        denied_files: Vec<String>,
    },
}

/// Resultado de ejecutar `subagent_merge` sobre un job finalizado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentMergeReport {
    pub job_id: String,
    pub changed_files: Vec<String>,
    pub unified_diff: String,
    pub was_noop: bool,
    pub impact: Option<ImpactAssessment>,
    pub pre_merge_checkpoint: Option<(String, String)>,
}

struct SubagentCompletion {
    proposal: SubagentProposal,
    cancelled: bool,
}

struct ManagedJob {
    job_id: String,
    /// Backend de ejecución resuelto en `spawn_job` (`"native"` o `id` de driver externo).
    driver: String,
    name: String,
    goal: String,
    spawn_tick: u64,
    handle: WorktreeHandle,
    cancel_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    state: SubagentJobState,
    worktree_removed: bool,
}

impl ManagedJob {
    fn summary(&self) -> SubagentJobSummary {
        let (ok, touched_files, summary) = match self.state.proposal() {
            Some(p) => (Some(p.ok), p.touched_files.clone(), p.summary.clone()),
            None => (None, Vec::new(), String::new()),
        };
        SubagentJobSummary {
            job_id: self.job_id.clone(),
            driver: self.driver.clone(),
            name: self.name.clone(),
            goal: self.goal.clone(),
            spawn_tick: self.spawn_tick,
            branch_name: self.handle.branch_name.clone(),
            worktree_path: self.handle.worktree_path.clone(),
            status: self.state.as_str(),
            ok,
            touched_files,
            summary,
        }
    }

    /// Garantiza que el hilo del worker ha terminado completamente (y por tanto todos los
    /// `ProcessJob` RAII, subprocesos `shell_exec` y servicios Sprout se han liberado) antes
    /// de tocar o eliminar `handle.worktree_path`.
    fn join_worker(&mut self) {
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

/// Gestor de subagentes asíncronos con aislamiento en *worktrees* Git (`SDD-09` Fases 2-4,
/// `SDD-10` Fase 1: drivers de harness CLI externos como backend alternativo).
pub struct SubagentManager {
    worktree_mgr: Result<WorktreeManager, String>,
    root_policy: WorkspacePolicy,
    root_seed: u64,
    network_policy: SubagentNetworkPolicy,
    responder_factory: ResponderFactory,
    /// Drivers de harness CLI externos registrados (`SDD-10`): construidos desde
    /// `root_policy.external_drivers()` en [`Self::with_worktree_base_dir`].
    external_drivers: Vec<Arc<dyn ExternalHarnessDriver>>,
    jobs: BTreeMap<String, ManagedJob>,
    pending_merge_tickets: BTreeMap<String, MergePreviewTicket>,
    tx: mpsc::Sender<SubagentCompletion>,
    rx: mpsc::Receiver<SubagentCompletion>,
    next_seq: u64,
    max_concurrent: usize,
    ready_flag: Option<Arc<AtomicBool>>,
    root_tools: Option<WorkspaceTools>,
}

impl SubagentManager {
    /// Crea un gestor para el workspace indicado por `root_policy`.
    /// Si el workspace es un repositorio Git válido (`>= 2.38.0`), ejecuta `git worktree prune`
    /// al arrancar para limpiar metadatos huérfanos de sesiones anteriores (`SDD-09` §4.1.6).
    pub fn new(
        root_policy: WorkspacePolicy,
        root_seed: u64,
        responder_factory: ResponderFactory,
    ) -> Self {
        Self::with_worktree_base_dir(root_policy, root_seed, responder_factory, None)
    }

    /// Variante con override de directorio base de *worktrees* (útil en tests herméticos).
    pub fn with_worktree_base_dir(
        root_policy: WorkspacePolicy,
        root_seed: u64,
        responder_factory: ResponderFactory,
        worktree_base_dir: Option<&std::path::Path>,
    ) -> Self {
        let worktree_mgr =
            WorktreeManager::with_base_dir_override(&root_policy.root, worktree_base_dir);
        if let Ok(ref mgr) = worktree_mgr {
            let _ = mgr.prune_worktrees();
        }
        let (tx, rx) = mpsc::channel();
        let external_drivers: Vec<Arc<dyn ExternalHarnessDriver>> = root_policy
            .external_drivers()
            .iter()
            .map(|cfg| -> Arc<dyn ExternalHarnessDriver> {
                Arc::new(ExternalDriverRunner::new(cfg.clone()))
            })
            .collect();
        Self {
            worktree_mgr,
            root_policy,
            root_seed,
            network_policy: SubagentNetworkPolicy::NativeOffline,
            responder_factory,
            external_drivers,
            jobs: BTreeMap::new(),
            pending_merge_tickets: BTreeMap::new(),
            tx,
            rx,
            next_seq: 1,
            max_concurrent: DEFAULT_MAX_CONCURRENT_SUBAGENTS,
            ready_flag: None,
            root_tools: None,
        }
    }

    pub fn with_max_concurrent(mut self, max_concurrent: usize) -> Self {
        self.max_concurrent = max_concurrent.max(1);
        self
    }

    #[allow(dead_code)]
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    pub fn with_ready_flag(mut self, ready_flag: Arc<AtomicBool>) -> Self {
        self.ready_flag = Some(ready_flag);
        self
    }

    pub fn with_root_tools(mut self, tools: WorkspaceTools) -> Self {
        self.root_tools = Some(tools);
        self
    }

    #[allow(dead_code)]
    pub fn with_network_policy(mut self, policy: SubagentNetworkPolicy) -> Self {
        self.network_policy = policy;
        self
    }

    fn require_worktree_mgr(&self) -> Result<&WorktreeManager, String> {
        self.worktree_mgr.as_ref().map_err(|reason| {
            format!(
                "async subagents require a Git repository with Git >= 2.38.0 (`merge-tree --write-tree`): {reason}"
            )
        })
    }

    fn allocate_job_id(&mut self, requested_name: Option<&str>) -> String {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let prefix = requested_name
            .map(sanitize_job_prefix)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sub".to_string());
        let candidate = format!("{prefix}-{seq}");
        if validate_job_id(&candidate).is_ok() && !self.jobs.contains_key(&candidate) {
            candidate
        } else {
            format!("sub-{seq}")
        }
    }

    /// Lista los `id` de los drivers de harness CLI externos registrados (`SDD-10`),
    /// p. ej. `["opencode"]` (los presets integrados siempre están disponibles).
    #[allow(dead_code)]
    pub fn external_driver_ids(&self) -> Vec<String> {
        self.external_drivers
            .iter()
            .map(|d| d.id().to_string())
            .collect()
    }

    /// Lanza un subagente en un hilo aislado sobre un *worktree* Git nuevo y devuelve
    /// inmediatamente su resumen (`Running`) sin bloquear al agente raíz.
    /// Hace cumplir [`Self::max_concurrent`] deterministamente antes de crear worktrees o hilos.
    ///
    /// `driver` (`SDD-10` Fase 1): `None`/`"native"` → motor OnYX en proceso; `id` conocido
    /// (`[[external_drivers]]`) → harness CLI externo dentro del mismo worktree; `id`
    /// desconocido → error accionable con la lista de ids válidos.
    pub fn spawn_job(
        &mut self,
        goal: &str,
        name: Option<&str>,
        max_steps: Option<i64>,
        spawn_tick: u64,
        graph_snapshot: Vec<u8>,
        driver: Option<&str>,
    ) -> Result<SubagentJobSummary, String> {
        let goal_trimmed = goal.trim();
        if goal_trimmed.is_empty() {
            return Err("spawn_subagent requires a non-empty `goal`".to_string());
        }
        // Resolución + validación del driver ANTES de crear worktree (error accionable temprano).
        let resolved_external: Option<Arc<dyn ExternalHarnessDriver>> = match driver
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            Some(id) if !id.eq_ignore_ascii_case("native") => {
                // Defense in depth: el gate de trust ya decide en la aprobación; aquí se
                // garantiza que un workspace no confiado no ejecuta CLIs externos jamás.
                if !self.root_policy.trust.config_trusted {
                    return Err(format!(
                        "blocked: external harness driver `{id}` requires a trusted workspace (run `openonyx trust` or `/trust` to approve)"
                    ));
                }
                let entry = self
                    .external_drivers
                    .iter()
                    .find(|d| d.id() == id)
                    .ok_or_else(|| {
                        let available: Vec<&str> = std::iter::once("native")
                            .chain(self.external_drivers.iter().map(|d| d.id()))
                            .collect();
                        format!(
                            "unknown subagent driver `{id}`; available drivers: {}",
                            available.join(", ")
                        )
                    })?;
                Some(entry.clone())
            }
            _ => None,
        };
        let driver_id = resolved_external
            .as_ref()
            .map(|d| d.id().to_string())
            .unwrap_or_else(|| "native".to_string());
        let _ = self.poll_completions();
        let running = self
            .jobs
            .values()
            .filter(|j| matches!(j.state, SubagentJobState::Running))
            .count();
        if running >= self.max_concurrent {
            return Err(format!(
                "max concurrent subagents reached ({running}/{}); wait for an active subagent to finish or discard one with `subagent_discard`",
                self.max_concurrent
            ));
        }

        let mgr = self.require_worktree_mgr()?.clone();
        let job_id = self.allocate_job_id(name);
        let display_name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&job_id)
            .to_string();
        let handle = mgr.create_worktree(&job_id, spawn_tick)?;

        // `SDD-10` Fase 2: el backend se resuelve **antes** de `for_worktree` porque la política
        // de red del worktree depende del driver: un harness CLI externo deriva
        // `ExternalHarnessAllowlist` (no-`Offline`, sin hints `*_OFFLINE`), el nativo mantiene
        // `NativeOffline`.
        let sub_seed = derive_subagent_seed(self.root_seed, spawn_tick, &job_id);
        let worker_backend = match &resolved_external {
            Some(driver) => WorkerBackend::External {
                driver: driver.clone(),
            },
            None => WorkerBackend::Native {
                graph_snapshot,
                seed: sub_seed,
            },
        };
        let wt_network_policy = match &worker_backend {
            WorkerBackend::External { driver } => driver.config().network_policy.clone(),
            WorkerBackend::Native { .. } => self.network_policy.clone(),
        };
        let wt_policy = match self
            .root_policy
            .for_worktree(&handle.worktree_path, &wt_network_policy)
        {
            Ok(p) => p,
            Err(e) => {
                let _ = mgr.remove_worktree(&handle);
                return Err(format!(
                    "failed to derive worktree policy for `{job_id}`: {e}"
                ));
            }
        };
        let bounded_steps = max_steps
            .unwrap_or(DEFAULT_SUBAGENT_MAX_STEPS)
            .clamp(1, MAX_SUBAGENT_MAX_STEPS);
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel_flag);
        let worker_ready = self.ready_flag.clone();
        let worker_tx = self.tx.clone();
        let worker_factory = Arc::clone(&self.responder_factory);
        let worker_handle = handle.clone();
        let worker_goal = goal_trimmed.to_string();
        let worker_name = display_name.clone();
        let worker_job_id = job_id.clone();

        let thread_builder = thread::Builder::new().name(format!("onyx-sub-{job_id}"));
        let join_handle = thread_builder
            .spawn(move || {
                run_subagent_worker(SubagentWorkerInput {
                    job_id: worker_job_id,
                    name: worker_name,
                    goal: worker_goal,
                    spawn_tick,
                    max_steps: bounded_steps,
                    backend: worker_backend,
                    wt_policy,
                    handle: worker_handle,
                    worktree_mgr: mgr,
                    responder_factory: worker_factory,
                    cancel_flag: worker_cancel,
                    ready_flag: worker_ready,
                    tx: worker_tx,
                });
            })
            .map_err(|e| format!("failed to spawn subagent thread for `{job_id}`: {e}"))?;

        let managed = ManagedJob {
            job_id: job_id.clone(),
            driver: driver_id,
            name: display_name,
            goal: goal_trimmed.to_string(),
            spawn_tick,
            handle,
            cancel_flag,
            worker: Some(join_handle),
            state: SubagentJobState::Running,
            worktree_removed: false,
        };
        let summary = managed.summary();
        self.jobs.insert(job_id, managed);
        Ok(summary)
    }

    /// Drena todas las finalizaciones de subagentes recibidas hasta esta frontera de tick y las
    /// devuelve ordenadas canónicamente por `(spawn_tick, job_id)` (`SDD-09` §2.5), conservando
    /// el indicador de cancelación.
    pub fn poll_completions_with_flags(&mut self) -> Vec<(SubagentProposal, bool)> {
        let mut drained = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            drained.push(msg);
        }
        drained.sort_by(|a, b| {
            a.proposal
                .spawn_tick
                .cmp(&b.proposal.spawn_tick)
                .then_with(|| a.proposal.job_id.cmp(&b.proposal.job_id))
        });

        let mut ordered = Vec::with_capacity(drained.len());
        for item in drained {
            if let Some(job) = self.jobs.get_mut(&item.proposal.job_id) {
                job.join_worker();
                if matches!(job.state, SubagentJobState::Running) {
                    job.state = if item.cancelled {
                        SubagentJobState::Cancelled
                    } else if item.proposal.ok {
                        SubagentJobState::Done(item.proposal.clone())
                    } else {
                        SubagentJobState::Failed(item.proposal.clone())
                    };
                }
            }
            ordered.push((item.proposal, item.cancelled));
        }
        ordered
    }

    /// Drena todas las finalizaciones de subagentes recibidas hasta esta frontera de tick y las
    /// devuelve ordenadas canónicamente por `(spawn_tick, job_id)` (`SDD-09` §2.5).
    pub fn poll_completions(&mut self) -> Vec<SubagentProposal> {
        self.poll_completions_with_flags()
            .into_iter()
            .map(|(p, _)| p)
            .collect()
    }

    /// Consulta el estado de un subagente concreto o de todos los subagentes registrados.
    pub fn status(&mut self, job_id: Option<&str>) -> Result<Vec<SubagentJobSummary>, String> {
        let _ = self.poll_completions();
        if let Some(id) = job_id.map(str::trim).filter(|s| !s.is_empty()) {
            let job = self
                .jobs
                .get(id)
                .ok_or_else(|| format!("unknown subagent job_id `{id}`"))?;
            Ok(vec![job.summary()])
        } else {
            let mut list: Vec<SubagentJobSummary> =
                self.jobs.values().map(ManagedJob::summary).collect();
            list.sort_by(|a, b| {
                a.spawn_tick
                    .cmp(&b.spawn_tick)
                    .then_with(|| a.job_id.cmp(&b.job_id))
            });
            Ok(list)
        }
    }

    /// Prepara en memoria una vista previa 3-way de `subagent_merge`, calcula el calor AST real
    /// de `onyx-scent` sobre `<root_now_commit>..<merged_tree>` y sella un [`MergePreviewTicket`]
    /// para bloquear carreras **TOCTOU** antes de aprobar o materializar en disco.
    pub fn prepare_merge_preview(&mut self, job_id: &str) -> Result<MergePreviewState, String> {
        let _ = self.poll_completions();
        let mgr = self.require_worktree_mgr()?.clone();
        let id = job_id.trim();
        let proposal = {
            let job = self
                .jobs
                .get_mut(id)
                .ok_or_else(|| format!("unknown subagent job_id `{id}`"))?;
            let p = match &job.state {
                SubagentJobState::Running => {
                    return Err(format!(
                        "subagent `{id}` is still running; wait for completion (`subagent_status`) or discard it (`subagent_discard`)"
                    ));
                }
                SubagentJobState::Merged(_) => {
                    return Err(format!("subagent `{id}` was already merged"));
                }
                SubagentJobState::Discarded => {
                    return Err(format!("subagent `{id}` was already discarded"));
                }
                SubagentJobState::Cancelled => {
                    return Err(format!(
                        "subagent `{id}` was cancelled and cannot be merged"
                    ));
                }
                SubagentJobState::Done(p) | SubagentJobState::Failed(p) => p.clone(),
            };
            if p.sub_commit.is_empty() {
                return Err(format!(
                    "subagent `{id}` has no captured commit to merge (summary: {})",
                    p.summary
                ));
            }
            job.join_worker();
            p
        };

        if let Some(existing) = self.pending_merge_tickets.get(id) {
            if existing.sub_commit == proposal.sub_commit
                && mgr.current_working_tree_sha().ok().as_deref()
                    == Some(existing.root_now_tree.as_str())
            {
                return Ok(MergePreviewState::Clean(existing.clone()));
            }
        }

        let handle = self
            .jobs
            .get(id)
            .expect("job validated above")
            .handle
            .clone();
        match mgr.preview_three_way_merge(&handle, &proposal.sub_commit)? {
            WorktreeMergeOutcome::NoOp => {
                self.pending_merge_tickets.remove(id);
                Ok(MergePreviewState::NoOp)
            }
            WorktreeMergeOutcome::Clean {
                root_now_commit,
                root_now_tree,
                merged_tree,
                changed_files,
                unified_diff,
            } => {
                // `SDD-10` Fase 3: comprobación **obligatoria** de Control Plane sobre el
                // delta real `<root_now_commit>..<merged_tree>` ANTES de sellar el ticket.
                // `root_now_commit`/`merged_tree` solo se usan en `Deny` para el mensaje.
                if classify_merge_delta(&changed_files) == MergeDeltaVerdict::Deny {
                    let denied_files: Vec<String> = changed_files
                        .iter()
                        .filter(|f| is_control_plane_rel_path(f))
                        .cloned()
                        .collect();
                    self.pending_merge_tickets.remove(id);
                    return Ok(MergePreviewState::ControlPlaneDenied { denied_files });
                }
                let impact = self
                    .root_tools
                    .as_ref()
                    .and_then(|t| t.assess_unified_diff_impact(&unified_diff));
                let ticket = MergePreviewTicket {
                    job_id: id.to_string(),
                    approval_key: None,
                    root_now_commit,
                    root_now_tree,
                    sub_commit: proposal.sub_commit,
                    merged_tree,
                    changed_files,
                    unified_diff,
                    impact,
                };
                self.pending_merge_tickets
                    .insert(id.to_string(), ticket.clone());
                Ok(MergePreviewState::Clean(ticket))
            }
            WorktreeMergeOutcome::Conflict {
                conflicting_files,
                details,
                ..
            } => {
                self.pending_merge_tickets.remove(id);
                Ok(MergePreviewState::Conflict {
                    conflicting_files,
                    details,
                })
            }
        }
    }

    /// Vista previa 3-way ligada a una solicitud de aprobación concreta `(agent_id,
    /// request_id)`: si ya existe un [`MergePreviewTicket`] sellado para esa misma solicitud,
    /// lo devuelve tal cual **sin** lanzar `git` (ni `current_working_tree_sha`/`write-tree`
    /// ni `merge-tree`), de modo que los redibujados periódicos de `TuiDriver`/`WebDriver`
    /// (`pump_idle`/SSE) y el propio `SubagentManagerDriver` no sobrescriban `root_now_tree`
    /// en cada frame mientras el usuario decide. La guarda anti-TOCTOU de
    /// `apply_clean_merge_checked` sigue validando el árbol en el momento de aplicar
    /// (`SDD-10` Paso 0).
    pub fn preview_merge_for_request(
        &mut self,
        job_id: &str,
        agent_id: u64,
        request_id: u64,
    ) -> Result<MergePreviewState, String> {
        let key = (agent_id, request_id);
        let id = job_id.trim();
        if let Some(existing) = self.pending_merge_tickets.get(id) {
            if existing.approval_key == Some(key) {
                return Ok(MergePreviewState::Clean(existing.clone()));
            }
        }
        let mut state = self.prepare_merge_preview(job_id)?;
        if let MergePreviewState::Clean(ticket) = &mut state {
            ticket.approval_key = Some(key);
            self.pending_merge_tickets
                .insert(ticket.job_id.clone(), ticket.clone());
        }
        Ok(state)
    }

    /// Reconcilia en 3 vías el resultado de un subagente terminado sobre el *working tree* de
    /// `root` sin mover `HEAD`, validando la guarda **TOCTOU** (`root_now_tree`) y garantizando
    /// que el hilo worker y sus `ProcessJob` RAII hayan finalizado antes de invocar `remove_worktree`.
    pub fn merge_job(&mut self, job_id: &str) -> Result<SubagentMergeReport, String> {
        let _ = self.poll_completions();
        let mgr = self.require_worktree_mgr()?.clone();
        let id = job_id.trim();

        let (proposal, handle) = {
            let job = self
                .jobs
                .get_mut(id)
                .ok_or_else(|| format!("unknown subagent job_id `{id}`"))?;

            let proposal = match &job.state {
                SubagentJobState::Running => {
                    return Err(format!(
                        "subagent `{id}` is still running; wait for completion (`subagent_status`) or discard it (`subagent_discard`)"
                    ));
                }
                SubagentJobState::Merged(_) => {
                    return Err(format!("subagent `{id}` was already merged"));
                }
                SubagentJobState::Discarded => {
                    return Err(format!("subagent `{id}` was already discarded"));
                }
                SubagentJobState::Cancelled => {
                    return Err(format!(
                        "subagent `{id}` was cancelled and cannot be merged"
                    ));
                }
                SubagentJobState::Done(p) | SubagentJobState::Failed(p) => p.clone(),
            };

            if proposal.sub_commit.is_empty() {
                return Err(format!(
                    "subagent `{id}` has no captured commit to merge (summary: {})",
                    proposal.summary
                ));
            }

            // Asegura que el hilo worker haya cerrado todos los `ProcessJob` RAII y descriptores
            // antes de desmontar el worktree.
            job.join_worker();
            (proposal, job.handle.clone())
        };

        let cached_ticket = self
            .pending_merge_tickets
            .remove(id)
            .filter(|t| t.sub_commit == proposal.sub_commit);

        let ticket = if let Some(t) = cached_ticket {
            t
        } else {
            match mgr.preview_three_way_merge(&handle, &proposal.sub_commit)? {
                WorktreeMergeOutcome::NoOp => {
                    let job = self.jobs.get_mut(id).expect("job must exist");
                    if !job.worktree_removed {
                        mgr.remove_worktree(&job.handle)?;
                        job.worktree_removed = true;
                    }
                    job.state = SubagentJobState::Merged(proposal);
                    return Ok(SubagentMergeReport {
                        job_id: id.to_string(),
                        changed_files: Vec::new(),
                        unified_diff: String::new(),
                        was_noop: true,
                        impact: None,
                        pre_merge_checkpoint: None,
                    });
                }
                WorktreeMergeOutcome::Clean {
                    root_now_commit,
                    root_now_tree,
                    merged_tree,
                    changed_files,
                    unified_diff,
                } => {
                    let impact = self
                        .root_tools
                        .as_ref()
                        .and_then(|t| t.assess_unified_diff_impact(&unified_diff));
                    MergePreviewTicket {
                        job_id: id.to_string(),
                        approval_key: None,
                        root_now_commit,
                        root_now_tree,
                        sub_commit: proposal.sub_commit.clone(),
                        merged_tree,
                        changed_files,
                        unified_diff,
                        impact,
                    }
                }
                WorktreeMergeOutcome::Conflict {
                    conflicting_files,
                    details,
                    ..
                } => {
                    return Err(format!(
                        "3-way merge conflict for subagent `{id}` in {:?} (root working tree left untouched; worktree `{}` kept for inspection or `subagent_discard`):\n{}",
                        conflicting_files,
                        handle.worktree_path.display(),
                        details
                    ));
                }
            }
        };

        // `SDD-10` Fase 3: guarda **dura** de Control Plane antes de materializar en `root`
        // (cubre tanto el ticket en caché como el recién sellado). El disco de `root` y el
        // worktree quedan intactos: el job sigue `Done` y el worktree se conserva.
        if classify_merge_delta(&ticket.changed_files) == MergeDeltaVerdict::Deny {
            let denied_files: Vec<String> = ticket
                .changed_files
                .iter()
                .filter(|f| is_control_plane_rel_path(f))
                .cloned()
                .collect();
            return Err(format!(
                "blocked: subagent `{id}` merge delta touches Control Plane paths {denied_files:?}; merge denied without touching root disk (worktree `{}` kept for inspection or `subagent_discard`)",
                handle.worktree_path.display()
            ));
        }

        // Guarda anti-TOCTOU: si algo modificó el árbol de root desde el preview/aprobación,
        // aborta limpiamente conservando intactos tanto el disco de root como el worktree.
        let applied = mgr.apply_clean_merge_checked(
            &ticket.root_now_commit,
            &ticket.root_now_tree,
            &ticket.merged_tree,
        )?;
        if let Some(ref tools) = self.root_tools {
            tools.clear_seen_reads();
        }
        let job = self.jobs.get_mut(id).expect("job must exist");
        if !job.worktree_removed {
            mgr.remove_worktree(&job.handle)?;
            job.worktree_removed = true;
        }
        job.state = SubagentJobState::Merged(proposal);
        Ok(SubagentMergeReport {
            job_id: id.to_string(),
            changed_files: if applied.is_empty() {
                ticket.changed_files
            } else {
                applied
            },
            unified_diff: ticket.unified_diff,
            was_noop: false,
            impact: ticket.impact,
            pre_merge_checkpoint: Some((ticket.root_now_commit, ticket.root_now_tree)),
        })
    }

    /// Cancela (si sigue activo), espera a que los `ProcessJob` RAII liberen cualquier proceso
    /// hijo en el worktree (`join_worker`), y elimina el worktree + rama temporal `onyx/sub/<job_id>`.
    pub fn discard_job(&mut self, job_id: &str) -> Result<String, String> {
        let _ = self.poll_completions();
        let mgr = self.require_worktree_mgr()?.clone();
        let id = job_id.trim();
        self.pending_merge_tickets.remove(id);
        let job = self
            .jobs
            .get_mut(id)
            .ok_or_else(|| format!("unknown subagent job_id `{id}`"))?;

        // 1. Señalar cancelación para abortar ticks y `shell_exec` en vuelo (dispara `ProcessJob::terminate`).
        job.cancel_flag.store(true, Ordering::Release);
        // 2. Unir el hilo worker ANTES de invocar `remove_worktree` (garantiza 0 handles vivos en Windows).
        job.join_worker();
        // 3. Desmontar worktree y borrar rama temporal `onyx/sub/<job_id>`.
        if !job.worktree_removed {
            mgr.remove_worktree(&job.handle)?;
            job.worktree_removed = true;
        }
        job.state = SubagentJobState::Discarded;
        Ok(format!(
            "discarded subagent `{id}` (removed worktree `{}` and branch `{}`)",
            job.handle.worktree_path.display(),
            job.handle.branch_name
        ))
    }

    /// Número de subagentes actualmente en ejecución en segundo plano.
    #[allow(dead_code)]
    pub fn active_count(&mut self) -> usize {
        let _ = self.poll_completions();
        self.jobs
            .values()
            .filter(|j| matches!(j.state, SubagentJobState::Running))
            .count()
    }
}

/// Handle compartible y thread-safe sobre [`SubagentManager`] para UI (TUI/Web), comandos slash
/// y el driver determinista [`SubagentManagerDriver`].
#[derive(Clone)]
pub struct SubagentHandle(Arc<Mutex<SubagentManager>>);

impl From<SubagentManager> for SubagentHandle {
    fn from(manager: SubagentManager) -> Self {
        Self::new(manager)
    }
}

impl SubagentHandle {
    pub fn new(manager: SubagentManager) -> Self {
        Self(Arc::new(Mutex::new(manager)))
    }

    pub fn spawn_job(
        &self,
        goal: &str,
        name: Option<&str>,
        max_steps: Option<i64>,
        spawn_tick: u64,
        graph_snapshot: Vec<u8>,
        driver: Option<&str>,
    ) -> Result<SubagentJobSummary, String> {
        let mut mgr = self
            .0
            .lock()
            .map_err(|_| "subagent manager lock poisoned".to_string())?;
        mgr.spawn_job(goal, name, max_steps, spawn_tick, graph_snapshot, driver)
    }

    pub fn status(&self, job_id: Option<&str>) -> Result<Vec<SubagentJobSummary>, String> {
        let mut mgr = self
            .0
            .lock()
            .map_err(|_| "subagent manager lock poisoned".to_string())?;
        mgr.status(job_id)
    }

    pub fn list_summaries(&self) -> Vec<SubagentJobSummary> {
        self.0
            .lock()
            .ok()
            .and_then(|mut mgr| mgr.status(None).ok())
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn active_count(&self) -> usize {
        self.0
            .lock()
            .ok()
            .map(|mut mgr| mgr.active_count())
            .unwrap_or(0)
    }

    pub fn poll_completions_with_flags(&self) -> Vec<(SubagentProposal, bool)> {
        self.0
            .lock()
            .ok()
            .map(|mut mgr| mgr.poll_completions_with_flags())
            .unwrap_or_default()
    }

    /// Vista previa 3-way para la solicitud de aprobación `(agent_id, request_id)`:
    /// reutiliza el [`MergePreviewTicket`] sellado para esa misma solicitud sin relanzar
    /// `git write-tree`/`merge-tree` en cada redibujado (`SDD-10` Paso 0).
    pub fn preview_merge_for_request(
        &self,
        job_id: &str,
        agent_id: u64,
        request_id: u64,
    ) -> Result<MergePreviewState, String> {
        let mut mgr = self
            .0
            .lock()
            .map_err(|_| "subagent manager lock poisoned".to_string())?;
        mgr.preview_merge_for_request(job_id, agent_id, request_id)
    }

    pub fn merge_job(&self, job_id: &str) -> Result<SubagentMergeReport, String> {
        let mut mgr = self
            .0
            .lock()
            .map_err(|_| "subagent manager lock poisoned".to_string())?;
        mgr.merge_job(job_id)
    }

    pub fn discard_job(&self, job_id: &str) -> Result<String, String> {
        let mut mgr = self
            .0
            .lock()
            .map_err(|_| "subagent manager lock poisoned".to_string())?;
        mgr.discard_job(job_id)
    }
}

impl Drop for SubagentManager {
    fn drop(&mut self) {
        // 1. Señalar cancelación a todos los workers activos para abortar `ProcessJob` en paralelo.
        for job in self.jobs.values() {
            job.cancel_flag.store(true, Ordering::Release);
        }
        // 2. Unir todos los hilos ANTES de eliminar worktrees para liberar todos los `ProcessJob` RAII.
        for job in self.jobs.values_mut() {
            job.join_worker();
        }
        // 3. Limpiar cualquier worktree y rama temporal que no hubiese sido desmontada todavía.
        if let Ok(ref mgr) = self.worktree_mgr {
            for job in self.jobs.values_mut() {
                if !job.worktree_removed {
                    let _ = mgr.remove_worktree(&job.handle);
                    job.worktree_removed = true;
                }
            }
        }
    }
}

struct SubagentWorkerInput {
    job_id: String,
    name: String,
    goal: String,
    spawn_tick: u64,
    max_steps: i64,
    /// Backend de ejecución (`SDD-10`): solo [`WorkerBackend::Native`] usa
    /// `max_steps`/`responder_factory`; [`WorkerBackend::External`] ejecuta el harness CLI.
    backend: WorkerBackend,
    wt_policy: WorkspacePolicy,
    handle: WorktreeHandle,
    worktree_mgr: WorktreeManager,
    responder_factory: ResponderFactory,
    cancel_flag: Arc<AtomicBool>,
    ready_flag: Option<Arc<AtomicBool>>,
    tx: mpsc::Sender<SubagentCompletion>,
}

fn run_subagent_worker(input: SubagentWorkerInput) {
    let SubagentWorkerInput {
        job_id,
        name,
        goal,
        spawn_tick,
        max_steps,
        backend,
        wt_policy,
        handle,
        worktree_mgr,
        responder_factory,
        cancel_flag,
        ready_flag,
        tx,
    } = input;

    // `driver` de la propuesta: `"native"` o el `id` del harness CLI externo (`SDD-10`).
    let driver_id = match &backend {
        WorkerBackend::Native { .. } => "native".to_string(),
        WorkerBackend::External { driver } => driver.id().to_string(),
    };

    let mut ok = false;
    let mut summary;
    let mut cancelled = cancel_flag.load(Ordering::Acquire);

    match backend {
        WorkerBackend::Native {
            graph_snapshot,
            seed: sub_seed,
        } => {
            if !cancelled {
                let wt_tools =
                    WorkspaceTools::new(wt_policy).with_cancel_flag(Arc::clone(&cancel_flag));
                let sub_config = AgentConfig {
                    default_max_steps: max_steps,
                    default_timeout_ticks: max_steps.saturating_mul(4).max(32),
                    ..AgentConfig::default()
                };

                let mut store = MemStore::new();
                let has_snapshot =
                    !graph_snapshot.is_empty() && store.save("init", &graph_snapshot).is_ok();

                let host_res = AgentHost::builder(sub_seed)
                    .config(sub_config)
                    .system(Box::new(WorkspaceLedgerSystem::new()))
                    .store(Box::new(store))
                    .build();

                match host_res {
                    Ok(mut sub_host) => {
                        let sub_ws_vocab = WorkspaceVocab::resolve_with(|attr| sub_host.attr(attr));
                        if has_snapshot {
                            sub_host.request_restore("init");
                            sub_host.step(&mut HeadlessDriver::new());
                            sub_host.set_tick(spawn_tick);
                        }

                        // Subagente nativo: sin `spawn_agent`/`spawn_subagent` recursivos y con
                        // auto-aprobación confinada al worktree desechable (las reglas `deny` y la
                        // protección del Control Plane siguen activas en `WorkspaceToolBridge`).
                        let sub_spec = AgentSpec::new(&name, &goal)
                            .with_tools(WorkspaceTools::all_tool_names().iter().copied());
                        sub_host.spawn(&sub_spec);

                        let ws_bridge =
                            WorkspaceToolBridge::new(sub_host.vocab(), wt_tools.clone())
                                .with_mode(WorkspaceMode::Build)
                                .with_workspace_vocab(sub_ws_vocab)
                                .require_read_before_edit(true)
                                .wrapping(HeadlessDriver::new());
                        let responder = (responder_factory)();
                        let mut driver =
                            RecordingBridge::new(sub_host.vocab(), responder).wrapping(ws_bridge);

                        let max_ticks =
                            usize::try_from(max_steps.saturating_mul(4).max(16)).unwrap_or(256);
                        for _ in 0..max_ticks {
                            if cancel_flag.load(Ordering::Acquire) {
                                cancelled = true;
                                break;
                            }
                            sub_host.step(&mut driver);
                            if cancel_flag.load(Ordering::Acquire) {
                                cancelled = true;
                                break;
                            }
                            if !sub_host.agents().is_empty() && sub_host.is_settled() {
                                break;
                            }
                        }

                        if let Some(agent_id) = sub_host.agents().last().copied() {
                            let final_state = sub_host.agent_state(agent_id);
                            ok = !cancelled && final_state == Some(AgentState::Done);
                            summary = sub_host.result(agent_id).unwrap_or_else(|| {
                                if cancelled {
                                    "cancelled by parent".to_string()
                                } else {
                                    format!("subagent ended in state {final_state:?}")
                                }
                            });
                        } else {
                            summary = if cancelled {
                                "cancelled before spawn".to_string()
                            } else {
                                "subagent did not produce an agent entity".to_string()
                            };
                        }

                        // Teardown explícito RAII ANTES de capturar commit o salir del hilo:
                        // 1. Detiene servicios Sprout (suelta sus `ProcessJob`).
                        // 2. Suelta `driver`, `sub_host` y `wt_tools` (libera handles en
                        //    `worktree_path`).
                        let _ = wt_tools.sprout().stop(None, true);
                        drop(driver);
                        drop(sub_host);
                        drop(wt_tools);
                    }
                    Err(e) => {
                        let _ = wt_tools.sprout().stop(None, true);
                        drop(wt_tools);
                        summary = format!("failed to initialize isolated subagent host: {e}");
                    }
                }
            } else {
                summary = "cancelled before execution".to_string();
            }
        }
        WorkerBackend::External { driver } => {
            // Harness CLI externo (`SDD-10`): se lanza dentro del mismo worktree Git aislado;
            // el disco es la verdad y la captura `git commit-tree` compartida al final recoge
            // lo que el harness haya dejado (o no).
            if !cancelled {
                match driver.run_in_worktree(&handle.worktree_path, &goal, &cancel_flag) {
                    Ok(mut out) => {
                        cancelled = out.cancelled;
                        ok = !out.cancelled && !out.timed_out && out.exit_code == 0;
                        // `SDD-10` Fase 3: el log crudo pasa por `scrub_secrets` y se sella en el
                        // **vault** del workspace root (no en el worktree):
                        // `.onyx/vault/subagent-logs/<job_id>.log`, tras `ensure_vault_gitignore`.
                        // El stdout/stderr es **log**, nunca verdad: la propuesta solo se construye
                        // de `capture_subagent_commit` + `changed_files_between` (abajo).
                        let mut log_note: Option<String> = None;
                        {
                            let workspace_root = worktree_mgr.workspace_root();
                            let path = crate::vault::vault_subagent_logs_dir(workspace_root)
                                .join(format!("{job_id}.log"));
                            let raw_log = format!(
                                "driver: {driver_id}\nexit_code: {exit_code}\ntimed_out: {timed_out}\ncancelled: {cancelled}\n\n--- stdout ---\n{stdout}\n\n--- stderr ---\n{stderr}\n",
                                exit_code = out.exit_code,
                                timed_out = out.timed_out,
                                cancelled = out.cancelled,
                                stdout = out.stdout,
                                stderr = out.stderr
                            );
                            let scrubbed = scrub_secrets(&raw_log);
                            match crate::vault::ensure_vault_gitignore(workspace_root).and_then(
                                |_| {
                                    if let Some(parent) = path.parent() {
                                        std::fs::create_dir_all(parent)?;
                                    }
                                    std::fs::write(&path, scrubbed)
                                },
                            ) {
                                Ok(()) => out.full_log_path = Some(path),
                                // Best-effort: un fallo de escritura no fracasa el job (la verdad
                                // vive en Git); se deja constancia en el `summary` de la propuesta.
                                Err(e) => log_note = Some(format!("vault log write failed: {e}")),
                            }
                        }
                        let mut base = if out.cancelled {
                            "cancelled by parent".to_string()
                        } else if out.timed_out {
                            format!(
                                "external driver `{}` killed after timeout {:?}",
                                driver.id(),
                                driver.config().timeout
                            )
                        } else if out.exit_code == 0 {
                            format!("external driver `{}` completed (exit 0)", driver.id())
                        } else {
                            format!(
                                "external driver `{}` exited with code {}",
                                driver.id(),
                                out.exit_code
                            )
                        };
                        if !out.clean_summary.is_empty() {
                            base.push_str("\n--- output ---\n");
                            base.push_str(&out.clean_summary);
                        }
                        if let Some(note) = log_note {
                            base.push_str(&format!(" [{note}]"));
                        }
                        summary = base;
                    }
                    Err(e) => {
                        summary = e;
                    }
                }
            } else {
                summary = "cancelled before execution".to_string();
            }
        }
    }

    let (sub_commit, touched_files) = if cancelled {
        (String::new(), Vec::new())
    } else {
        match worktree_mgr.capture_subagent_commit(&handle) {
            Ok((commit, _tree)) => {
                let files = worktree_mgr
                    .changed_files_between(&handle.base_commit, &commit)
                    .unwrap_or_default();
                (commit, files)
            }
            Err(err) => {
                ok = false;
                if summary.is_empty() {
                    summary = format!("failed to capture subagent commit: {err}");
                } else {
                    summary.push_str(&format!(" [commit capture error: {err}]"));
                }
                (String::new(), Vec::new())
            }
        }
    };

    let proposal = SubagentProposal {
        job_id,
        driver: driver_id,
        spawn_tick,
        branch_name: handle.branch_name,
        base_commit: handle.base_commit,
        sub_commit,
        ok,
        summary,
        touched_files,
    };

    let _ = tx.send(SubagentCompletion {
        proposal,
        cancelled,
    });
    if let Some(flag) = ready_flag {
        flag.store(true, Ordering::Release);
    }
}

fn sanitize_job_prefix(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    let mut out: String = trimmed.chars().take(24).collect();
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[derive(Deserialize)]
struct SpawnSubagentArgs {
    goal: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    max_steps: Option<i64>,
    /// Backend de ejecución (`SDD-10`): omitido/`"native"` → motor OnYX; `id` de un
    /// `[[external_drivers]]` → harness CLI externo en el mismo worktree.
    #[serde(default)]
    driver: Option<String>,
}

#[derive(Deserialize, Default)]
struct SubagentStatusArgs {
    #[serde(default)]
    job_id: Option<String>,
}

#[derive(Deserialize)]
struct SubagentJobIdArgs {
    job_id: String,
}

/// Capa `PlatformDriver` que intercepta `spawn_subagent`, `subagent_status`, `subagent_merge` y
/// `subagent_discard`, aplica *Scent-Heated Approval* sobre el diff 3-way de `subagent_merge` e
/// inyecta proactivamente propuestas completadas en frontera de tick para despertar `root` y
/// grabarlas en `ONYXTR2`.
///
/// Jamás se instancia en `openonyx replay`: la reproducción offline consume únicamente la
/// transcripción `ONYXTR2` con 0 worktrees, 0 hilos y 0 escrituras en disco.
pub struct SubagentManagerDriver {
    vocab: Vocab,
    handle: SubagentHandle,
    inner: Option<Box<dyn PlatformDriver>>,
    dispatched: BTreeSet<(u64, u64)>,
    handled_approvals: BTreeSet<(u64, u64)>,
    queued: Vec<Reply>,
    ready_flag: Option<Arc<AtomicBool>>,
    policy: Option<WorkspacePolicy>,
    mode_handle: WorkspaceModeHandle,
    proactive_notifications: bool,
    current_poll: u64,
    last_completed_proposals: Vec<SubagentProposal>,
}

impl SubagentManagerDriver {
    pub fn new(vocab: Vocab, manager: impl Into<SubagentHandle>) -> Self {
        Self {
            vocab,
            handle: manager.into(),
            inner: None,
            dispatched: BTreeSet::new(),
            handled_approvals: BTreeSet::new(),
            queued: Vec::new(),
            ready_flag: None,
            policy: None,
            mode_handle: WorkspaceModeHandle::new(WorkspaceMode::Build),
            proactive_notifications: false,
            current_poll: 0,
            last_completed_proposals: Vec::new(),
        }
    }

    pub fn with_ready_flag(mut self, ready_flag: Arc<AtomicBool>) -> Self {
        self.ready_flag = Some(ready_flag);
        self
    }

    pub fn with_policy(mut self, policy: WorkspacePolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn with_mode_handle(mut self, mode_handle: WorkspaceModeHandle) -> Self {
        self.mode_handle = mode_handle;
        self
    }

    /// Habilita el envío automático de [`SubagentProposal`] como `IngressEvent::Command` en
    /// frontera de tick para despertar `root` sin polling (`chat`/`serve`). Desactivado en `run`.
    pub fn with_proactive_notifications(mut self, enabled: bool) -> Self {
        self.proactive_notifications = enabled;
        self
    }

    pub fn wrapping(mut self, inner: impl PlatformDriver + 'static) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

    #[allow(dead_code)]
    pub fn handle(&self) -> SubagentHandle {
        self.handle.clone()
    }

    #[allow(dead_code)]
    pub fn last_completed_proposals(&self) -> &[SubagentProposal] {
        &self.last_completed_proposals
    }

    fn push_reply(&mut self, reply: Reply) {
        self.queued.push(reply);
        if let Some(flag) = &self.ready_flag {
            flag.store(true, Ordering::Release);
        }
    }

    fn execute_tool(
        &mut self,
        graph: &dyn GraphView,
        tool: &str,
        args_json: &str,
    ) -> Result<String, String> {
        let raw = if args_json.trim().is_empty() {
            "{}"
        } else {
            args_json
        };
        match tool {
            "spawn_subagent" => {
                let args: SpawnSubagentArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid spawn_subagent args: {e}"))?;
                let snapshot = snapshot_context_graph(graph, &self.vocab);
                let summary = self.handle.spawn_job(
                    &args.goal,
                    args.name.as_deref(),
                    args.max_steps,
                    self.current_poll,
                    snapshot,
                    args.driver.as_deref(),
                )?;
                Ok(json!({
                    "job_id": summary.job_id,
                    "driver": summary.driver,
                    "name": summary.name,
                    "status": summary.status,
                    "spawn_tick": summary.spawn_tick,
                    "branch": summary.branch_name,
                    "worktree_path": summary.worktree_path.to_string_lossy(),
                    "note": "Subagent running asynchronously in isolated Git worktree. Root agent is not blocked and will be notified on completion (or check via `subagent_status`)."
                })
                .to_string())
            }
            "subagent_status" => {
                let args: SubagentStatusArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid subagent_status args: {e}"))?;
                let summaries = self.handle.status(args.job_id.as_deref())?;
                let payload: Vec<serde_json::Value> = summaries
                    .into_iter()
                    .map(|s| {
                        json!({
                            "job_id": s.job_id,
                            "driver": s.driver,
                            "name": s.name,
                            "status": s.status,
                            "ok": s.ok,
                            "spawn_tick": s.spawn_tick,
                            "branch": s.branch_name,
                            "touched_files": s.touched_files,
                            "summary": s.summary,
                        })
                    })
                    .collect();
                Ok(json!({ "jobs": payload }).to_string())
            }
            "subagent_merge" => {
                let args: SubagentJobIdArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid subagent_merge args: {e}"))?;
                let report = self.handle.merge_job(&args.job_id)?;
                let diff_preview: String = report.unified_diff.chars().take(4096).collect();
                let impact_line = report
                    .impact
                    .as_ref()
                    .map(|i| i.summary_line.as_str())
                    .unwrap_or("");
                let epicenters: Vec<String> = report
                    .impact
                    .as_ref()
                    .map(|i| i.epicenters.clone())
                    .unwrap_or_default();
                let (ckpt_head, ckpt_tree) = report.pre_merge_checkpoint.unwrap_or_default();
                Ok(json!({
                    "job_id": report.job_id,
                    "merged": true,
                    "noop": report.was_noop,
                    "changed_files": report.changed_files,
                    "impact": impact_line,
                    "epicenters": epicenters,
                    "checkpoint_head": ckpt_head,
                    "checkpoint_tree": ckpt_tree,
                    "diff_preview": diff_preview,
                })
                .to_string())
            }
            "subagent_discard" => {
                let args: SubagentJobIdArgs = serde_json::from_str(raw)
                    .map_err(|e| format!("invalid subagent_discard args: {e}"))?;
                let msg = self.handle.discard_job(&args.job_id)?;
                Ok(json!({
                    "job_id": args.job_id,
                    "discarded": true,
                    "message": msg,
                })
                .to_string())
            }
            other => Err(format!("unsupported async subagent tool `{other}`")),
        }
    }
}

impl PlatformDriver for SubagentManagerDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        // Primero `inner.poll(bus)` (donde `WorkspaceToolBridge` limpia `ready_flag` antes de que
        // nosotros drenemos `mpsc`), manteniendo invariante libre de carreras con los workers.
        if let Some(inner) = self.inner.as_mut() {
            inner.poll(bus);
        }
        let completed = self.handle.poll_completions_with_flags();
        if !completed.is_empty() {
            self.last_completed_proposals = completed.iter().map(|(p, _)| p.clone()).collect();
            if self.proactive_notifications {
                for (proposal, cancelled) in completed {
                    bus.push(OnyxEvent::Ingress(IngressEvent::Command(
                        encode_proposal_command(&proposal, cancelled),
                    )));
                }
            }
        }
        for reply in self.queued.drain(..) {
            bus.push(reply.to_event());
        }
        self.current_poll = self.current_poll.saturating_add(1);
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        // Evalúa `pending_approvals` para `subagent_merge` con *Scent-Heated Approval* sobre el
        // diff 3-way real antes de delegar en la UI/CLI interna, evitando banners innecesarios si
        // está por debajo de umbral (cuando la política lo permite) o si hay conflicto directo.
        for view in pending_approvals(graph, &self.vocab) {
            let Some(pending) = &view.pending_approval else {
                continue;
            };
            if pending.tool != "subagent_merge" {
                continue;
            }
            let current_mode = self.mode_handle.get();
            let static_decision = self.policy.as_ref().map_or(PermissionDecision::Ask, |p| {
                p.evaluate_permission_in_mode(current_mode, "subagent_merge", &pending.args)
            });
            if static_decision == PermissionDecision::Deny {
                if self.handled_approvals.insert((view.agent.0, view.request)) {
                    self.push_reply(Reply::deny(
                        view.agent,
                        view.request,
                        "blocked by permission policy or read-only workspace mode: `subagent_merge`",
                    ));
                }
                continue;
            }

            let raw = if pending.args.trim().is_empty() {
                "{}"
            } else {
                pending.args.as_str()
            };
            let parsed_job = serde_json::from_str::<SubagentJobIdArgs>(raw);
            let decision = match parsed_job {
                Ok(args) => {
                    // Preview ligado a la solicitud `(agent_id, request_id)`: los ticks
                    // posteriores mientras la aprobación sigue pendiente reutilizan el
                    // ticket sellado sin relanzar `git write-tree` (`SDD-10` Paso 0).
                    let preview = self.handle.preview_merge_for_request(
                        &args.job_id,
                        view.agent.0,
                        view.request,
                    );
                    // `SDD-10` Fase 3: el delta toca el *Control Plane* → se niega la
                    // aprobación de plano (la guarda dura de `merge_job` queda como red de
                    // seguridad); el disco de `root` no se toca y el worktree se conserva.
                    if let Ok(MergePreviewState::ControlPlaneDenied { denied_files }) = &preview {
                        if self.handled_approvals.insert((view.agent.0, view.request)) {
                            self.push_reply(Reply::deny(
                                view.agent,
                                view.request,
                                format!(
                                    "Control Plane protection: subagent `{}` merge would touch {denied_files:?}; denied without touching root disk (worktree kept for inspection or `subagent_discard`)",
                                    args.job_id
                                ),
                            ));
                        }
                        continue;
                    }
                    match preview {
                        // Gobernanza → `Ask` obligatorio; resto → policy + escalado por impacto.
                        Ok(MergePreviewState::Clean(ticket)) => {
                            effective_merge_decision(static_decision, &ticket)
                        }
                        Ok(MergePreviewState::NoOp) => static_decision,
                        // No alcanzable: `ControlPlaneDenied` se niega arriba (el `if let` no
                        // estrecha el tipo para el exhaustiveness del match).
                        Ok(MergePreviewState::ControlPlaneDenied { .. }) => PermissionDecision::Ask,
                        // Conflictos o errores de estado se aprueban para que `subagent_merge` los
                        // ejecute enseguida y devuelva la ruta exacta en conflicto a `root` (conservando worktree).
                        Ok(MergePreviewState::Conflict { .. }) | Err(_) => {
                            PermissionDecision::Allow
                        }
                    }
                }
                Err(_) => PermissionDecision::Allow,
            };

            if decision == PermissionDecision::Allow
                && self.handled_approvals.insert((view.agent.0, view.request))
            {
                self.push_reply(Reply::approve(view.agent, view.request));
            }
        }

        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
        for (agent, request, pending) in pending_tool_calls(graph, &self.vocab) {
            if !is_async_subagent_tool(&pending.tool) {
                continue;
            }
            if !self.dispatched.insert((agent.0, request)) {
                continue;
            }
            let outcome = self.execute_tool(graph, &pending.tool, &pending.args);
            let mut reply = match outcome {
                Ok(out) => Reply::observe(agent, request, out),
                Err(err) => Reply::tool_error(agent, request, err),
            };
            if let Some(cid) = pending.call_id {
                reply = reply.with_call_id(cid);
            }
            self.push_reply(reply);
        }
    }
}

/// Formatea una línea humana compacta con la descripción del impacto cuando está presente.
#[allow(dead_code)]
pub fn format_merge_preview_banner(ticket: &MergePreviewTicket) -> Option<String> {
    ticket.impact.as_ref().map(|i| i.summary_line.clone())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use onyx_soul_agents::{AgentView, ReplayBridge, Transcript};
    use onyx_workspace::SandboxMode;

    use super::*;
    use crate::session::{build_host, make_spec};

    fn init_temp_git_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "openonyx_subagent_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let run_git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@local")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@local")
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        run_git(&["init", "-q"]);
        fs::write(dir.join("root.txt"), "root-v1\n").unwrap();
        fs::write(dir.join("sub.txt"), "sub-v1\n").unwrap();
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "initial"]);
        dir
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

    fn git_branch_exists(dir: &Path, branch: &str) -> bool {
        Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn derive_subagent_seed_is_deterministic_and_unique() {
        let s1 = derive_subagent_seed(42, 10, "worker-1");
        let s2 = derive_subagent_seed(42, 10, "worker-1");
        let s3 = derive_subagent_seed(42, 11, "worker-1");
        let s4 = derive_subagent_seed(42, 10, "worker-2");
        assert_eq!(s1, s2);
        assert_ne!(s1, s3);
        assert_ne!(s1, s4);
    }

    #[test]
    fn async_subagent_spawn_does_not_block_root_and_merges_cleanly_without_moving_head() {
        let repo = init_temp_git_repo("clean_async");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_async_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);
        let head_before = git_head_sha(&repo);

        // Uncommitted root change BEFORE spawn must be visible in the subagent worktree.
        fs::write(repo.join("root.txt"), "root-dirty-before-spawn\n").unwrap();

        let policy = WorkspacePolicy::new(&repo).unwrap();
        let tools = WorkspaceTools::new(policy.clone());

        // Scripted subagent responder: reads `root.txt`, edits `sub.txt`, finishes.
        let sub_factory: ResponderFactory = Arc::new(|| {
            let mut step = 0usize;
            Box::new(move |req: &AgentView| {
                step += 1;
                match step {
                    1 => Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_read",
                        r#"{"path":"root.txt"}"#,
                    )),
                    2 => Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"sub.txt","content":"sub-updated-async\n"}"#,
                    )),
                    _ => Some(Reply::done(
                        req.agent,
                        req.request,
                        "updated sub.txt cleanly",
                    )),
                }
            })
        });

        let mut mgr =
            SubagentManager::with_worktree_base_dir(policy, 7, sub_factory, Some(&wt_base))
                .with_root_tools(tools);

        let spawned = mgr
            .spawn_job(
                "update sub.txt",
                Some("refactor"),
                Some(16),
                5,
                Vec::new(),
                None,
            )
            .unwrap();
        assert_eq!(spawned.status, "running");
        assert_eq!(spawned.job_id, "refactor-1");
        assert!(spawned.worktree_path.is_dir());

        // Root continues editing `root.txt` concurrently without being blocked!
        fs::write(repo.join("root.txt"), "root-concurrent-edit\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let statuses = mgr.status(Some(&spawned.job_id)).unwrap();
            if statuses[0].status == "done" {
                assert_eq!(statuses[0].ok, Some(true));
                assert!(
                    statuses[0].touched_files.contains(&"sub.txt".to_string()),
                    "{:?}",
                    statuses[0].touched_files
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for subagent: {:?}",
                statuses[0]
            );
            thread::sleep(Duration::from_millis(25));
        }

        // Before merge, root's `sub.txt` is still untouched.
        assert_eq!(
            fs::read_to_string(repo.join("sub.txt")).unwrap(),
            "sub-v1\n"
        );

        let report = mgr.merge_job(&spawned.job_id).unwrap();
        assert!(!report.was_noop);
        assert!(report.changed_files.contains(&"sub.txt".to_string()));
        assert_eq!(
            fs::read_to_string(repo.join("root.txt")).unwrap(),
            "root-concurrent-edit\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("sub.txt")).unwrap(),
            "sub-updated-async\n"
        );
        assert_eq!(git_head_sha(&repo), head_before, "HEAD must never move");
        assert!(!spawned.worktree_path.exists());
        assert!(!git_branch_exists(&repo, &spawned.branch_name));

        drop(mgr);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn async_subagent_discard_terminates_running_shell_process_job_before_removing_worktree() {
        let repo = init_temp_git_repo("discard_proc");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_proc_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let mut policy = WorkspacePolicy::new(&repo).unwrap();
        policy.sandbox.mode = SandboxMode::WorkspaceWrite;
        policy.command_timeout = Duration::from_secs(30);

        let started_shell = Arc::new(AtomicBool::new(false));
        let started_clone = Arc::clone(&started_shell);

        let sleep_cmd = if cfg!(windows) {
            r#"{"command":"Start-Sleep -Seconds 20"}"#
        } else {
            r#"{"command":"sleep 20"}"#
        };

        let sub_factory: ResponderFactory = Arc::new(move || {
            let started = Arc::clone(&started_clone);
            let mut step = 0usize;
            Box::new(move |req: &AgentView| {
                step += 1;
                if step == 1 {
                    started.store(true, Ordering::Release);
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "shell_exec",
                        sleep_cmd,
                    ))
                } else {
                    Some(Reply::done(req.agent, req.request, "done"))
                }
            })
        });

        let mut mgr =
            SubagentManager::with_worktree_base_dir(policy, 99, sub_factory, Some(&wt_base));

        let spawned = mgr
            .spawn_job(
                "run long shell",
                Some("sleeper"),
                Some(8),
                3,
                Vec::new(),
                None,
            )
            .unwrap();

        // Wait until the subagent has actually dispatched `shell_exec` inside the worktree.
        let wait_start = Instant::now() + Duration::from_secs(10);
        while !started_shell.load(Ordering::Acquire) {
            assert!(
                Instant::now() < wait_start,
                "subagent never invoked shell_exec"
            );
            thread::sleep(Duration::from_millis(15));
        }
        // Give the shell process a brief moment to spawn and lock the worktree CWD on Windows.
        thread::sleep(Duration::from_millis(250));

        let t0 = Instant::now();
        let msg = mgr.discard_job(&spawned.job_id).unwrap();
        let elapsed = t0.elapsed();
        assert!(msg.contains("discarded"), "{msg}");
        assert!(
            elapsed < Duration::from_secs(10),
            "discard must terminate ProcessJob promptly instead of waiting 20s (took {elapsed:?})"
        );
        assert!(
            !spawned.worktree_path.exists(),
            "worktree directory must be removed cleanly after ProcessJob teardown"
        );
        assert!(
            !git_branch_exists(&repo, &spawned.branch_name),
            "temporary branch `{}` must be deleted on discard",
            spawned.branch_name
        );

        drop(mgr);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn async_subagent_completions_drain_in_canonical_spawn_tick_job_id_order() {
        let repo = init_temp_git_repo("canonical_order");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_order_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let policy = WorkspacePolicy::new(&repo).unwrap();
        let sub_factory: ResponderFactory = Arc::new(|| {
            Box::new(|req: &AgentView| Some(Reply::done(req.agent, req.request, "ok")))
        });
        let mut mgr =
            SubagentManager::with_worktree_base_dir(policy, 1, sub_factory, Some(&wt_base));

        // Push synthetic completions out of canonical order to verify sorting by `(spawn_tick, job_id)`.
        let push = |tx: &mpsc::Sender<SubagentCompletion>, tick: u64, id: &str| {
            tx.send(SubagentCompletion {
                proposal: SubagentProposal {
                    job_id: id.to_string(),
                    driver: "native".to_string(),
                    spawn_tick: tick,
                    branch_name: format!("onyx/sub/{id}"),
                    base_commit: "aaa".to_string(),
                    sub_commit: "bbb".to_string(),
                    ok: true,
                    summary: "done".to_string(),
                    touched_files: Vec::new(),
                },
                cancelled: false,
            })
            .unwrap();
        };
        push(&mgr.tx, 12, "b-job");
        push(&mgr.tx, 5, "z-job");
        push(&mgr.tx, 5, "a-job");

        let ordered = mgr.poll_completions();
        let keys: Vec<(u64, &str)> = ordered
            .iter()
            .map(|p| (p.spawn_tick, p.job_id.as_str()))
            .collect();
        assert_eq!(keys, vec![(5, "a-job"), (5, "z-job"), (12, "b-job")]);

        drop(mgr);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn subagent_manager_driver_integrates_with_recording_bridge_and_replays_offline() {
        let repo = init_temp_git_repo("driver_replay");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_replay_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let policy = WorkspacePolicy::new(&repo).unwrap();
        let tools = WorkspaceTools::new(policy.clone());

        let sub_factory: ResponderFactory = Arc::new(|| {
            let mut step = 0usize;
            Box::new(move |req: &AgentView| {
                step += 1;
                if step == 1 {
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"from_sub.txt","content":"hello from async subagent\n"}"#,
                    ))
                } else {
                    Some(Reply::done(req.agent, req.request, "wrote from_sub.txt"))
                }
            })
        });

        let cfg = AgentConfig {
            default_max_steps: 192,
            default_timeout_ticks: 512,
            ..AgentConfig::default()
        };
        let (mut host, ws_vocab) = build_host(1, cfg.clone(), None, None).unwrap();
        host.spawn(&make_spec("delegate and merge", true));

        let mgr = SubagentManager::with_worktree_base_dir(policy, 1, sub_factory, Some(&wt_base))
            .with_root_tools(tools.clone());

        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode(WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab)
            .wrapping(HeadlessDriver::new());
        let sub_driver = SubagentManagerDriver::new(host.vocab(), mgr).wrapping(ws_bridge);

        let phase = Arc::new(Mutex::new(0usize));
        let phase_clone = Arc::clone(&phase);
        let mut driver = RecordingBridge::new(host.vocab(), move |req: &AgentView| {
            let mut p = phase_clone.lock().unwrap();
            *p += 1;
            match *p {
                1 => Some(Reply::call_structured(
                    req.agent,
                    req.request,
                    "spawn_subagent",
                    r#"{"name":"writer","goal":"write from_sub.txt"}"#,
                )),
                // Poll status until the worker finishes (never entering WaitingChildren!).
                _ => {
                    let last_obs = req
                        .memory
                        .iter()
                        .rev()
                        .find(|(k, _)| k == "observe")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");
                    if last_obs.contains("\"merged\":true") {
                        Some(Reply::done(req.agent, req.request, "merged cleanly"))
                    } else if last_obs.contains("\"status\":\"done\"") {
                        Some(Reply::call_structured(
                            req.agent,
                            req.request,
                            "subagent_merge",
                            r#"{"job_id":"writer-1"}"#,
                        ))
                    } else {
                        thread::sleep(Duration::from_millis(120));
                        Some(Reply::call_structured(
                            req.agent,
                            req.request,
                            "subagent_status",
                            r#"{"job_id":"writer-1"}"#,
                        ))
                    }
                }
            }
        })
        .wrapping(sub_driver);

        for _ in 0..256 {
            host.step(&mut driver);
            // Verify root NEVER entered WaitingChildren during async delegation!
            if let Some(&root_id) = host.agents().first() {
                assert_ne!(host.agent_state(root_id), Some(AgentState::WaitingChildren));
            }
            if !host.agents().is_empty() && host.is_settled() {
                break;
            }
        }
        assert!(host.is_settled());
        let root_id = host.agents()[0];
        assert_eq!(
            host.agent_state(root_id),
            Some(AgentState::Done),
            "root agent did not finish cleanly: result={:?}, memory={:?}",
            host.result(root_id),
            host.view(root_id).memory
        );
        assert_eq!(
            fs::read_to_string(repo.join("from_sub.txt")).unwrap(),
            "hello from async subagent\n"
        );
        let original_fp = host.fingerprint();
        let tr_bytes = driver.transcript().to_bytes();
        drop(driver);

        // Pure offline replay: 0 threads, 0 worktrees, 0 disk touches — identical fingerprint!
        let tr = Transcript::from_bytes(&tr_bytes).unwrap();
        let (mut replay_host, _) = build_host(1, cfg, None, None).unwrap();
        replay_host.spawn(&make_spec("delegate and merge", true));
        let mut replay_driver = ReplayBridge::new(tr).wrapping(HeadlessDriver::new());
        for _ in 0..256 {
            replay_host.step(&mut replay_driver);
            if replay_driver.remaining() == 0 && replay_host.is_settled() {
                break;
            }
        }
        assert_eq!(replay_host.fingerprint(), original_fp);

        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn spawn_job_enforces_max_concurrent_subagents_cap() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let repo = init_temp_git_repo("max_cap");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_cap_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let release = Arc::new(AtomicBool::new(false));
        let release_factory = Arc::clone(&release);
        let blocking_factory: ResponderFactory = Arc::new(move || {
            let rel = Arc::clone(&release_factory);
            Box::new(move |req: &AgentView| {
                for _ in 0..300 {
                    if rel.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Some(Reply::done(req.agent, req.request, "released"))
            })
        });

        let policy = WorkspacePolicy::new(&repo).unwrap();
        let mut mgr =
            SubagentManager::with_worktree_base_dir(policy, 1, blocking_factory, Some(&wt_base))
                .with_max_concurrent(2);
        assert_eq!(mgr.max_concurrent(), 2);

        let j1 = mgr
            .spawn_job("job 1", Some("cap"), Some(8), 1, Vec::new(), None)
            .unwrap();
        let j2 = mgr
            .spawn_job("job 2", Some("cap"), Some(8), 2, Vec::new(), None)
            .unwrap();
        assert_eq!(mgr.active_count(), 2);

        let err = mgr
            .spawn_job("job 3", Some("cap"), Some(8), 3, Vec::new(), None)
            .unwrap_err();
        assert!(
            err.contains("max concurrent subagents reached (2/2)"),
            "{err}"
        );
        assert_eq!(
            fs::read_dir(j1.worktree_path.parent().unwrap())
                .unwrap()
                .count(),
            2
        );

        release.store(true, Ordering::Release);
        let _ = mgr.discard_job(&j1.job_id);
        let _ = mgr.discard_job(&j2.job_id);
        drop(mgr);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn spawn_job_rejects_unknown_external_driver_with_actionable_error() {
        let dir = std::env::temp_dir().join(format!(
            "openonyx_drv_unknown_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let policy = WorkspacePolicy::new(&dir).unwrap();
        let mut mgr = SubagentManager::new(policy, 1, Arc::new(|| Box::new(|_: &AgentView| None)));

        // `driver` desconocido → error accionable que lista los ids disponibles
        // (los presets integrados siempre están registrados: `native` + `opencode`).
        let err = mgr
            .spawn_job("goal", Some("x"), None, 1, Vec::new(), Some("nope"))
            .unwrap_err();
        assert!(err.contains("unknown subagent driver `nope`"), "{err}");
        assert!(err.contains("native, opencode"), "{err}");

        // `None` → backend nativo: el fallo (directorio no Git) llega en la creación del
        // worktree, NO en la resolución de driver.
        let err_native = mgr
            .spawn_job("goal", None, None, 2, Vec::new(), None)
            .unwrap_err();
        assert!(
            !err_native.contains("unknown subagent driver"),
            "{err_native}"
        );
        assert!(
            err_native.contains("async subagents require a Git repository"),
            "{err_native}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_job_blocks_external_driver_on_untrusted_workspace() {
        use onyx_workspace::WorkspaceTrustReport;

        let repo = init_temp_git_repo("drv_untrusted");
        let mut trust = WorkspaceTrustReport::no_project_config();
        trust.config_trusted = false;
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_trust_report(trust);
        let mut mgr = SubagentManager::new(policy, 1, Arc::new(|| Box::new(|_: &AgentView| None)));

        let err = mgr
            .spawn_job("goal", None, None, 1, Vec::new(), Some("opencode"))
            .unwrap_err();
        assert!(
            err.contains("requires a trusted workspace"),
            "untrusted workspace must hard-block external drivers, got: {err}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn spawn_job_runs_external_driver_in_worktree_and_reports_result() {
        use onyx_workspace::PromptTransport;

        let repo = init_temp_git_repo("drv_e2e");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_drv_e2e_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        // Driver externo de prueba con un bin inexistente: el job debe completarse con
        // `ok=false` y un summary accionable, sin romper el ciclo de trabajotree.
        let fake = onyx_workspace::ExternalDriverConfig {
            id: "fake-drv".to_string(),
            bin: "definitely-missing-onyx-binary".to_string(),
            args: Vec::new(),
            prompt_transport: PromptTransport::Stdin,
            prompt_flag: None,
            timeout: Duration::from_secs(10),
            allowed_env: Vec::new(),
            inherit_home: false,
            network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(Vec::new()),
        };
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_external_drivers(vec![fake]);
        let mut mgr = SubagentManager::with_worktree_base_dir(
            policy,
            1,
            Arc::new(|| Box::new(|_: &AgentView| None)),
            Some(&wt_base),
        );
        assert_eq!(mgr.external_driver_ids(), vec!["fake-drv".to_string()]);

        let spawned = mgr
            .spawn_job(
                "do something",
                Some("ext"),
                None,
                7,
                Vec::new(),
                Some("fake-drv"),
            )
            .unwrap();
        assert_eq!(spawned.driver, "fake-drv");
        assert_eq!(spawned.status, "running");

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let statuses = mgr.status(Some(&spawned.job_id)).unwrap();
            if statuses[0].status == "done" || statuses[0].status == "failed" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "external driver job did not finish"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let s = &mgr.status(Some(&spawned.job_id)).unwrap()[0];
        assert_eq!(s.driver, "fake-drv");
        assert_eq!(s.status, "failed");
        assert_eq!(s.ok, Some(false));
        // `SDD-10` Fase 2: el binario inexistente se rechaza **antes** del `spawn`
        // (resolución explícita contra `PATH`/`PATHEXT`), con summary accionable.
        assert!(
            s.summary
                .contains("binary `definitely-missing-onyx-binary` was not resolved against PATH"),
            "summary must be actionable, got: {}",
            s.summary
        );
        assert!(!s.touched_files.iter().any(|f| f == "sub.txt"));
        assert!(
            s.worktree_path.is_dir(),
            "worktree stays until discard/merge"
        );
        let _ = mgr.discard_job(&spawned.job_id);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    /// `SDD-10` Fase 2: el techo `max_concurrent` es **compartido** por subagentes nativos y
    /// drivers externos, y el error de capacidad ocurre **antes** de crear el worktree.
    #[test]
    fn spawn_job_max_concurrent_is_shared_by_native_and_external_before_worktree_creation() {
        use onyx_workspace::{ExternalDriverConfig, PromptTransport, SubagentNetworkPolicy};
        use std::sync::atomic::{AtomicBool, Ordering};

        let repo = init_temp_git_repo("mixed_cap");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_mixedcap_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        // 2 nativos bloqueados por factory + 1 externo que se mantiene vivo (sleep/ping).
        let release = Arc::new(AtomicBool::new(false));
        let release_factory = Arc::clone(&release);
        // Presupuesto de bloqueo amplio (30 s): bajo carga de batería completa, los
        // `spawn_job` (git worktree) tardan varios segundos; con un presupuesto corto el
        // worker nativo devolvería `Reply::done` antes del assert de `active_count`.
        let blocking_factory: ResponderFactory = Arc::new(move || {
            let rel = Arc::clone(&release_factory);
            Box::new(move |req: &AgentView| {
                for _ in 0..3000 {
                    if rel.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Some(Reply::done(req.agent, req.request, "released"))
            })
        });

        let sleeper = if cfg!(windows) {
            ExternalDriverConfig {
                id: "sleeper".to_string(),
                bin: "ping".to_string(),
                args: vec!["-n".to_string(), "30".to_string(), "127.0.0.1".to_string()],
                prompt_transport: PromptTransport::Stdin,
                prompt_flag: None,
                timeout: Duration::from_secs(120),
                allowed_env: Vec::new(),
                inherit_home: false,
                network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(Vec::new()),
            }
        } else {
            ExternalDriverConfig {
                id: "sleeper".to_string(),
                bin: "sleep".to_string(),
                args: vec!["30".to_string()],
                prompt_transport: PromptTransport::Stdin,
                prompt_flag: None,
                timeout: Duration::from_secs(120),
                allowed_env: Vec::new(),
                inherit_home: false,
                network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(Vec::new()),
            }
        };
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_external_drivers(vec![sleeper]);
        let mut mgr =
            SubagentManager::with_worktree_base_dir(policy, 1, blocking_factory, Some(&wt_base))
                .with_max_concurrent(3);

        let n1 = mgr
            .spawn_job("n1", Some("mix"), Some(8), 1, Vec::new(), None)
            .unwrap();
        let n2 = mgr
            .spawn_job("n2", Some("mix"), Some(8), 2, Vec::new(), None)
            .unwrap();
        let e1 = mgr
            .spawn_job("e1", Some("mix"), None, 3, Vec::new(), Some("sleeper"))
            .unwrap();
        assert_eq!(e1.driver, "sleeper");
        let active = mgr.active_count();
        assert_eq!(
            active,
            3,
            "los 3 jobs debían estar Running ({}); estados: {:?}",
            active,
            mgr.status(None).unwrap()
        );

        // Los worktrees son hijos directos del directorio base resuelto (anidado bajo
        // `wt_base/openonyx_subagent_<tag>_<pid>_<nanos>/`).
        let wt_parent = n1.worktree_path.parent().unwrap().to_path_buf();
        let count_worktrees = || {
            fs::read_dir(&wt_parent)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .count()
        };

        // El 4º (nativo) no crea worktree: el techo se comprueba antes de `create_worktree`.
        let err = mgr
            .spawn_job("n4", Some("mix"), Some(8), 4, Vec::new(), None)
            .unwrap_err();
        assert!(
            err.contains("max concurrent subagents reached (3/3)"),
            "{err}"
        );
        assert_eq!(count_worktrees(), 3);

        // Y el 4º externo tampoco (resolución de driver + techo, sin worktree).
        let err = mgr
            .spawn_job("e4", Some("mix"), None, 5, Vec::new(), Some("sleeper"))
            .unwrap_err();
        assert!(
            err.contains("max concurrent subagents reached (3/3)"),
            "{err}"
        );
        assert_eq!(count_worktrees(), 3);

        release.store(true, Ordering::Release);
        for id in [&n1.job_id, &n2.job_id, &e1.job_id] {
            mgr.discard_job(id)
                .unwrap_or_else(|e| panic!("discard {id}: {e}"));
        }
        assert_eq!(count_worktrees(), 0);
        drop(mgr);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    /// `SDD-10` Fase 2 (nivel manager): un driver externo con timeout y un **nieto** vivo que
    /// retiene la pipe muere entero por `terminate()`/`taskkill`, el job queda `failed` y
    /// `discard_job` elimina el worktree **sin** `Access Denied` (0 handles/procesos vivos).
    #[test]
    fn spawn_job_external_driver_timeout_with_grandchild_allows_worktree_removal() {
        use onyx_workspace::{ExternalDriverConfig, PromptTransport, SubagentNetworkPolicy};

        let repo = init_temp_git_repo("ext_timeout_wt");
        let wt_base = repo.parent().unwrap().join(format!(
            "openonyx_wt_base_exttimeout_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&wt_base);

        // Script en el host (fuera del worktree): toca un fichero del worktree y deja un
        // nieto vivo (`ping`/`sleep`) heredando la pipe de `stdout`.
        let host_tmp = std::env::temp_dir().join(format!(
            "onyx_extdrv_wt_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&host_tmp).unwrap();
        let script = write_stall_script(&host_tmp);

        let driver = ExternalDriverConfig {
            id: "staller".to_string(),
            bin: script,
            args: Vec::new(),
            prompt_transport: PromptTransport::Stdin,
            prompt_flag: None,
            timeout: Duration::from_secs(3),
            allowed_env: Vec::new(),
            inherit_home: false,
            network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(Vec::new()),
        };
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_external_drivers(vec![driver]);
        let mut mgr = SubagentManager::with_worktree_base_dir(
            policy,
            1,
            Arc::new(|| Box::new(|_: &AgentView| None)),
            Some(&wt_base),
        );

        let spawned = mgr
            .spawn_job("stall", Some("stall"), None, 1, Vec::new(), Some("staller"))
            .unwrap();
        assert_eq!(spawned.driver, "staller");

        // El job debe terminar en `failed` (timeout ~3 s + kill del árbol) en < 30 s.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let statuses = mgr.status(Some(&spawned.job_id)).unwrap();
            if matches!(statuses[0].status, "done" | "failed") {
                break;
            }
            assert!(Instant::now() < deadline, "timeout job did not finish");
            thread::sleep(Duration::from_millis(50));
        }
        let s = &mgr.status(Some(&spawned.job_id)).unwrap()[0];
        assert_eq!(s.status, "failed", "el driver debió fallar por timeout");
        assert_eq!(s.ok, Some(false));

        // `discard_job` elimina el worktree sin procesos vivos reteniendo handles (el
        // nieto fue matado junto al árbol): `git worktree remove` no da `Access Denied`.
        let r = mgr.discard_job(&spawned.job_id);
        assert!(
            r.is_ok(),
            "remove_worktree falló (¿nieto vivo en el worktree?): {:?}",
            r.err()
        );
        assert!(!s.worktree_path.exists(), "worktree debe estar desmontado");
        drop(mgr);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&host_tmp);
    }

    #[test]
    fn subagent_merge_toctou_aborts_when_root_changes_after_preview_and_keeps_worktree() {
        let repo = init_temp_git_repo("toctou");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_toctou_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let sub_factory: ResponderFactory = Arc::new(|| {
            let mut step = 0usize;
            Box::new(move |req: &AgentView| {
                step += 1;
                if step == 1 {
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"from_sub.txt","content":"subagent payload\n"}"#,
                    ))
                } else {
                    Some(Reply::done(req.agent, req.request, "wrote from_sub.txt"))
                }
            })
        });

        let policy = WorkspacePolicy::new(&repo).unwrap();
        let mut mgr =
            SubagentManager::with_worktree_base_dir(policy, 3, sub_factory, Some(&wt_base));
        let spawned = mgr
            .spawn_job("write file", Some("toctou"), Some(16), 4, Vec::new(), None)
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let statuses = mgr.status(Some(&spawned.job_id)).unwrap();
            if statuses[0].status == "done" {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for worker");
            thread::sleep(Duration::from_millis(20));
        }

        let preview = mgr.prepare_merge_preview(&spawned.job_id).unwrap();
        assert!(matches!(preview, MergePreviewState::Clean(_)));

        // Concurrent external edit after preview must trigger TOCTOU abort without touching disk.
        fs::write(repo.join("root.txt"), "modified-after-preview\n").unwrap();

        let err = mgr.merge_job(&spawned.job_id).unwrap_err();
        assert!(err.contains("TOCTOU guard aborted merge"), "{err}");
        assert!(!repo.join("from_sub.txt").exists());
        assert_eq!(
            fs::read_to_string(repo.join("root.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "modified-after-preview\n"
        );
        assert!(
            spawned.worktree_path.exists(),
            "worktree must remain intact when TOCTOU aborts merge"
        );

        let _ = mgr.discard_job(&spawned.job_id).unwrap();
        drop(mgr);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn preview_merge_for_request_reuses_sealed_ticket_per_approval_key() {
        let repo = init_temp_git_repo("approval_key");
        let wt_base = repo.parent().unwrap().join(format!(
            "openonyx_wt_base_approval_key_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&wt_base);

        let sub_factory: ResponderFactory = Arc::new(|| {
            let mut step = 0usize;
            Box::new(move |req: &AgentView| {
                step += 1;
                if step == 1 {
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"from_sub.txt","content":"subagent payload\n"}"#,
                    ))
                } else {
                    Some(Reply::done(req.agent, req.request, "wrote from_sub.txt"))
                }
            })
        });

        let policy = WorkspacePolicy::new(&repo).unwrap();
        let mut mgr =
            SubagentManager::with_worktree_base_dir(policy, 7, sub_factory, Some(&wt_base));
        let spawned = mgr
            .spawn_job("write file", Some("keyed"), Some(16), 5, Vec::new(), None)
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let statuses = mgr.status(Some(&spawned.job_id)).unwrap();
            if statuses[0].status == "done" {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for worker");
            thread::sleep(Duration::from_millis(20));
        }

        // 1º redibujado: sella el ticket para la solicitud de aprobación (11, 1).
        let first = match mgr
            .preview_merge_for_request(&spawned.job_id, 11, 1)
            .unwrap()
        {
            MergePreviewState::Clean(t) => t,
            other => panic!("expected Clean preview, got {other:?}"),
        };
        assert_eq!(first.approval_key, Some((11, 1)));
        let sealed_tree = first.root_now_tree.clone();

        // Edición externa concurrente: el árbol de root cambia mientras "el usuario decide".
        fs::write(repo.join("late_edit.txt"), "concurrent root edit\n").unwrap();

        // Mismo `(agent_id, request_id)`: reutiliza el ticket sellado sin re-sellar
        // `root_now_tree` (ni `git write-tree` ni `merge-tree` en cada frame).
        let redraw = match mgr
            .preview_merge_for_request(&spawned.job_id, 11, 1)
            .unwrap()
        {
            MergePreviewState::Clean(t) => t,
            other => panic!("expected Clean preview, got {other:?}"),
        };
        assert_eq!(redraw.approval_key, Some((11, 1)));
        assert_eq!(
            redraw.root_now_tree, sealed_tree,
            "redraw must reuse the sealed ticket, not re-scale root_now_tree"
        );

        // Nueva solicitud (11, 2): re-evalúa y sella un ticket fresco contra el árbol actual.
        let second = match mgr
            .preview_merge_for_request(&spawned.job_id, 11, 2)
            .unwrap()
        {
            MergePreviewState::Clean(t) => t,
            other => panic!("expected Clean preview, got {other:?}"),
        };
        assert_eq!(second.approval_key, Some((11, 2)));
        assert_ne!(
            second.root_now_tree, sealed_tree,
            "a new approval request must re-scale against the current working tree"
        );

        // La guarda anti-TOCTOU sigue protegiendo el apply: un cambio posterior al
        // último sello debe abortar el merge sin tocar el disco de root.
        fs::write(repo.join("post_seal.txt"), "another root edit\n").unwrap();
        let err = mgr.merge_job(&spawned.job_id).unwrap_err();
        assert!(err.contains("TOCTOU guard aborted merge"), "{err}");
        assert!(!repo.join("from_sub.txt").exists());

        let _ = mgr.discard_job(&spawned.job_id).unwrap();
        drop(mgr);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn subagent_merge_conflict_preserves_worktree_and_returns_conflicting_paths() {
        let repo = init_temp_git_repo("conflict");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_conflict_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let sub_factory: ResponderFactory = Arc::new(|| {
            let mut step = 0usize;
            Box::new(move |req: &AgentView| {
                step += 1;
                match step {
                    1 => Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_read",
                        r#"{"path":"sub.txt"}"#,
                    )),
                    2 => Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"sub.txt","content":"subagent-conflicting-edit\n"}"#,
                    )),
                    _ => Some(Reply::done(req.agent, req.request, "edited sub.txt")),
                }
            })
        });

        let policy = WorkspacePolicy::new(&repo).unwrap();
        let mut mgr =
            SubagentManager::with_worktree_base_dir(policy, 5, sub_factory, Some(&wt_base));
        let spawned = mgr
            .spawn_job(
                "edit sub.txt",
                Some("conflict"),
                Some(16),
                6,
                Vec::new(),
                None,
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let statuses = mgr.status(Some(&spawned.job_id)).unwrap();
            if statuses[0].status == "done" {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for worker");
            thread::sleep(Duration::from_millis(20));
        }

        fs::write(repo.join("sub.txt"), "root-conflicting-edit\n").unwrap();

        let err = mgr.merge_job(&spawned.job_id).unwrap_err();
        assert!(err.contains("3-way merge conflict"), "{err}");
        assert!(err.contains("sub.txt"), "{err}");
        assert_eq!(
            fs::read_to_string(repo.join("sub.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "root-conflicting-edit\n"
        );
        assert!(
            spawned.worktree_path.exists(),
            "worktree must NOT be deleted on Conflict"
        );
        assert!(git_branch_exists(&repo, &spawned.branch_name));

        mgr.discard_job(&spawned.job_id).unwrap();
        assert!(!spawned.worktree_path.exists());
        assert!(!git_branch_exists(&repo, &spawned.branch_name));
        drop(mgr);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn subagent_merge_scent_heat_escalates_allow_to_ask_on_hot_public_symbol() {
        use onyx_workspace::PermissionRules;

        let repo = init_temp_git_repo("scent_heat");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(
            repo.join("src/lib.rs"),
            "pub fn hot_api(x: i32) -> i32 {\n    x + 1\n}\n\npub fn caller_one() -> i32 {\n    hot_api(1)\n}\n\npub fn caller_two() -> i32 {\n    hot_api(2)\n}\n",
        )
        .unwrap();
        let add_out = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(add_out.status.success());
        let commit_out = Command::new("git")
            .args(["commit", "-q", "-m", "add hot_api"])
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@local")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@local")
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(commit_out.status.success());

        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_scent_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_permissions(PermissionRules {
                allow: vec!["subagent_merge".to_string()],
                ..Default::default()
            });
        assert_eq!(
            policy.evaluate_permission_in_mode(
                WorkspaceMode::Build,
                "subagent_merge",
                r#"{"job_id":"scent-1"}"#
            ),
            onyx_workspace::PermissionDecision::Allow
        );
        let tools = WorkspaceTools::new(policy.clone());

        let sub_factory: ResponderFactory = Arc::new(|| {
            let mut step = 0usize;
            Box::new(move |req: &AgentView| {
                step += 1;
                match step {
                    1 => Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_read",
                        r#"{"path":"src/lib.rs"}"#,
                    )),
                    2 => Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_edit",
                        r#"{"path":"src/lib.rs","old_text":"x + 1","new_text":"x + 2"}"#,
                    )),
                    _ => Some(Reply::done(req.agent, req.request, "updated hot_api")),
                }
            })
        });

        let cfg = AgentConfig {
            default_max_steps: 192,
            default_timeout_ticks: 512,
            ..AgentConfig::default()
        };
        let (mut host, ws_vocab) = build_host(1, cfg, None, None).unwrap();
        host.spawn(&make_spec("modify hot public symbol", false));

        let mgr =
            SubagentManager::with_worktree_base_dir(policy.clone(), 1, sub_factory, Some(&wt_base))
                .with_root_tools(tools.clone());
        let handle = SubagentHandle::new(mgr);
        let handle_for_driver = handle.clone();

        let seen_approval_tool = Arc::new(Mutex::new(None));
        struct CaptureApprovalDriver {
            vocab: onyx_soul_agents::Vocab,
            seen_tool: Arc<Mutex<Option<String>>>,
        }
        impl PlatformDriver for CaptureApprovalDriver {
            fn poll(&mut self, _: &mut dyn onyx_soul_agents::EventBus) {}
            fn present(&mut self, graph: &dyn GraphView, _: &[onyx_soul_agents::SurfaceBinding]) {
                for v in onyx_soul_agents::pending_approvals(graph, &self.vocab) {
                    if let Some(p) = &v.pending_approval {
                        *self.seen_tool.lock().unwrap() = Some(p.tool.clone());
                    }
                }
            }
        }

        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode(WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab)
            .wrapping(CaptureApprovalDriver {
                vocab: host.vocab(),
                seen_tool: Arc::clone(&seen_approval_tool),
            });
        let sub_driver = SubagentManagerDriver::new(host.vocab(), handle_for_driver)
            .with_policy(policy)
            .with_mode_handle(WorkspaceModeHandle::new(WorkspaceMode::Build))
            .wrapping(ws_bridge);

        let phase = Arc::new(Mutex::new(0usize));
        let phase_clone = Arc::clone(&phase);
        let mut driver = RecordingBridge::new(host.vocab(), move |req: &AgentView| {
            let mut p = phase_clone.lock().unwrap();
            *p += 1;
            if *p == 1 {
                Some(Reply::call_structured(
                    req.agent,
                    req.request,
                    "spawn_subagent",
                    r#"{"name":"scent","goal":"update hot_api"}"#,
                ))
            } else {
                let last_obs = req
                    .memory
                    .iter()
                    .rev()
                    .find(|(k, _)| k == "observe")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("");
                if last_obs.contains("\"status\":\"done\"") {
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "subagent_merge",
                        r#"{"job_id":"scent-1"}"#,
                    ))
                } else {
                    thread::sleep(Duration::from_millis(120));
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "subagent_status",
                        r#"{"job_id":"scent-1"}"#,
                    ))
                }
            }
        })
        .wrapping(sub_driver);

        for _ in 0..256 {
            host.step(&mut driver);
            if *seen_approval_tool.lock().unwrap() == Some("subagent_merge".to_string()) {
                // Step two more ticks to verify static Allow was NOT auto-approved.
                host.step(&mut driver);
                host.step(&mut driver);
                break;
            }
        }

        let root_id = host.agents()[0];
        assert_eq!(
            *seen_approval_tool.lock().unwrap(),
            Some("subagent_merge".to_string())
        );
        assert_eq!(
            host.agent_state(root_id),
            Some(AgentState::WaitingApproval),
            "scent heat on public `hot_api` must escalate Allow -> Ask"
        );
        match handle.preview_merge_for_request("scent-1", 0, 0).unwrap() {
            MergePreviewState::Clean(ticket) => {
                let impact = ticket.impact.expect("ticket must carry Scent impact");
                assert!(impact.escalates_allow_to_ask, "{impact:?}");
                assert!(impact.summary_line.contains("hot_api"), "{impact:?}");
            }
            other => panic!("expected Clean MergePreviewState, got {other:?}"),
        }

        let _ = handle.discard_job("scent-1");
        drop(driver);
        drop(handle);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn proactive_completion_wakes_idle_root_and_replays_with_zero_disk_side_effects() {
        let repo = init_temp_git_repo("proactive_wake");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_wake_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);

        let policy = WorkspacePolicy::new(&repo).unwrap();
        let tools = WorkspaceTools::new(policy.clone());

        let sub_factory: ResponderFactory = Arc::new(|| {
            let mut step = 0usize;
            Box::new(move |req: &AgentView| {
                step += 1;
                if step == 1 {
                    thread::sleep(Duration::from_millis(120));
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"bg_out.txt","content":"proactive hello\n"}"#,
                    ))
                } else {
                    Some(Reply::done(req.agent, req.request, "bg job completed"))
                }
            })
        });

        let cfg = AgentConfig {
            default_max_steps: 32,
            default_timeout_ticks: 1024,
            ..AgentConfig::default()
        };
        let spawner = crate::command::CommandSpawnerSystem::new(true);
        let (mut host, ws_vocab) =
            build_host(1, cfg.clone(), None, Some(Box::new(spawner))).unwrap();
        host.spawn(&make_spec("delegate in background", true));

        let mgr = SubagentManager::with_worktree_base_dir(policy, 1, sub_factory, Some(&wt_base))
            .with_root_tools(tools.clone());
        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode(WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab)
            .wrapping(HeadlessDriver::new());
        let sub_driver = SubagentManagerDriver::new(host.vocab(), mgr)
            .with_proactive_notifications(true)
            .wrapping(ws_bridge);

        let phase = Arc::new(Mutex::new(0usize));
        let phase_clone = Arc::clone(&phase);
        let mut driver = RecordingBridge::new(host.vocab(), move |req: &AgentView| {
            let mut p = phase_clone.lock().unwrap();
            *p += 1;
            match *p {
                1 => Some(Reply::call_structured(
                    req.agent,
                    req.request,
                    "spawn_subagent",
                    r#"{"name":"bg","goal":"write bg_out.txt"}"#,
                )),
                // Finish initial turn immediately so root goes IDLE (`Done`) before worker completes.
                2 => Some(Reply::done(req.agent, req.request, "waiting idly")),
                3 => {
                    assert!(
                        req.memory
                            .iter()
                            .any(|(_, m)| m.contains("[subagent `bg-1` finished")),
                        "expected proactive wake notice in memory: {:?}",
                        req.memory
                    );
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "subagent_merge",
                        r#"{"job_id":"bg-1"}"#,
                    ))
                }
                _ => Some(Reply::done(
                    req.agent,
                    req.request,
                    "woken proactively and merged",
                )),
            }
        })
        .wrapping(sub_driver);

        let mut merged_seen = false;
        let mut idle_reached = false;
        for _ in 0..320 {
            host.step(&mut driver);
            let root_id = host.agents()[0];
            if *phase.lock().unwrap() == 2 && host.agent_state(root_id) == Some(AgentState::Done) {
                idle_reached = true;
            }
            if *phase.lock().unwrap() >= 4 && host.is_settled() {
                merged_seen = true;
                break;
            }
            if host.is_settled() {
                thread::sleep(Duration::from_millis(80));
            }
        }
        assert!(
            idle_reached,
            "root must reach idle Done state before wakeup"
        );
        assert!(merged_seen, "proactive completion must wake root to merge");
        assert_eq!(
            fs::read_to_string(repo.join("bg_out.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "proactive hello\n"
        );
        let recs = onyx_workspace::graph_subagent_records(host.graph(), &ws_vocab);
        assert!(
            recs.iter()
                .any(|r| r.job_id == "bg-1" && r.status == "merged"),
            "{recs:?}"
        );

        let original_fp = host.fingerprint();
        let tr_bytes = driver.transcript().to_bytes();
        drop(driver);
        fs::remove_file(repo.join("bg_out.txt")).unwrap();
        let _ = fs::remove_dir_all(&wt_base);

        // Replay must run without SubagentManagerDriver, 0 threads/worktrees, 0 disk touches.
        let tr = Transcript::from_bytes(&tr_bytes).unwrap();
        let replay_spawner = crate::command::CommandSpawnerSystem::new(true);
        let (mut replay_host, _) =
            build_host(1, cfg, None, Some(Box::new(replay_spawner))).unwrap();
        replay_host.spawn(&make_spec("delegate in background", true));
        let mut replay_driver = ReplayBridge::new(tr).wrapping(HeadlessDriver::new());
        for _ in 0..512 {
            replay_host.step(&mut replay_driver);
            if replay_driver.remaining() == 0 && replay_host.is_settled() {
                break;
            }
        }
        assert_eq!(replay_host.fingerprint(), original_fp);
        assert!(!repo.join("bg_out.txt").exists());
        assert!(!wt_base.exists());

        let _ = fs::remove_dir_all(&repo);
    }

    /// `SDD-10` Fase 3 (retrocompatibilidad): una propuesta grabada en el wire **sin** el
    /// campo `driver` (`SDD-09`) decodifica como `"native"`; el round-trip preserva el
    /// `id` de un driver externo y el flag `cancelled`.
    #[test]
    fn decode_proposal_command_without_driver_field_defaults_to_native() {
        let legacy = format!(
            "{SUBAGENT_PROPOSAL_CMD_PREFIX}{{\"proposal\":{{\"job_id\":\"old-1\",\"spawn_tick\":3,\"branch_name\":\"onyx/sub/old-1\",\"base_commit\":\"b\",\"sub_commit\":\"s\",\"ok\":true,\"summary\":\"legacy sdd-09\",\"touched_files\":[\"a.txt\"]}},\"cancelled\":false}}"
        );
        let (p, cancelled) =
            decode_proposal_command(&legacy).expect("el comando legacy debe decodificar");
        assert_eq!(p.driver, "native");
        assert_eq!(p.job_id, "old-1");
        assert!(!cancelled);

        let mut ext = p.clone();
        ext.driver = "fake-drv".to_string();
        let cmd = encode_proposal_command(&ext, true);
        let (p2, c2) = decode_proposal_command(&cmd).unwrap();
        assert_eq!(p2.driver, "fake-drv");
        assert!(c2);
    }

    /// Directorio temporal del host (fuera de repo y worktree) para scripts de CLI fake.
    fn host_tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onyx_extdrv_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Script cross-platform de CLI fake: `.cmd` en Windows (CRLF), `.sh` (0755) en el resto;
    /// devuelve la ruta absoluta para usarla como `bin` del driver externo.
    fn write_cli_script(
        host_tmp: &Path,
        name: &str,
        body_windows: &str,
        body_posix: &str,
    ) -> String {
        #[cfg(windows)]
        {
            let _ = body_posix;
            let p = host_tmp.join(format!("{name}.cmd"));
            fs::write(&p, body_windows).unwrap();
            p.to_string_lossy().into_owned()
        }
        #[cfg(not(windows))]
        {
            let _ = body_windows;
            let p = host_tmp.join(format!("{name}.sh"));
            fs::write(&p, body_posix).unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            p.to_string_lossy().into_owned()
        }
    }

    /// `ExternalDriverConfig` mínima para los tests de `SDD-10` Fase 3 (stdin, 30 s).
    fn test_external_driver(id: &str, bin: String) -> onyx_workspace::ExternalDriverConfig {
        onyx_workspace::ExternalDriverConfig {
            id: id.to_string(),
            bin,
            args: Vec::new(),
            prompt_transport: onyx_workspace::PromptTransport::Stdin,
            prompt_flag: None,
            timeout: Duration::from_secs(30),
            allowed_env: Vec::new(),
            inherit_home: false,
            network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(Vec::new()),
        }
    }

    /// Espera (con deadline de 30 s) a que un job de driver externo llegue a `done`/`failed`.
    fn wait_for_external_job(mgr: &mut SubagentManager, job_id: &str) -> SubagentJobSummary {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let st = mgr.status(Some(job_id)).unwrap();
            if st[0].status == "done" || st[0].status == "failed" {
                return st[0].clone();
            }
            assert!(
                Instant::now() < deadline,
                "el job externo `{job_id}` no terminó en 30 s: {:?}",
                st[0]
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Driver de test que aprueba **solo** los `spawn_subagent` pendientes (la aprobación
    /// de `subagent_merge` la decide `SubagentManagerDriver`; el resto se deja pendiente).
    struct AutoApproveSpawns {
        vocab: Vocab,
        queued: Vec<Reply>,
        handled: BTreeSet<(u64, u64)>,
    }
    impl PlatformDriver for AutoApproveSpawns {
        fn poll(&mut self, bus: &mut dyn EventBus) {
            for r in self.queued.drain(..) {
                bus.push(r.to_event());
            }
        }
        fn present(&mut self, graph: &dyn GraphView, _: &[SurfaceBinding]) {
            for v in pending_approvals(graph, &self.vocab) {
                if v.pending_approval
                    .as_ref()
                    .is_some_and(|p| p.tool == "spawn_subagent")
                    && self.handled.insert((v.agent.0, v.request))
                {
                    self.queued.push(Reply::approve(v.agent, v.request));
                }
            }
        }
    }

    /// `SDD-10` Fase 3 (transductor determinista): el stdout del CLI **miente** (clama un
    /// `lying.txt` que nunca crea y fuga un token); la verdad llega **solo** de Git
    /// (`capture_subagent_commit` + `changed_files_between`), y el log crudo se sella en el
    /// vault del root **pasado por `scrub_secrets`**.
    #[test]
    fn external_driver_stdout_lies_but_git_is_the_truth_and_vault_log_is_scrubbed() {
        let repo = init_temp_git_repo("drv_liar");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_liar_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);
        let host_tmp = host_tmp_dir("liar");

        // El CLI fake crea **solo** `real-change.txt` en el worktree (CWD del runner).
        let token = "sk-abcdefghijklmnopqrstuvwxyz1234";
        let bin = write_cli_script(
            &host_tmp,
            "liar",
            &format!(
                "@echo off\r\necho hello > real-change.txt\r\necho CLAIM lying.txt\r\necho token {token}\r\n"
            ),
            &format!(
                "#!/bin/sh\necho hello > real-change.txt\necho CLAIM lying.txt\necho token {token}\n"
            ),
        );
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_external_drivers(vec![test_external_driver("liar", bin)]);
        let mut mgr = SubagentManager::with_worktree_base_dir(
            policy,
            1,
            Arc::new(|| Box::new(|_: &AgentView| None)),
            Some(&wt_base),
        )
        .with_root_tools(WorkspaceTools::new(WorkspacePolicy::new(&repo).unwrap()));

        let spawned = mgr
            .spawn_job("lie to me", Some("ext"), None, 1, Vec::new(), Some("liar"))
            .unwrap();
        assert_eq!(spawned.job_id, "ext-1");
        let s = wait_for_external_job(&mut mgr, "ext-1");
        assert_eq!(s.status, "done");
        assert_eq!(s.ok, Some(true));
        // La verdad viene de Git: el `lying.txt` de la mentira del stdout NO existe en el delta.
        assert_eq!(s.touched_files, vec!["real-change.txt".to_string()]);
        // El summary es el log saneado: conserva la claim y redacta el token.
        assert!(s.summary.contains("CLAIM lying.txt"), "{}", s.summary);
        assert!(s.summary.contains("[REDACTED]"), "{}", s.summary);
        assert!(!s.summary.contains(token), "{}", s.summary);

        // Log crudo (scrubbeado) sellado en el **vault del workspace root**, nunca en el worktree.
        let log = crate::vault::vault_subagent_logs_dir(&repo).join("ext-1.log");
        let log_raw = fs::read_to_string(&log)
            .unwrap_or_else(|e| panic!("falta el log del vault en {}: {e}", log.display()));
        assert!(log_raw.contains("CLAIM lying.txt"), "{log_raw}");
        assert!(log_raw.contains("[REDACTED]"), "{log_raw}");
        assert!(!log_raw.contains(token), "{log_raw}");
        assert!(
            crate::vault::vault_root(&repo).join(".gitignore").exists(),
            "el vault debe llevar su `.gitignore` (`*`)"
        );

        // Preview sellado: `Clean` con `sub_commit` real y el delta exacto.
        match mgr.prepare_merge_preview("ext-1").unwrap() {
            MergePreviewState::Clean(ticket) => {
                assert!(!ticket.sub_commit.is_empty());
                assert_eq!(ticket.changed_files, vec!["real-change.txt".to_string()]);
            }
            other => panic!("esperado Clean, obtenido {other:?}"),
        }
        let report = mgr.merge_job("ext-1").unwrap();
        assert!(!report.was_noop);
        assert_eq!(report.changed_files, vec!["real-change.txt".to_string()]);
        assert!(repo.join("real-change.txt").is_file());
        assert!(!repo.join("lying.txt").exists());

        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&host_tmp);
    }

    /// `SDD-10` Fase 3 (NoOp): un driver externo que **no cambia nada** en el worktree
    /// produce `sub_tree == base_tree` → el merge es `NoOp` y el disco de `root` queda intacto.
    #[test]
    fn external_driver_with_no_changes_produces_noop_merge_root_untouched() {
        let repo = init_temp_git_repo("drv_noop");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_noop_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);
        let host_tmp = host_tmp_dir("noop");

        let bin = write_cli_script(
            &host_tmp,
            "noop",
            "@echo off\r\necho doing absolutely nothing\r\n",
            "#!/bin/sh\necho doing absolutely nothing\n",
        );
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_external_drivers(vec![test_external_driver("noopy", bin)]);
        let mut mgr = SubagentManager::with_worktree_base_dir(
            policy,
            1,
            Arc::new(|| Box::new(|_: &AgentView| None)),
            Some(&wt_base),
        );

        let spawned = mgr
            .spawn_job(
                "noop goal",
                Some("noop"),
                None,
                1,
                Vec::new(),
                Some("noopy"),
            )
            .unwrap();
        let s = wait_for_external_job(&mut mgr, &spawned.job_id);
        assert_eq!(s.status, "done");
        assert!(s.touched_files.is_empty(), "{:?}", s.touched_files);

        match mgr.prepare_merge_preview(&spawned.job_id).unwrap() {
            MergePreviewState::NoOp => {}
            other => panic!("esperado NoOp, obtenido {other:?}"),
        }
        let head_before = git_head_sha(&repo);
        let report = mgr.merge_job(&spawned.job_id).unwrap();
        assert!(report.was_noop);
        assert!(report.changed_files.is_empty());
        // `root` intacto: `HEAD` sin mover y contenido sin cambios.
        assert_eq!(git_head_sha(&repo), head_before);
        assert_eq!(
            fs::read_to_string(repo.join("root.txt")).unwrap(),
            "root-v1\n"
        );

        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&host_tmp);
    }

    /// `SDD-10` Fase 3 (Control Plane): un driver externo que toca `.env` + `.onyx/config.toml`
    /// → el preview se niega de plano (`ControlPlaneDenied`) y `merge_job` falla **sin tocar**
    /// el disco de `root` (el worktree se conserva para inspección/descarte).
    #[test]
    fn external_driver_control_plane_delta_is_denied_without_touching_root() {
        let repo = init_temp_git_repo("drv_cp_deny");
        // Sentinel de Control Plane **commitado** en `root` (el worktree lo hereda en base).
        fs::create_dir_all(repo.join(".onyx")).unwrap();
        fs::write(repo.join(".onyx").join("config.toml"), "control=original\n").unwrap();
        let add_out = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(add_out.status.success());
        let commit_out = Command::new("git")
            .args(["commit", "-q", "-m", "add control plane sentinel"])
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@local")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@local")
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(commit_out.status.success());

        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_cpdeny_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);
        let host_tmp = host_tmp_dir("cpdeny");

        let bin = write_cli_script(
            &host_tmp,
            "cp",
            "@echo off\r\necho control=hacked > .onyx\\config.toml\r\necho secret=value > .env\r\n",
            "#!/bin/sh\necho control=hacked > .onyx/config.toml\necho secret=value > .env\n",
        );
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_external_drivers(vec![test_external_driver("cphacker", bin)]);
        let mut mgr = SubagentManager::with_worktree_base_dir(
            policy,
            1,
            Arc::new(|| Box::new(|_: &AgentView| None)),
            Some(&wt_base),
        )
        .with_root_tools(WorkspaceTools::new(WorkspacePolicy::new(&repo).unwrap()));

        let spawned = mgr
            .spawn_job("hacked", Some("cp"), None, 1, Vec::new(), Some("cphacker"))
            .unwrap();
        let s = wait_for_external_job(&mut mgr, &spawned.job_id);
        assert_eq!(s.status, "done");
        for f in [".env", ".onyx/config.toml"] {
            assert!(
                s.touched_files.contains(&f.to_string()),
                "{:?}",
                s.touched_files
            );
        }

        // Preview: negación de plano con las rutas de Control Plane identificadas.
        match mgr.prepare_merge_preview(&spawned.job_id).unwrap() {
            MergePreviewState::ControlPlaneDenied { denied_files } => {
                for f in [".env", ".onyx/config.toml"] {
                    assert!(denied_files.contains(&f.to_string()), "{denied_files:?}");
                }
            }
            other => panic!("esperado ControlPlaneDenied, obtenido {other:?}"),
        }
        // Guarda dura: `merge_job` bloquea incluso sin ticket en caché.
        let err = mgr.merge_job(&spawned.job_id).unwrap_err();
        assert!(err.contains("Control Plane"), "{err}");

        // `root` intacto: el sentinel sigue siendo el original y no aparece `.env`.
        assert_eq!(
            fs::read_to_string(repo.join(".onyx").join("config.toml")).unwrap(),
            "control=original\n"
        );
        assert!(!repo.join(".env").exists());
        // El worktree se conserva para inspección o descarte.
        assert!(
            s.worktree_path.is_dir(),
            "el worktree debe conservarse tras la denegación"
        );

        mgr.discard_job(&spawned.job_id).unwrap();
        assert!(!s.worktree_path.exists());

        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&host_tmp);
    }

    /// `SDD-10` Fase 3 (gobernanza, e2e vía host): un driver externo que toca `sprout.toml`
    /// fuerza `Ask` en la aprobación de `subagent_merge` **aunque la política lo permita
    /// estáticamente**: el merge nunca se materializa y `root` queda en `WaitingApproval`.
    #[test]
    fn external_driver_governance_delta_forces_ask_on_subagent_merge() {
        use onyx_workspace::PermissionRules;

        let repo = init_temp_git_repo("gov_ask");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_govask_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);
        let host_tmp = host_tmp_dir("govask");

        let bin = write_cli_script(
            &host_tmp,
            "gov",
            "@echo off\r\necho name = hacked > sprout.toml\r\n",
            "#!/bin/sh\necho 'name = \"hacked\"' > sprout.toml\n",
        );
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_permissions(PermissionRules {
                allow: vec!["subagent_merge".to_string()],
                ..Default::default()
            })
            .with_external_drivers(vec![test_external_driver("gov", bin)]);
        // La política **permite estáticamente** `subagent_merge`: el `Ask` debe venir de la
        // superficie de gobernanza del delta, no de la regla.
        assert_eq!(
            policy.evaluate_permission_in_mode(
                WorkspaceMode::Build,
                "subagent_merge",
                r#"{"job_id":"gov-1"}"#
            ),
            PermissionDecision::Allow
        );
        let driver_policy = policy.clone();
        let tools = WorkspaceTools::new(policy.clone());

        let cfg = AgentConfig {
            default_max_steps: 192,
            default_timeout_ticks: 512,
            ..AgentConfig::default()
        };
        let (mut host, ws_vocab) = build_host(1, cfg, None, None).unwrap();
        // `auto_approve=false`: `subagent_merge` pasa por el gate de aprobación del spec,
        // de modo que la decisión `Ask`/`Allow` del driver realmente aplica.
        host.spawn(&make_spec("touch governance", false));

        let mgr = SubagentManager::with_worktree_base_dir(
            policy,
            1,
            Arc::new(|| Box::new(|_: &AgentView| None)),
            Some(&wt_base),
        )
        .with_root_tools(tools.clone());
        let handle = SubagentHandle::new(mgr);

        let responder = move |req: &AgentView| {
            static PHASE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let p = PHASE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if p == 0 {
                Some(Reply::call_structured(
                    req.agent,
                    req.request,
                    "spawn_subagent",
                    r#"{"name":"gov","goal":"touch governance","driver":"gov"}"#,
                ))
            } else {
                let last_obs = req
                    .memory
                    .iter()
                    .rev()
                    .find(|(k, _)| k == "observe")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("");
                if last_obs.contains("\"status\":\"done\"") {
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "subagent_merge",
                        r#"{"job_id":"gov-1"}"#,
                    ))
                } else {
                    thread::sleep(Duration::from_millis(120));
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "subagent_status",
                        r#"{"job_id":"gov-1"}"#,
                    ))
                }
            }
        };
        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools.clone())
            .with_mode(WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab);
        let sub_driver = SubagentManagerDriver::new(host.vocab(), handle.clone())
            .with_policy(driver_policy)
            .with_mode_handle(WorkspaceModeHandle::new(WorkspaceMode::Build))
            .wrapping(ws_bridge.wrapping(AutoApproveSpawns {
                vocab: host.vocab(),
                queued: Vec::new(),
                handled: BTreeSet::new(),
            }));
        let mut driver = RecordingBridge::new(host.vocab(), responder).wrapping(sub_driver);

        let mut saw_merge_approval = false;
        for _ in 0..256 {
            host.step(&mut driver);
            if host.pending_approvals().iter().any(|v| {
                v.pending_approval
                    .as_ref()
                    .is_some_and(|p| p.tool == "subagent_merge")
            }) {
                saw_merge_approval = true;
                break;
            }
        }
        assert!(
            saw_merge_approval,
            "`root` debe quedar en `WaitingApproval` esperando la aprobación de `subagent_merge`"
        );
        let root_id = host.agents()[0];
        assert_eq!(
            host.agent_state(root_id),
            Some(AgentState::WaitingApproval),
            "memory: {:?}",
            host.view(root_id).memory
        );
        // El merge **nunca** se materializó: la gobernanza no llegó a `root` y el job sigue unmerged.
        assert!(
            !repo.join("sprout.toml").exists(),
            "el delta de gobernanza no debe materializarse sin aprobación explícita"
        );
        let s = handle.status(Some("gov-1")).unwrap();
        assert_eq!(s[0].status, "done", "job terminado pero sin fusionar");

        handle.discard_job("gov-1").unwrap();
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&host_tmp);
    }

    /// `SDD-10` Fase 3 (replay offline): una ejecución con driver CLI externo se graba en una
    /// `Transcript` y se reproduce con `ReplayBridge` **sin** `SubagentManager` ni CLI:
    /// `fingerprint()` idéntico, 0 subprocessos (el marker no se recrea) y 0 worktrees.
    #[test]
    fn external_driver_run_replays_offline_with_identical_fingerprint() {
        let repo = init_temp_git_repo("drv_replay_ext");
        let wt_base = repo
            .parent()
            .unwrap()
            .join(format!("openonyx_wt_base_drvreplay_{}", std::process::id()));
        let _ = fs::remove_dir_all(&wt_base);
        let host_tmp = host_tmp_dir("drvreplay");

        // El CLI fake escribe en el worktree y un **marker fuera del repo** como prueba
        // inequívoca de que durante el replay el proceso nunca se re-ejecuta.
        let marker = host_tmp.join("ext_cli_marker.txt");
        let _ = fs::remove_file(&marker);
        let bin = write_cli_script(
            &host_tmp,
            "replay",
            &format!(
                "@echo off\r\necho hello > from_ext.txt\r\necho worked\r\necho marker > \"{}\"\r\n",
                marker.to_string_lossy()
            ),
            &format!(
                "#!/bin/sh\necho hello > from_ext.txt\necho worked\necho marker > \"{}\"\n",
                marker.to_string_lossy()
            ),
        );
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_external_drivers(vec![test_external_driver("cli", bin)]);
        let tools = WorkspaceTools::new(policy.clone());

        let cfg = AgentConfig {
            default_max_steps: 192,
            default_timeout_ticks: 512,
            ..AgentConfig::default()
        };
        let (mut host, ws_vocab) = build_host(1, cfg.clone(), None, None).unwrap();
        host.spawn(&make_spec("delegate to external cli", true));

        let mgr = SubagentManager::with_worktree_base_dir(
            policy,
            1,
            Arc::new(|| Box::new(|_: &AgentView| None)),
            Some(&wt_base),
        )
        .with_root_tools(tools.clone());
        let handle = SubagentHandle::new(mgr);

        let phase = Arc::new(Mutex::new(0usize));
        let phase_clone = Arc::clone(&phase);
        let responder = move |req: &AgentView| {
            let mut p = phase_clone.lock().unwrap();
            *p += 1;
            if *p == 1 {
                Some(Reply::call_structured(
                    req.agent,
                    req.request,
                    "spawn_subagent",
                    r#"{"name":"ext","goal":"write from_ext.txt","driver":"cli"}"#,
                ))
            } else {
                let last_obs = req
                    .memory
                    .iter()
                    .rev()
                    .find(|(k, _)| k == "observe")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("");
                if last_obs.contains("\"merged\":true") {
                    Some(Reply::done(
                        req.agent,
                        req.request,
                        "merged from external cli",
                    ))
                } else if last_obs.contains("\"status\":\"done\"") {
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "subagent_merge",
                        r#"{"job_id":"ext-1"}"#,
                    ))
                } else {
                    thread::sleep(Duration::from_millis(120));
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "subagent_status",
                        r#"{"job_id":"ext-1"}"#,
                    ))
                }
            }
        };
        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode(WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab);
        let sub_driver = SubagentManagerDriver::new(host.vocab(), handle).wrapping(
            ws_bridge.wrapping(AutoApproveSpawns {
                vocab: host.vocab(),
                queued: Vec::new(),
                handled: BTreeSet::new(),
            }),
        );
        let mut driver = RecordingBridge::new(host.vocab(), responder).wrapping(sub_driver);

        for _ in 0..512 {
            host.step(&mut driver);
            if !host.agents().is_empty() && host.is_settled() {
                break;
            }
        }
        assert!(host.is_settled(), "la grabación no se asentó");
        let root_id = host.agents()[0];
        assert_eq!(
            host.agent_state(root_id),
            Some(AgentState::Done),
            "memory: {:?}",
            host.view(root_id).memory
        );
        // Grabación real: el CLI se ejecutó (marker) y el merge materializó en `root`.
        assert!(
            marker.exists(),
            "el CLI fake debe haberse ejecutado en la grabación"
        );
        fs::remove_file(&marker).unwrap();
        assert!(
            fs::read_to_string(repo.join("from_ext.txt"))
                .unwrap()
                .contains("hello"),
            "el merge debe haber materializado el cambio del CLI en root"
        );
        let original_fp = host.fingerprint();
        let tr = driver.transcript().clone();
        drop(driver);

        // Tras el merge el worktree se desmontó: 0 worktrees en el directorio base.
        let count_worktrees = || {
            let mut n = 0usize;
            if let Ok(entries) = fs::read_dir(&wt_base) {
                for e in entries.flatten() {
                    if e.path().is_dir() {
                        if let Ok(sub) = fs::read_dir(e.path()) {
                            n += sub
                                .filter_map(|x| x.ok())
                                .filter(|x| x.path().is_dir())
                                .count();
                        }
                    }
                }
            }
            n
        };
        assert_eq!(
            count_worktrees(),
            0,
            "el worktree debe desmontarse tras el merge"
        );

        // Replay offline: host nuevo + `ReplayBridge` solo (sin `SubagentManager` → 0 I/O
        // estructural; sin `SubagentManagerDriver` → la CLI nunca se re-ejecuta).
        let (mut replay_host, _) = build_host(1, cfg, None, None).unwrap();
        replay_host.spawn(&make_spec("delegate to external cli", true));
        let mut replay_driver = ReplayBridge::new(tr).wrapping(HeadlessDriver::new());
        for _ in 0..512 {
            replay_host.step(&mut replay_driver);
            if replay_driver.remaining() == 0 && replay_host.is_settled() {
                break;
            }
        }
        assert_eq!(
            replay_host.fingerprint(),
            original_fp,
            "el replay offline debe reproducir el grafo bit-idéntico"
        );
        assert!(
            !marker.exists(),
            "el replay no debe re-ejecutar el CLI externo (0 subprocessos)"
        );
        assert_eq!(count_worktrees(), 0, "el replay no debe crear worktrees");

        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&host_tmp);
    }

    /// Ticket mínimo para los tests unitarios de [`effective_merge_decision`]: solo
    /// `changed_files` e `impact` influyen en la decisión; el resto son campos inertes.
    fn test_merge_ticket(
        changed_files: Vec<&str>,
        impact: Option<ImpactAssessment>,
    ) -> MergePreviewTicket {
        MergePreviewTicket {
            job_id: "t-1".to_string(),
            approval_key: None,
            root_now_commit: "root-commit".to_string(),
            root_now_tree: "root-tree".to_string(),
            sub_commit: "sub-commit".to_string(),
            merged_tree: "merged-tree".to_string(),
            changed_files: changed_files.into_iter().map(String::from).collect(),
            unified_diff: String::new(),
            impact,
        }
    }

    /// `SDD-10` Fase 4 (fix deadlock de gobernanza): [`effective_merge_decision`] es la única
    /// fuente de verdad compartida por `SubagentManagerDriver` y las superficies TUI/Web/CLI:
    /// un delta que toca gobernanza (`sprout.toml`, `.github/workflows/**`) fuerza `Ask`
    /// **aunque** la política lo permita estáticamente; en el resto delega en
    /// `apply_impact_escalation` (monotonía `Allow ≤ Ask`).
    #[test]
    fn effective_merge_decision_forces_ask_on_governance_and_delegates_to_impact() {
        // Impacto que escala `Allow → Ask` (heat por encima del umbral duro).
        let escalation = ImpactAssessment {
            max_heat_bp: 400,
            callers: 12,
            hops: 4,
            touches_public_src: true,
            hard_threshold_hit: true,
            escalates_allow_to_ask: true,
            epicenters: vec!["src/main.rs".to_string()],
            summary_line: "high impact".to_string(),
        };

        // Gobernanza → `Ask` siempre, aunque la política permita el merge.
        assert_eq!(
            effective_merge_decision(
                PermissionDecision::Allow,
                &test_merge_ticket(vec!["src/a.rs", "sprout.toml"], None)
            ),
            PermissionDecision::Ask
        );
        assert_eq!(
            effective_merge_decision(
                PermissionDecision::Allow,
                &test_merge_ticket(vec![".github/workflows/ci.yml"], None)
            ),
            PermissionDecision::Ask
        );
        // `Ask` ya pedido por la política sigue siendo `Ask`.
        assert_eq!(
            effective_merge_decision(
                PermissionDecision::Ask,
                &test_merge_ticket(vec!["sprout.toml"], None)
            ),
            PermissionDecision::Ask
        );

        // Sin gobernanza → delega en el impacto: `Allow` + escalado → `Ask`.
        assert_eq!(
            effective_merge_decision(
                PermissionDecision::Allow,
                &test_merge_ticket(vec!["src/a.rs"], Some(escalation.clone()))
            ),
            PermissionDecision::Ask
        );
        // Sin gobernanza ni impacto escalado → la decisión estática se conserva.
        assert_eq!(
            effective_merge_decision(
                PermissionDecision::Allow,
                &test_merge_ticket(vec!["src/a.rs"], None)
            ),
            PermissionDecision::Allow
        );
        assert_eq!(
            effective_merge_decision(
                PermissionDecision::Ask,
                &test_merge_ticket(vec!["src/a.rs"], Some(escalation))
            ),
            PermissionDecision::Ask
        );
    }

    /// `SDD-10` Fase 4 (e2e sin turno de root): `/spawn --driver <id> <tarea>` vía
    /// [`apply_session_action`] lanza un job de worktree con driver externo en un host
    /// **fresco** (0 agentes, 0 turnos de `root`): el job es visible en `list_summaries()`
    /// con el `driver` correcto y el tick del host no avanza; fuera de BUILD el gate
    /// bloquea el spawn sin crear job alguno.
    #[test]
    fn slash_spawn_external_driver_runs_without_root_turn() {
        use crate::command::{PendingSessionAction, PendingSystemNotes, SessionAction};
        use crate::session::{SessionActionContext, apply_session_action};

        let repo = init_temp_git_repo("spawn_slash");
        let wt_base = repo.parent().unwrap().join(format!(
            "openonyx_wt_base_spawn_slash_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&wt_base);
        let host_tmp = host_tmp_dir("spawn_slash");

        // CLI fake que crea `from_slash.txt` dentro del worktree (CWD del runner).
        let bin = write_cli_script(
            &host_tmp,
            "spawnfake",
            "@echo off\r\necho spawned > from_slash.txt\r\n",
            "#!/bin/sh\necho spawned > from_slash.txt\n",
        );
        let policy = WorkspacePolicy::new(&repo)
            .unwrap()
            .with_external_drivers(vec![test_external_driver("fake", bin)]);

        // Host fresco: **sin** `host.spawn(make_spec(...))` — `/spawn` no exige turno de root.
        let (mut host, ws_vocab) = build_host(1, AgentConfig::default(), None, None).unwrap();
        let before_ticks = host.ticks();

        let tools = WorkspaceTools::new(policy.clone());
        let mgr = SubagentManager::with_worktree_base_dir(
            policy,
            1,
            Arc::new(|| Box::new(|_: &AgentView| None)),
            Some(&wt_base),
        )
        .with_root_tools(tools.clone());
        let handle = SubagentHandle::new(mgr);

        let ctx = SessionActionContext {
            workspace_root: repo.clone(),
            config: AgentConfig::default(),
            ws_vocab,
            mode_handle: WorkspaceModeHandle::new(WorkspaceMode::Build),
            tools,
            snapshot_store: None,
            subagents: Some(handle.clone()),
            pending_action: PendingSessionAction::new(),
            pending_notes: PendingSystemNotes::new(),
        };

        // Gate: fuera de BUILD el `/spawn` se bloquea y no crea job.
        ctx.mode_handle.set(WorkspaceMode::Plan);
        let blocked = apply_session_action(
            &mut host,
            SessionAction::SpawnSubagent {
                goal: "should be blocked".to_string(),
                driver: None,
            },
            &ctx,
        );
        assert!(
            blocked.contains("Blocked in `PLAN` mode"),
            "spawn must be blocked in PLAN mode, got: {blocked}"
        );
        assert!(
            handle.list_summaries().is_empty(),
            "no job may be created outside BUILD"
        );

        // BUILD: `/spawn --driver fake <tarea>` lanza sin gastar turno de root.
        ctx.mode_handle.set(WorkspaceMode::Build);
        let msg = apply_session_action(
            &mut host,
            SessionAction::SpawnSubagent {
                goal: "Add from_slash.txt".to_string(),
                driver: Some("fake".to_string()),
            },
            &ctx,
        );
        assert!(
            msg.contains("Launched background worktree job"),
            "spawn should launch, got: {msg}"
        );
        assert!(
            msg.contains("on driver `fake`"),
            "message must mention the external driver: {msg}"
        );
        assert_eq!(
            host.ticks(),
            before_ticks,
            "/spawn must not consume a root turn"
        );

        // El job es visible con el driver correcto hasta completar (verdad de Git).
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut finished = None;
        while finished.is_none() {
            for j in handle.list_summaries() {
                if j.status == "done" || j.status == "failed" {
                    finished = Some(j);
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "external job launched via /spawn did not finish in 30 s: {:?}",
                handle.list_summaries()
            );
            thread::sleep(Duration::from_millis(25));
        }
        let s = finished.unwrap();
        assert_eq!(s.driver, "fake");
        assert_eq!(s.status, "done");
        assert_eq!(s.ok, Some(true));
        assert!(
            s.touched_files.iter().any(|f| f == "from_slash.txt"),
            "the fake CLI's file must be captured from Git, got: {:?}",
            s.touched_files
        );

        let _ = handle.discard_job(&s.job_id);
        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&host_tmp);
    }

    /// Crea en el host el script de "stall" (toca un fichero del worktree + nieto vivo que
    /// hereda la pipe de `stdout`) y devuelve su ruta absoluta.
    #[cfg(windows)]
    fn write_stall_script(host_tmp: &Path) -> String {
        let p = host_tmp.join("wt_stall.cmd");
        fs::write(
            &p,
            "@echo off\r\necho marker > touch.txt\r\nping -n 300 127.0.0.1\r\n",
        )
        .unwrap();
        p.to_string_lossy().into_owned()
    }

    #[cfg(not(windows))]
    fn write_stall_script(host_tmp: &Path) -> String {
        let p = host_tmp.join("wt_stall.sh");
        fs::write(
            &p,
            "#!/bin/sh\necho marker > touch.txt\nsleep 300 &\nwait\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }
}
