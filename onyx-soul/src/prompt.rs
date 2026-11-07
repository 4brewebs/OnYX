//! Adaptador de superficie textual / LLM.
//! Implementa `PlatformDriver` proyectando el grafo a texto
//! y traduciendo respuestas del modelo a eventos.

use std::collections::{HashMap, VecDeque};

use onyx_core::bus::EventBus;
use onyx_core::event::{DomainEvent, IngressEvent, OnyxEvent};
use onyx_core::graph::{Attr, ConceptId, Graph, Value};
use onyx_core::surface::{SurfaceBinding, SurfaceId};
use onyx_core::PlatformDriver;

/// Proyector: convierte una vista del grafo en texto.
pub type TextProjector = fn(&dyn Graph, Attr, bool) -> String;

/// Tool call ya parseada, lista para convertirse en `DomainEvent`.
#[derive(Debug, Clone)]
pub struct AgentToolCall {
    pub kind: Attr,
    pub target: Option<ConceptId>,
    pub args: Vec<(Attr, Value)>,
}

/// Driver de prompt / LLM.
/// Vive fuera del core. Solo implementa el puerto.
pub struct PromptDriver {
    viewports: HashMap<SurfaceId, (&'static str, TextProjector)>,
    pending_commands: VecDeque<String>,
    pending_tool_calls: VecDeque<AgentToolCall>,
    last_rendered_prompt: String,
}

impl PromptDriver {
    pub fn new() -> Self {
        Self {
            viewports: HashMap::new(),
            pending_commands: VecDeque::new(),
            pending_tool_calls: VecDeque::new(),
            last_rendered_prompt: String::new(),
        }
    }

    /// Registra un `SurfaceId` que este driver sabe proyectar.
    pub fn register_viewport(
        mut self,
        surface: SurfaceId,
        section_tag: &'static str,
        projector: TextProjector,
    ) -> Self {
        self.viewports.insert(surface, (section_tag, projector));
        self
    }

    /// Encola un comando de texto (entra como `IngressEvent::Command`).
    pub fn push_command(&mut self, cmd: impl Into<String>) {
        self.pending_commands.push_back(cmd.into());
    }

    /// Encola una tool call (entra como `DomainEvent`).
    pub fn push_tool_call(
        &mut self,
        kind: Attr,
        target: Option<ConceptId>,
        args: Vec<(Attr, Value)>,
    ) {
        self.pending_tool_calls.push_back(AgentToolCall {
            kind,
            target,
            args,
        });
    }

    /// Prompt generado en el último `present`.
    pub fn current_prompt(&self) -> &str {
        &self.last_rendered_prompt
    }
}

impl Default for PromptDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformDriver for PromptDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        while let Some(cmd) = self.pending_commands.pop_front() {
            bus.push(OnyxEvent::Ingress(IngressEvent::Command(cmd)));
        }

        while let Some(call) = self.pending_tool_calls.pop_front() {
            bus.push(OnyxEvent::Domain(DomainEvent {
                kind: call.kind,
                subject: call.target,
                data: call.args,
            }));
        }
    }

    fn present(&mut self, graph: &dyn Graph, surfaces: &[SurfaceBinding]) {
        self.last_rendered_prompt.clear();

        for binding in surfaces {
            if let Some((section_tag, projector)) = self.viewports.get(&binding.surface) {
                let focus = if binding.focused { " [FOCUSED]" } else { "" };
                let body = projector(graph, binding.view, binding.focused);

                self.last_rendered_prompt.push_str(&format!(
                    "=== VIEWPORT: {}{} ===\n{}\n\n",
                    section_tag, focus, body
                ));
            }
        }
    }
}