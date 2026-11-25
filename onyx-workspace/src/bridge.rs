use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use onyx_soul_agents::{
    AgentState, Attr, ConceptId, EventBus, GraphView, PlatformDriver, Reply, SurfaceBinding, Value,
    Vocab, pending_approvals, pending_tool_calls,
};
use serde_json::Value as JsonValue;

use crate::policy::{PermissionDecision, is_mutating_tool};
use crate::state::{
    PendingCheckpoints, WorkspaceMode, WorkspaceModeHandle, WorkspaceVocab,
    capture_git_tree_checkpoint, file_read_tick, graph_workspace_mode, is_file_read,
    normalize_rel_path, restore_git_tree_checkpoint_for_targets, undone_effects,
};
use crate::tools::{
    WorkspaceTools, is_async_subagent_tool, spawn_agent_is_isolated,
    spawn_subagent_driver_from_args,
};
use crate::worktree::{
    SubagentNetworkPolicy, SubagentProposal, WorktreeHandle, WorktreeManager, WorktreeMergeOutcome,
};

/// Driver que observa agentes en `AgentState::Acting` con herramientas del
/// workspace pendientes, valida políticas de modo/lectura previa, las ejecuta
/// bajo `WorkspacePolicy` y devuelve `Reply::observe` / `Reply::tool_error` en el
/// siguiente `poll`.
///
/// También materializa en disco las intenciones de `/undo` marcadas en el grafo
/// (`ws.effect.undone`) sin contaminar la reproducción determinista (`ReplayBridge`).
///
/// Puede envolver otro `PlatformDriver` (`TuiDriver`, `HeadlessDriver`, etc.).
pub struct WorkspaceToolBridge {
    vocab: Vocab,
    ws_vocab: Option<WorkspaceVocab>,
    mode: WorkspaceModeHandle,
    require_read_before_edit: bool,
    checkpoints: Option<PendingCheckpoints>,
    tools: WorkspaceTools,
    inner: Option<Box<dyn PlatformDriver>>,
    dispatched: BTreeSet<(u64, u64)>,
    handled_approvals: BTreeSet<(u64, u64)>,
    applied_undos: BTreeSet<u64>,
    queued: Vec<Reply>,
    ready_flag: Option<Arc<AtomicBool>>,
    /// Override explícito del directorio base de *worktrees* aislados (tests herméticos);
    /// `None` → resolución estándar (`ONYX_WT_DIR`, carpeta hermana, temp).
    worktree_base_dir: Option<PathBuf>,
    /// Cache de la inicialización (o fallo) de [`WorktreeManager`] para el root actual.
    worktree_mgr: Option<Result<WorktreeManager, String>>,
    /// Hijos activos de `spawn_agent(isolated: true)` en vuelo: `child_id` → slot con su
    /// *worktree* y sus `WorkspaceTools` re-enraizadas en el *worktree*.
    isolated_slots: BTreeMap<u64, IsolatedSlot>,
    /// Hijos finalizados con resultado 3-way pendiente de decisión (apply/discard/transfer).
    isolated_results: BTreeMap<u64, IsolatedWorktreeResult>,
    /// Hijos cuyo *worktree* no pudo crearse (no-repo-Git, Git < 2.38, …): todas sus
    /// llamadas a herramienta devuelven este error accionable sin tocar `root`.
    isolated_errors: BTreeMap<u64, String>,
    /// Secuencia determinista para `job_id` de *worktrees* aislados.
    isolated_seq: u64,
}

/// Slot de un subagente hijo aislado: su *worktree* y las herramientas re-enraizadas
/// en él (política [`WorkspacePolicy::for_worktree`], red `NativeOffline`, sin MCP).
struct IsolatedSlot {
    handle: WorktreeHandle,
    tools: WorkspaceTools,
}

/// Estado de un *worktree* aislado finalizado a la espera de una decisión (`SDD-05` REQ-05.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolatedWorktreeState {
    /// El *worktree* sigue en disco y puede aplicarse, descartarse o transferirse.
    Pending,
    /// El merge 3-way se aplicó sobre `root`; *worktree* y rama `onyx/sub/<job_id>` eliminados.
    Applied,
    /// *Worktree* y rama eliminados sin aplicar.
    Discarded,
    /// *Worktree* transferido a `SubagentManager` (`subagent_merge`/`subagent_discard` lo poseen).
    Transferred,
}

