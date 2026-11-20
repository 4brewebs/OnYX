use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use onyx_soul_agents::{
    EventBus, GraphView, PlatformDriver, Reply, SurfaceBinding, Vocab, pending_approvals,
    pending_tool_calls,
};
use serde_json::Value as JsonValue;

use crate::policy::{PermissionDecision, is_mutating_tool};
use crate::state::{
    PendingCheckpoints, WorkspaceMode, WorkspaceModeHandle, WorkspaceVocab,
    capture_git_tree_checkpoint, file_read_tick, graph_workspace_mode, is_file_read,
    normalize_rel_path, restore_git_tree_checkpoint_for_targets, undone_effects,
};
use crate::tools::WorkspaceTools;

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
        }
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
        let current_mode = self.current_mode(graph);
        if is_mutating_tool(tool) && !current_mode.allows_mutation() {
            return Err(format!(
                "blocked by workspace mode `{:?}`: `{tool}` is not permitted in read-only mode",
                current_mode
            ));
        }

        if self.tools.policy().evaluate_permission(tool, args_json) == PermissionDecision::Deny {
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

        // Evalúa aprobaciones pendientes frente a `WorkspaceMode` y `PermissionRules`
        // (`deny > ask > allow`) antes de delegar la interacción humana al driver interno.
        let current_mode = self.current_mode(graph);
        for view in pending_approvals(graph, &self.vocab) {
            let Some(pending) = &view.pending_approval else {
                continue;
            };
            let decision = self.tools.policy().evaluate_permission_in_mode(
                current_mode,
                &pending.tool,
                &pending.args,
            );
            match decision {
                PermissionDecision::Allow => {
                    if self.handled_approvals.insert((view.agent.0, view.request)) {
                        self.push_reply(Reply::approve(view.agent, view.request));
                    }
                }
                PermissionDecision::Deny => {
                    if self.handled_approvals.insert((view.agent.0, view.request)) {
                        let reason = if is_mutating_tool(&pending.tool)
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
            if pending.tool == "spawn_agent" {
                continue;
            }
            if !self.dispatched.insert((agent.0, request)) {
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
                    self.tools
                        .execute_with_tick(&pending.tool, &pending.args, tick_hint)
                        .map(|mut out| {
                            if let Some((head, tree)) = captured_ckpt {
                                out.push_str(&format!(" [checkpoint:{head}:{tree}]"));
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
