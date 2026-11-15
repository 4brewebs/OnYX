use std::collections::BTreeSet;

use onyx_soul_agents::{
    EventBus, GraphView, PlatformDriver, Reply, SurfaceBinding, Vocab, pending_tool_calls,
};

use crate::tools::WorkspaceTools;

/// Driver que observa agentes en `AgentState::Acting` con herramientas del
/// workspace pendientes, las ejecuta bajo `WorkspacePolicy` y devuelve
/// `Reply::observe` / `Reply::tool_error` en el siguiente `poll`.
///
/// Puede envolver otro `PlatformDriver` (`TuiDriver`, `HeadlessDriver`, etc.).
pub struct WorkspaceToolBridge {
    vocab: Vocab,
    tools: WorkspaceTools,
    inner: Option<Box<dyn PlatformDriver>>,
    dispatched: BTreeSet<(u64, u64)>,
    queued: Vec<Reply>,
}

impl WorkspaceToolBridge {
    pub fn new(vocab: Vocab, tools: WorkspaceTools) -> Self {
        Self {
            vocab,
            tools,
            inner: None,
            dispatched: BTreeSet::new(),
            queued: Vec::new(),
        }
    }

    pub fn wrapping(mut self, inner: impl PlatformDriver + 'static) -> Self {
        self.inner = Some(Box::new(inner));
        self
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
            let mut reply = match self.tools.execute(&pending.tool, &pending.args) {
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