/// Resultado final de un subagente hijo ejecutado en un *worktree* Git aislado (`SDD-05` REQ-05.5).
///
/// El `outcome` es el preview 3-way calculado en la finalización; puede quedar obsoleto si
/// `root` cambia después — [`WorkspaceToolBridge::apply_isolated_worktree`] recalcula el
/// 3-way fresco sobre el `root` actual antes de aplicar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedWorktreeResult {
    pub child_id: u64,
    pub job_id: String,
    /// *Worktree* en disco (válido hasta aplicar/descartar/transferir).
    pub handle: WorktreeHandle,
    /// Commit efímero con el *working tree* final del hijo (vacío si la captura falló).
    pub sub_commit: String,
    /// `true` si el hijo terminó en `Done`.
    pub ok: bool,
    /// Resultado final del hijo (texto de `v.result`).
    pub summary: String,
    pub outcome: WorktreeMergeOutcome,
    pub changed_files: Vec<String>,
    pub unified_diff: String,
    /// Fallo durante la captura/reconciliación (el *worktree* se conserva para descartar).
    pub error: Option<String>,
    pub state: IsolatedWorktreeState,
}

impl WorkspaceToolBridge {
    pub fn new(vocab: Vocab, tools: WorkspaceTools) -> Self {
        Self {
            vocab,
            ws_vocab: None,
            mode: WorkspaceModeHandle::new(WorkspaceMode::Build),
            require_read_before_edit: false,
            checkpoints: None,
            tools,
            inner: None,
            dispatched: BTreeSet::new(),
            handled_approvals: BTreeSet::new(),
            applied_undos: BTreeSet::new(),
            queued: Vec::new(),
            ready_flag: None,
            worktree_base_dir: None,
            worktree_mgr: None,
            isolated_slots: BTreeMap::new(),
            isolated_results: BTreeMap::new(),
            isolated_errors: BTreeMap::new(),
            isolated_seq: 1,
        }
    }

