use std::collections::BTreeSet;

use onyx_soul_agents::{
    EventBus, GraphView, PlatformDriver, Reply, SurfaceBinding, Vocab, pending_tool_calls,
};
use serde_json::Value as JsonValue;

use crate::state::{
    PendingCheckpoints, WorkspaceMode, WorkspaceVocab, capture_git_tree_checkpoint, is_file_read,
    normalize_rel_path,
};
use crate::tools::WorkspaceTools;

/// Driver que observa agentes en `AgentState::Acting` con herramientas del
/// workspace pendientes, valida políticas de modo/lectura previa, las ejecuta
/// bajo `WorkspacePolicy` y devuelve `Reply::observe` / `Reply::tool_error` en el
/// siguiente `poll`.
///
/// Puede envolver otro `PlatformDriver` (`TuiDriver`, `HeadlessDriver`, etc.).
pub struct WorkspaceToolBridge {
    vocab: Vocab,
    ws_vocab: Option<WorkspaceVocab>,
    mode: WorkspaceMode,
    require_read_before_edit: bool,
    checkpoints: Option<PendingCheckpoints>,
    tools: WorkspaceTools,
    inner: Option<Box<dyn PlatformDriver>>,
    dispatched: BTreeSet<(u64, u64)>,
    queued: Vec<Reply>,
}

impl WorkspaceToolBridge {
    pub fn new(vocab: Vocab, tools: WorkspaceTools) -> Self {
        Self {
            vocab,
            ws_vocab: None,
            mode: WorkspaceMode::Build,
            require_read_before_edit: false,
            checkpoints: None,
            tools,
            inner: None,
            dispatched: BTreeSet::new(),
            queued: Vec::new(),
        }
    }

    pub fn with_mode(mut self, mode: WorkspaceMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_workspace_vocab(mut self, ws_vocab: WorkspaceVocab) -> Self {
        self.ws_vocab = Some(ws_vocab);
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

    pub fn wrapping(mut self, inner: impl PlatformDriver + 'static) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

    fn validate_policy(
        &self,
        graph: &dyn GraphView,
        tool: &str,
        args_json: &str,
    ) -> Result<(), String> {
        let is_mutating = matches!(tool, "fs_write" | "fs_edit" | "shell_exec");
        if is_mutating && !self.mode.allows_mutation() {
            return Err(format!(
                "blocked by workspace mode `{:?}`: `{tool}` is not permitted in read-only mode",
                self.mode
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
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
        for (agent, request, pending) in pending_tool_calls(graph, &self.vocab) {
            if !self.dispatched.insert((agent.0, request)) {
                continue;
            }
            let outcome = match self.validate_policy(graph, &pending.tool, &pending.args) {
                Ok(()) => {
                    if matches!(pending.tool.as_str(), "fs_write" | "fs_edit") {
                        if let Some(cp_map) = &self.checkpoints {
                            let parsed: JsonValue =
                                serde_json::from_str(&pending.args).unwrap_or(JsonValue::Null);
                            let path = parsed
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            if let Some((head, tree)) =
                                capture_git_tree_checkpoint(&self.tools.policy().root)
                            {
                                if let Ok(mut guard) = cp_map.lock() {
                                    guard.insert(
                                        (agent.0, pending.tool.clone(), normalize_rel_path(path)),
                                        (head, tree),
                                    );
                                }
                            }
                        }
                    }
                    self.tools.execute(&pending.tool, &pending.args)
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
            self.queued.push(reply);
        }
    }
}
