//! Vocabulario del dominio de agentes.
//!
//! Los agentes no tienen tipos propios en el core: son conceptos del grafo EAV
//! con estos atributos y relaciones. `Vocab` resuelve los nombres a `Attr` una
//! sola vez por registro (en `OnyxSystem::init`) y es `Copy`.
//!
//! Todos los nombres cuelgan de un prefijo configurable (`AgentConfig::vocab_prefix`).
//! El modelo se extiende sin tocar este módulo: una app añade atributos
//! propios a un agente con `AgentSpec::with_extra`.

use onyx_core::{Attr, AttrRegistry};

use crate::config::DEFAULT_PREFIX;

/// Estado de ejecución de un agente. Vive en el grafo como `Text`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    /// Listo para decidir su siguiente paso.
    Idle,
    /// Espera una decisión externa (la petición está proyectada en el grafo).
    Thinking,
    /// Espera el resultado de una herramienta.
    Acting,
    /// Terminó con éxito. Estado terminal.
    Done,
    /// Terminó con error (timeout, presupuesto, fallo explícito). Terminal.
    Failed,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Thinking => "thinking",
            Self::Acting => "acting",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "idle" => Self::Idle,
            "thinking" => Self::Thinking,
            "acting" => Self::Acting,
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// Atributos, relaciones y claves de eventos ya resueltos contra un registro.
#[derive(Copy, Clone, Debug)]
pub struct Vocab {
    // TODO(zip, pasos 3-6): `policy`, `pending`, `remaining`, `mem_count`, los
    // `mem_*`, `ok` y la relación `rel_memory` ya se registran y el spawner
    // inicializa las propiedades, pero aún no las lee ningún sistema; entran con
    // `policy`, `tools` y el runtime del zip.

    // Propiedades del concepto agente.
    pub name: Attr,
    pub goal: Attr,
    pub state: Attr,
    pub policy: Attr,
    pub steps: Attr,
    pub max_steps: Attr,
    pub pending: Attr,
    pub remaining: Attr,
    pub timeout: Attr,
    pub born: Attr,
    pub result: Attr,
    pub mem_count: Attr,
    /// Solo viaja en el evento de spawn: nombres de herramienta separados por coma.
    pub tools_csv: Attr,

    // Relaciones salientes del agente.
    pub rel_tool: Attr,
    pub rel_memory: Attr,
    pub rel_parent: Attr,

    // Conceptos herramienta y memoria.
    pub tool_name: Attr,
    pub mem_seq: Attr,
    pub mem_kind: Attr,
    pub mem_text: Attr,
    pub mem_tick: Attr,

    // `DomainEvent::kind`.
    pub ev_spawn: Attr,
    pub ev_spawned: Attr,
    pub ev_rejected: Attr,
    pub ev_tool_call: Attr,
    pub ev_tool_result: Attr,
    pub ev_finished: Attr,
    pub ev_failed: Attr,

    // Claves de `DomainEvent::data`.
    pub request: Attr,
    pub tool: Attr,
    pub args: Attr,
    pub text: Attr,
    pub reason: Attr,
    /// `Bool` en `ev.tool_result`: `false` si la herramienta falló.
    pub ok: Attr,
}

impl Vocab {
    /// Con el prefijo por defecto (`agent`).
    pub fn resolve(attrs: &mut dyn AttrRegistry) -> Self {
        Self::resolve_with(attrs, DEFAULT_PREFIX)
    }

    /// Registra (o recupera) todos los nombres bajo `prefix`. Idempotente.
    pub fn resolve_with(attrs: &mut dyn AttrRegistry, prefix: &str) -> Self {
        let mut a = |name: &str| attrs.attr(&format!("{prefix}.{name}"));
        Self {
            name: a("name"),
            goal: a("goal"),
            state: a("state"),
            policy: a("policy"),
            steps: a("steps"),
            max_steps: a("max_steps"),
            pending: a("pending"),
            remaining: a("remaining"),
            timeout: a("timeout"),
            born: a("born"),
            result: a("result"),
            mem_count: a("mem_count"),
            tools_csv: a("tools_csv"),

            rel_tool: a("has_tool"),
            rel_memory: a("has_memory"),
            rel_parent: a("parent"),

            tool_name: a("tool.name"),
            mem_seq: a("mem.seq"),
            mem_kind: a("mem.kind"),
            mem_text: a("mem.text"),
            mem_tick: a("mem.tick"),

            ev_spawn: a("ev.spawn"),
            ev_spawned: a("ev.spawned"),
            ev_rejected: a("ev.spawn_rejected"),
            ev_tool_call: a("ev.tool_call"),
            ev_tool_result: a("ev.tool_result"),
            ev_finished: a("ev.finished"),
            ev_failed: a("ev.failed"),

            request: a("request"),
            tool: a("tool"),
            args: a("args"),
            text: a("text"),
            reason: a("reason"),
            ok: a("ok"),
        }
    }

    /// Atributos que gestiona el framework. Una propiedad extra de la app con
    /// uno de estos nombres se ignora: la app no puede falsear el estado.
    pub fn is_reserved(&self, attr: Attr) -> bool {
        [
            self.name,
            self.goal,
            self.state,
            self.policy,
            self.steps,
            self.max_steps,
            self.pending,
            self.remaining,
            self.timeout,
            self.born,
            self.result,
            self.mem_count,
            self.tools_csv,
        ]
        .contains(&attr)
    }
}