    /// Override explícito del directorio base de los *worktrees* de subagentes aislados
    /// (tests herméticos). `None` → resolución estándar (`ONYX_WT_DIR` → carpeta hermana
    /// `.onyx-wt` → `temp`), vía [`WorktreeManager::with_base_dir_override`].
    pub fn with_worktree_base_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.worktree_base_dir = dir;
        self
    }

    pub fn with_mode(self, mode: WorkspaceMode) -> Self {
        self.mode.set(mode);
        self
    }

    pub fn with_mode_handle(mut self, mode: WorkspaceModeHandle) -> Self {
        self.mode = mode;
        self
    }

    pub fn mode_handle(&self) -> WorkspaceModeHandle {
        self.mode.clone()
    }

    pub fn with_workspace_vocab(mut self, ws_vocab: WorkspaceVocab) -> Self {
        self.ws_vocab = Some(ws_vocab);
        self
    }

    /// Pre-registra los `/undo` ya aplicados en un grafo restaurado para no
    /// re-ejecutar `git read-tree` sobre efectos antiguos al reanudar sesión.
    pub fn sync_existing_undos<G: GraphView + ?Sized>(mut self, graph: &G) -> Self {
        if let Some(wv) = &self.ws_vocab {
            for eff in undone_effects(graph, wv) {
                self.applied_undos.insert(eff.id.0);
            }
            if let Some(mode) = graph_workspace_mode(graph, wv) {
                self.mode.set(mode);
            }
        }
        self
    }

    pub fn require_read_before_edit(mut self, require: bool) -> Self {
        self.require_read_before_edit = require;
        self
    }

    pub fn with_checkpoints(mut self, checkpoints: PendingCheckpoints) -> Self {
        self.checkpoints = Some(checkpoints);
        self
    }

    /// Señala a un bucle de sesión externo (p. ej. `StopPolicy::Quit`) cuando el puente
    /// tiene respuestas de auto-aprobación/auto-denegación o resultados de herramienta listos.
    pub fn with_ready_flag(mut self, ready_flag: Arc<AtomicBool>) -> Self {
        self.ready_flag = Some(ready_flag);
        self
    }

    pub fn wrapping(mut self, inner: impl PlatformDriver + 'static) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Subagentes aislados en worktrees (`SDD-05` REQ-05.5)
    // ─────────────────────────────────────────────────────────────────────────────

    /// Resultados finalizados de subagentes aislados (clonados), en orden de `child_id`.
    pub fn isolated_results(&self) -> Vec<IsolatedWorktreeResult> {
        self.isolated_results.values().cloned().collect()
    }

    /// Errores pegajosos por hijo: *worktree* que no pudo crearse (no-repo-Git, Git < 2.38…).
    /// Todas las llamadas a herramienta de esos hijos devuelven este error accionable.
    pub fn isolated_errors(&self) -> Vec<(u64, String)> {
        self.isolated_errors
            .iter()
            .map(|(id, reason)| (*id, reason.clone()))
            .collect()
    }

    /// Aplica el 3-way de un *worktree* aislado sobre el **estado actual** de `root`:
    /// recalcula el preview fresco en el momento de aplicar (así la aplicación secuencial de
    /// dos hijos paralelos hace merge limpio), usa `apply_clean_merge_checked` (guarda
    /// anti-TOCTOU + protección de Control Plane) y elimina *worktree* + rama `onyx/sub/<job_id>`.
    /// Devuelve las rutas materializadas en `root` (vacío si `NoOp`).
    pub fn apply_isolated_worktree(&mut self, child_id: u64) -> Result<Vec<String>, String> {
        let (sub_commit, handle) = {
            let result = self
                .isolated_results
                .get(&child_id)
                .filter(|r| r.state == IsolatedWorktreeState::Pending)
                .ok_or_else(|| {
                    format!("no pending isolated worktree for child {child_id} (apply it, discard it or take it first)")
                })?;
            if result.sub_commit.is_empty() {
                return Err(format!(
                    "isolated child {child_id} has no captured commit to apply ({}); discard it with `discard_isolated_worktree`",
                    result.error.as_deref().unwrap_or("capture failed")
                ));
            }
            (result.sub_commit.clone(), result.handle.clone())
        };
        let mgr = self.worktree_manager()?.clone();
        let applied = match mgr.preview_three_way_merge(&handle, &sub_commit)? {
            WorktreeMergeOutcome::NoOp => {
                mgr.remove_worktree(&handle)?;
                Vec::new()
            }
            WorktreeMergeOutcome::Clean {
                root_now_commit,
                root_now_tree,
                merged_tree,
                changed_files,
                ..
            } => {
                let applied =
                    mgr.apply_clean_merge_checked(&root_now_commit, &root_now_tree, &merged_tree)?;
                mgr.remove_worktree(&handle)?;
                if applied.is_empty() {
                    changed_files
                } else {
                    applied
                }
            }
            WorktreeMergeOutcome::Conflict {
                conflicting_files,
                details,
                ..
            } => {
                return Err(format!(
                    "3-way merge conflict for isolated child {child_id} in {conflicting_files:?} (root working tree left untouched; worktree `{}` kept for inspection or `discard_isolated_worktree`):\n{details}",
                    handle.worktree_path.display()
                ));
            }
        };
        if let Some(result) = self.isolated_results.get_mut(&child_id) {
            result.state = IsolatedWorktreeState::Applied;
        }
        Ok(applied)
    }

    /// Descarta el *worktree* de un hijo aislado: elimina *worktree* + rama temporal
    /// `onyx/sub/<job_id>` sin aplicar nada sobre `root`.
    pub fn discard_isolated_worktree(&mut self, child_id: u64) -> Result<(), String> {
        let handle = {
            let result = self
                .isolated_results
                .get(&child_id)
                .filter(|r| r.state == IsolatedWorktreeState::Pending)
                .ok_or_else(|| format!("no pending isolated worktree for child {child_id}"))?;
            result.handle.clone()
        };
        let mgr = self.worktree_manager()?.clone();
        mgr.remove_worktree(&handle)?;
        if let Some(result) = self.isolated_results.get_mut(&child_id) {
            result.state = IsolatedWorktreeState::Discarded;
        }
        Ok(())
    }

    /// Transfiere los *worktrees* finalizados (con commit capturado) a un
    /// `SubagentManager` (`open-onyx`): devuelve `(handle, proposal)` por hijo y marca el
    /// resultado `Transferred`, de modo que este puente ya no limpia ese *worktree*
    /// (lo posee el manager vía `subagent_merge`/`subagent_discard` — sin doble limpieza).
    pub fn take_completed_isolated_worktrees(&mut self) -> Vec<(WorktreeHandle, SubagentProposal)> {
        let pending: Vec<u64> = self
            .isolated_results
            .iter()
            .filter(|(_, r)| r.state == IsolatedWorktreeState::Pending && !r.sub_commit.is_empty())
            .map(|(id, _)| *id)
            .collect();
        let mut out = Vec::with_capacity(pending.len());
        for child_id in pending {
            let Some(result) = self.isolated_results.get_mut(&child_id) else {
                continue;
            };
            let proposal = SubagentProposal {
                job_id: result.job_id.clone(),
                driver: "native".to_string(),
                spawn_tick: result.handle.spawn_tick,
                branch_name: result.handle.branch_name.clone(),
                base_commit: result.handle.base_commit.clone(),
                sub_commit: result.sub_commit.clone(),
                ok: result.ok,
                summary: result.summary.clone(),
                touched_files: result.changed_files.clone(),
            };
            result.state = IsolatedWorktreeState::Transferred;
            out.push((result.handle.clone(), proposal));
        }
        out
    }

    /// Gestor de *worktrees* para el root actual, inicializado con perezas (validación de
    /// repo-Git + Git >= 2.38) y en caché: `Ok` reutilizable, `Err` pegajoso y accionable.
    fn worktree_manager(&mut self) -> Result<&WorktreeManager, String> {
        let entry = self.worktree_mgr.get_or_insert_with(|| {
            WorktreeManager::with_base_dir_override(
                &self.tools.policy().root,
                self.worktree_base_dir.as_deref(),
            )
        });
        entry.as_ref().map_err(|reason| reason.clone())
    }

    /// Sincroniza subagentes aislados (`SDD-05` REQ-05.5) en cada `present`:
    /// - Finaliza de inmediato los slots existentes cuyo hijo es terminal (`Done`/`Failed`)
    ///   o ya no existe en el grafo, **independientemente del estado actual del padre**
    ///   (el padre puede haberse reanudado antes de este `present` y dejar
    ///   `WaitingChildren`/`pending_tool == "spawn_agent"`): sin esto el 3-way se perdería
    ///   y el *worktree* colgaría hasta el `Drop`.
    /// - Después crea perezosamente el *worktree* de cada hijo recién nacido de un
    ///   `spawn_agent(isolated: true)` pendiente (error accionable pegajoso si no es
    ///   repo-Git / Git < 2.38 / fallo de creación, sin tocar `root`).
    fn sync_isolated_worktrees(&mut self, graph: &dyn GraphView) {
        let v = self.vocab;

        // 1) Slots existentes: finalización incondicional si el hijo es terminal o
        //    desapareció del grafo; si sigue activo, se conserva el slot.
        let existing: Vec<(u64, IsolatedSlot)> = std::mem::take(&mut self.isolated_slots)
            .into_iter()
            .collect();
        for (child_id, slot) in existing {
            let child = ConceptId(child_id);
            let raw_state = graph_text(graph, child, v.state);
            let terminal = raw_state
                .as_deref()
                .and_then(AgentState::parse)
                .is_some_and(AgentState::is_terminal);
            if terminal || raw_state.is_none() {
                self.finalize_isolated_child(graph, child, slot);
            } else {
                self.isolated_slots.insert(child_id, slot);
            }
        }

        // 2) Hijos recién nacidos: solo padres `WaitingChildren` con `spawn_agent`
        //    aislado pendiente; se crea slot si el hijo aún no lo tiene (ni resultado
        //    ni error pegajoso).
        let mut parents: Vec<ConceptId> = Vec::new();
        for id in graph.query(&[v.state, v.pending_tool]) {
            let Some(state) = graph_text(graph, id, v.state).and_then(|s| AgentState::parse(&s))
            else {
                continue;
            };
            if state != AgentState::WaitingChildren {
                continue;
            }
            if graph_text(graph, id, v.pending_tool).as_deref() != Some("spawn_agent") {
                continue;
            }
            let args = graph_text(graph, id, v.pending_args).unwrap_or_default();
            if spawn_agent_is_isolated(&args) {
                parents.push(id);
            }
        }
        if parents.is_empty() {
            return;
        }

        let agents: Vec<ConceptId> = graph.query(&[v.state]);
        for parent in parents {
            for child in &agents {
                if !graph.related(*child, v.rel_parent).contains(&parent) {
                    continue;
                }
                let child = *child;
                if self.isolated_slots.contains_key(&child.0)
                    || self.isolated_results.contains_key(&child.0)
                    || self.isolated_errors.contains_key(&child.0)
                {
                    continue;
                }
                let terminal = graph_text(graph, child, v.state)
                    .and_then(|s| AgentState::parse(&s))
                    .is_some_and(AgentState::is_terminal);
                if terminal {
                    // Primera observación ya terminal: no se creó *worktree*, no hay nada que hacer.
                    continue;
                }
                self.create_isolated_slot(graph, parent, child);
            }
        }
    }

    /// Crea el *worktree* de un hijo aislado recién nacido y sus herramientas re-enraizadas.
    fn create_isolated_slot(&mut self, graph: &dyn GraphView, parent: ConceptId, child: ConceptId) {
        let v = self.vocab;
        let job_id = format!("agent-{}-{}-{}", parent.0, child.0, self.isolated_seq);
        let spawn_tick = match graph.get(child, v.born) {
            Some(Value::Int(born)) => u64::try_from(born).unwrap_or(0),
            _ => 0,
        };
        self.isolated_seq += 1;

        let mgr = match self.worktree_manager() {
            Ok(m) => m.clone(),
            Err(reason) => {
                self.isolated_errors.insert(child.0, reason);
                return;
            }
        };
        let handle = match mgr.create_worktree(&job_id, spawn_tick) {
            Ok(h) => h,
            Err(reason) => {
                self.isolated_errors.insert(child.0, reason);
                return;
            }
        };
        let policy = match self
            .tools
            .policy()
            .for_worktree(&handle.worktree_path, &SubagentNetworkPolicy::NativeOffline)
        {
            Ok(p) => p,
            Err(e) => {
                let _ = mgr.remove_worktree(&handle);
                self.isolated_errors
                    .insert(child.0, format!("failed to derive worktree policy: {e}"));
                return;
            }
        };
        let mut child_tools = WorkspaceTools::new(policy);
        if let Some(flag) = self.tools.cancel_flag() {
            child_tools = child_tools.with_cancel_flag(flag);
        }
        self.isolated_slots.insert(
            child.0,
            IsolatedSlot {
                handle,
                tools: child_tools,
            },
        );
    }

    /// Finaliza un hijo aislado terminal: captura el commit del *worktree*, evalúa el 3-way
    /// fresco y deja el resultado `Pending` (o retira el *worktree* si no hubo cambios).
    fn finalize_isolated_child(
        &mut self,
        graph: &dyn GraphView,
        child: ConceptId,
        slot: IsolatedSlot,
    ) {
        let v = self.vocab;
        let ok = graph_text(graph, child, v.state).as_deref() == Some(AgentState::Done.as_str());
        let summary = graph_text(graph, child, v.result).unwrap_or_default();
        let job_id = slot.handle.job_id.clone();
        let handle = slot.handle;

        let mut sub_commit = String::new();
        let mut changed_files = Vec::new();
        let mut unified_diff = String::new();
        let mut outcome = WorktreeMergeOutcome::NoOp;
        let mut error: Option<String> = None;
        match self.worktree_manager() {
            Ok(mgr) => match mgr.capture_subagent_commit(&handle) {
                Ok((sub, _tree)) => {
                    sub_commit = sub;
                    changed_files = mgr
                        .changed_files_between(&handle.base_commit, &sub_commit)
                        .unwrap_or_default();
                    match mgr.preview_three_way_merge(&handle, &sub_commit) {
                        Ok(o @ WorktreeMergeOutcome::Clean { .. }) => {
                            if let WorktreeMergeOutcome::Clean {
                                unified_diff: ud, ..
                            } = &o
                            {
                                unified_diff = ud.clone();
                            }
                            outcome = o;
                        }
                        Ok(other) => outcome = other,
                        Err(e) => error = Some(format!("3-way preview failed: {e}")),
                    }
                }
                Err(e) => error = Some(format!("failed to capture subagent commit: {e}")),
            },
            Err(reason) => error = Some(format!("worktree manager unavailable: {reason}")),
        }

        self.isolated_slots.remove(&child.0);
        if matches!(outcome, WorktreeMergeOutcome::NoOp) && error.is_none() {
            // Sin cambios respecto al base: no hay que aplicar nada, se retira ya.
            if let Ok(mgr) = self.worktree_manager() {
                let _ = mgr.remove_worktree(&handle);
            }
            return;
        }
        self.isolated_results.insert(
            child.0,
            IsolatedWorktreeResult {
                child_id: child.0,
                job_id,
                handle,
                sub_commit,
                ok,
                summary,
                outcome,
                changed_files,
                unified_diff,
                error,
                state: IsolatedWorktreeState::Pending,
            },
        );
    }

    fn current_mode(&self, graph: &dyn GraphView) -> WorkspaceMode {
        self.ws_vocab
            .as_ref()
            .and_then(|wv| graph_workspace_mode(graph, wv))
            .unwrap_or_else(|| self.mode.get())
    }

    fn push_reply(&mut self, reply: Reply) {
        self.queued.push(reply);
        if let Some(flag) = &self.ready_flag {
            flag.store(true, Ordering::Release);
        }
    }

    fn validate_policy(
        &self,
        graph: &dyn GraphView,
        tool: &str,
        args_json: &str,
    ) -> Result<(), String> {
        self.validate_policy_with_tools(graph, &self.tools, tool, args_json)
    }

    /// Misma validación que [`Self::validate_policy`] pero contra las herramientas de un
    /// *worktree* aislado (hijos de `spawn_agent(isolated: true)`).
    fn validate_policy_with_tools(
        &self,
        graph: &dyn GraphView,
        tools: &WorkspaceTools,
        tool: &str,
        args_json: &str,
    ) -> Result<(), String> {
        let current_mode = self.current_mode(graph);
        if (is_mutating_tool(tool) || tool.starts_with("mcp__")) && !current_mode.allows_mutation()
        {
            return Err(format!(
                "blocked by workspace mode `{:?}`: `{tool}` is not permitted in read-only mode",
                current_mode
            ));
        }

        if tools.policy().evaluate_permission(tool, args_json) == PermissionDecision::Deny {
            return Err(format!(
                "blocked by permission policy (deny rule): `{tool}`"
            ));
        }

        if tool == "fs_edit" && self.require_read_before_edit {
            if let Some(wv) = &self.ws_vocab {
                let parsed: JsonValue = serde_json::from_str(args_json).unwrap_or(JsonValue::Null);
                let path = parsed
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !is_file_read(graph, wv, path) {
                    return Err(format!(
                        "policy violation: must read `{path}` with `fs_read` before calling `fs_edit`"
                    ));
                }
            }
        }

        Ok(())
    }
}

impl PlatformDriver for WorkspaceToolBridge {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Some(inner) = self.inner.as_mut() {
            inner.poll(bus);
        }
        for reply in self.queued.drain(..) {
            bus.push(reply.to_event());
        }
        if let Some(flag) = &self.ready_flag {
            flag.store(false, Ordering::Release);
        }
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        if let Some(wv) = &self.ws_vocab {
            if let Some(mode) = graph_workspace_mode(graph, wv) {
                self.mode.set(mode);
            }
            for eff in undone_effects(graph, wv) {
                if self.applied_undos.insert(eff.id.0) {
                    if let Some(tree_sha) = eff.tree_sha.as_deref() {
                        let created: Vec<&str> = if eff.tool == "fs_write" {
                            vec![eff.target.as_str()]
                        } else {
                            Vec::new()
                        };
                        let _ = restore_git_tree_checkpoint_for_targets(
                            &self.tools.policy().root,
                            tree_sha,
                            &created,
                        );
                        self.tools.clear_seen_reads();
                    }
                }
            }
        }

        // Sincroniza *worktrees* de subagentes aislados (`SDD-05` REQ-05.5): creación
        // perezosa de hijos nuevos y finalización (captura + 3-way) de hijos terminales.
        self.sync_isolated_worktrees(graph);

        // Evalúa aprobaciones pendientes frente a `WorkspaceMode`, `PermissionRules`
        // (`deny > ask > allow`) y escalado monótono *Scent-Heated Approval* (`Allow -> Ask`)
        // antes de delegar la interacción humana al driver interno.
        let current_mode = self.current_mode(graph);
        for view in pending_approvals(graph, &self.vocab) {
            let Some(pending) = &view.pending_approval else {
                continue;
            };

            // Hijos aislados: error pegajoso (sin *worktree*) → `Deny` accionable.
            if let Some(reason) = self.isolated_errors.get(&view.agent.0) {
                if self.handled_approvals.insert((view.agent.0, view.request)) {
                    self.push_reply(Reply::deny(
                        view.agent,
                        view.request,
                        format!("isolated subagent worktree unavailable: {reason}"),
                    ));
                }
                continue;
            }
            // Hijos aislados en vuelo: se evalúan contra las herramientas de su *worktree*.
            if let Some(slot) = self.isolated_slots.get(&view.agent.0) {
                let (decision, _impact) = slot.tools.evaluate_permission_with_impact(
                    current_mode,
                    &pending.tool,
                    &pending.args,
                );
                if pending.tool == "subagent_merge" && decision != PermissionDecision::Deny {
                    continue;
                }
                match decision {
                    PermissionDecision::Allow => {
                        if self.handled_approvals.insert((view.agent.0, view.request)) {
                            self.push_reply(Reply::approve(view.agent, view.request));
                        }
                    }
                    PermissionDecision::Deny => {
                        let reason = if (is_mutating_tool(&pending.tool)
                            || pending.tool.starts_with("mcp__"))
                            && !current_mode.allows_mutation()
                        {
                            format!(
                                "blocked by workspace mode `{:?}`: `{}` is not permitted in read-only mode",
                                current_mode, pending.tool
                            )
                        } else if slot
                            .tools
                            .policy()
                            .evaluate_permission(&pending.tool, &pending.args)
                            == PermissionDecision::Deny
                        {
                            format!(
                                "blocked by permission policy (deny rule): `{}`",
                                pending.tool
                            )
                        } else {
                            format!(
                                "blocked by control-plane protection (read-only): `{}`",
                                pending.tool
                            )
                        };
                        if self.handled_approvals.insert((view.agent.0, view.request)) {
                            self.push_reply(Reply::deny(view.agent, view.request, reason));
                        }
                    }
                    PermissionDecision::Ask => {}
                }
                continue;
            }

            let (decision, _impact) = self.tools.evaluate_permission_with_impact(
                current_mode,
                &pending.tool,
                &pending.args,
            );
            // Para `subagent_merge`, solo hacemos cumplir `Deny` aquí (p. ej. modo PLAN o regla deny).
            // Nunca auto-aprobamos `Allow`/`Ask` aquí: `SubagentManagerDriver` evalúa el diff
            // 3-way real con `onyx-scent` y sella el ticket anti-TOCTOU antes de aprobar.
            if pending.tool == "subagent_merge" && decision != PermissionDecision::Deny {
                continue;
            }
            match decision {
                PermissionDecision::Allow => {
                    if self.handled_approvals.insert((view.agent.0, view.request)) {
                        self.push_reply(Reply::approve(view.agent, view.request));
                    }
                }
                PermissionDecision::Deny => {
                    if self.handled_approvals.insert((view.agent.0, view.request)) {
                        let reason = if (is_mutating_tool(&pending.tool)
                            || pending.tool.starts_with("mcp__"))
                            && !current_mode.allows_mutation()
                        {
                            format!(
                                "blocked by workspace mode `{:?}`: `{}` is not permitted in read-only mode",
                                current_mode, pending.tool
                            )
                        } else if self
                            .tools
                            .policy()
                            .evaluate_permission(&pending.tool, &pending.args)
                            == PermissionDecision::Deny
                        {
                            format!(
                                "blocked by permission policy (deny rule): `{}`",
                                pending.tool
                            )
                        } else if let Some(driver) = pending
                            .tool
                            .eq("spawn_subagent")
                            .then(|| spawn_subagent_driver_from_args(&pending.args))
                            .flatten()
                            .filter(|d| !d.eq_ignore_ascii_case("native"))
                        {
                            // Gate de drivers externos (`SDD-10`): modo de solo lectura,
                            // workspace no confiado o regla `deny`/`ask`.
                            if !current_mode.allows_mutation() {
                                format!(
                                    "blocked by workspace mode `{:?}`: external harness driver `{driver}` cannot run CLI processes in read-only mode",
                                    current_mode
                                )
                            } else {
                                let (_, gate_reason) =
                                    self.tools.policy().external_driver_gate(&driver);
                                gate_reason
                            }
                        } else {
                            format!(
                                "blocked by control-plane protection (read-only): `{}`",
                                pending.tool
                            )
                        };
                        self.push_reply(Reply::deny(view.agent, view.request, reason));
                    }
                }
                PermissionDecision::Ask => {}
            }
        }

        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
        for (agent, request, pending) in pending_tool_calls(graph, &self.vocab) {
            if pending.tool == "spawn_agent" || is_async_subagent_tool(&pending.tool) {
                continue;
            }
            if !self.dispatched.insert((agent.0, request)) {
                continue;
            }

            // Hijo aislado cuyo *worktree* no pudo crearse: error accionable pegajoso
            // sin tocar `root` (`SDD-05` REQ-05.5).
            if let Some(reason) = self.isolated_errors.get(&agent.0) {
                let mut reply = Reply::tool_error(
                    agent,
                    request,
                    format!("isolated subagent worktree unavailable: {reason}"),
                );
                if let Some(cid) = pending.call_id {
                    reply = reply.with_call_id(cid);
                }
                self.push_reply(reply);
                continue;
            }
            // Hijo aislado en vuelo: valida y ejecuta contra las herramientas de su
            // *worktree* (sin checkpoints de `root` ni `tick_hint` de fs_read).
            if let Some(slot) = self.isolated_slots.get(&agent.0) {
                let outcome = match self.validate_policy_with_tools(
                    graph,
                    &slot.tools,
                    &pending.tool,
                    &pending.args,
                ) {
                    Ok(()) => {
                        let captured_symbols =
                            if matches!(pending.tool.as_str(), "fs_write" | "fs_edit") {
                                slot.tools
                                    .assess_mutation_impact(&pending.tool, &pending.args)
                                    .map(|a| a.epicenters)
                                    .unwrap_or_default()
                            } else {
                                Vec::new()
                            };
                        slot.tools
                            .execute_with_tick(&pending.tool, &pending.args, None)
                            .map(|mut out| {
                                if !captured_symbols.is_empty() {
                                    out.push_str(&format!(
                                        " [symbols:{}]",
                                        captured_symbols.join(",")
                                    ));
                                }
                                out
                            })
                    }
                    Err(reason) => Err(reason),
                };
                let mut reply = match outcome {
                    Ok(out) => Reply::observe(agent, request, out),
                    Err(err) => Reply::tool_error(agent, request, err),
                };
                if let Some(cid) = pending.call_id {
                    reply = reply.with_call_id(cid);
                }
                self.push_reply(reply);
                continue;
            }

            let outcome = match self.validate_policy(graph, &pending.tool, &pending.args) {
                Ok(()) => {
                    let captured_ckpt = if matches!(pending.tool.as_str(), "fs_write" | "fs_edit") {
                        let ckpt = capture_git_tree_checkpoint(&self.tools.policy().root);
                        if let (Some((head, tree)), Some(cp_map)) = (&ckpt, &self.checkpoints) {
                            let parsed: JsonValue =
                                serde_json::from_str(&pending.args).unwrap_or(JsonValue::Null);
                            let path = parsed
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            if let Ok(mut guard) = cp_map.lock() {
                                guard.insert(
                                    (agent.0, pending.tool.clone(), normalize_rel_path(path)),
                                    (head.clone(), tree.clone()),
                                );
                            }
                        }
                        ckpt
                    } else {
                        None
                    };
                    let tick_hint = if pending.tool == "fs_read" {
                        self.ws_vocab.as_ref().and_then(|wv| {
                            let parsed: JsonValue =
                                serde_json::from_str(&pending.args).unwrap_or(JsonValue::Null);
                            let path = parsed.get("path").and_then(|v| v.as_str())?;
                            file_read_tick(graph, wv, path).and_then(|t| u64::try_from(t).ok())
                        })
                    } else {
                        None
                    };
                    let captured_symbols =
                        if matches!(pending.tool.as_str(), "fs_write" | "fs_edit") {
                            self.tools
                                .assess_mutation_impact(&pending.tool, &pending.args)
                                .map(|a| a.epicenters)
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                    self.tools
                        .execute_with_tick(&pending.tool, &pending.args, tick_hint)
                        .map(|mut out| {
                            if let Some((head, tree)) = captured_ckpt {
                                out.push_str(&format!(" [checkpoint:{head}:{tree}]"));
                            }
                            if !captured_symbols.is_empty() {
                                out.push_str(&format!(" [symbols:{}]", captured_symbols.join(",")));
                            }
                            out
                        })
                }
                Err(reason) => Err(reason),
            };
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

impl Drop for WorkspaceToolBridge {
    fn drop(&mut self) {
        // Red de limpieza: retira en best-effort los *worktrees* que este puente aún posee
        // (hijos en vuelo y resultados `Pending`). Los `Applied`/`Discarded` ya fueron
        // retirados y los `Transferred` los posee `SubagentManager` (sin doble limpieza).
        //
        // Sin *worktrees* poseídos **no** se invoca `worktree_manager()`: su inicialización
        // perezosa crea el directorio base (p. ej. la carpeta hermana `.onyx-wt`), y ese
        // efecto colateral no debe producirse en puentes sin actividad aislada (p. ej. la
        // bridge de cada job nativo, enraizada en su propio worktree desechable).
        let has_owned_worktrees = !self.isolated_slots.is_empty()
            || self
                .isolated_results
                .values()
                .any(|r| r.state == IsolatedWorktreeState::Pending);
        if !has_owned_worktrees {
            return;
        }
        let Ok(mgr) = self.worktree_manager().cloned() else {
            return;
        };
        let handles: Vec<WorktreeHandle> = self
            .isolated_slots
            .values()
            .map(|slot| slot.handle.clone())
            .chain(
                self.isolated_results
                    .values()
                    .filter(|r| r.state == IsolatedWorktreeState::Pending)
                    .map(|r| r.handle.clone()),
            )
            .collect();
        for handle in handles {
            let _ = mgr.remove_worktree(&handle);
        }
    }
}

/// Lee una propiedad `Text` de un concepto (o `None` si no existe / no es `Text`).
fn graph_text(graph: &dyn GraphView, id: ConceptId, attr: Attr) -> Option<String> {
    match graph.get(id, attr)? {
        Value::Text(s) => Some(s),
        _ => None,
    }
}
