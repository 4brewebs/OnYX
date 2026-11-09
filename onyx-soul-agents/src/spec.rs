//! Descripción de un agente y el evento que lo crea.

use onyx_core::{Attr, ConceptId, OnyxEvent, Value};

use crate::util::domain;
use crate::vocab::Vocab;

/// Quién decide el siguiente paso de un agente.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Decide un driver externo (por ejemplo un LLM). El cerebro solo ve la respuesta.
    External,
    /// Decide el propio cerebro, con `DetRng`: agentes de simulación.
    Scripted,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Scripted => "scripted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "external" => Some(Self::External),
            "scripted" => Some(Self::Scripted),
            _ => None,
        }
    }
}

/// Datos para crear un agente. Los plazos se miden en ticks, nunca en tiempo real.
#[derive(Clone, Debug)]
pub struct AgentSpec {
    pub name: String,
    pub goal: String,
    pub policy: Policy,
    /// Nombres de herramienta. No pueden contener comas.
    pub tools: Vec<String>,
    /// Presupuesto de decisiones. `None` usa `AgentConfig::default_max_steps`.
    pub max_steps: Option<i64>,
    /// Ticks de espera. `None` usa `AgentConfig::default_timeout_ticks`.
    pub timeout_ticks: Option<i64>,
    /// Propiedades propias de la app. Se copian al concepto agente tal cual,
    /// salvo las que el framework gestiona (`Vocab::is_reserved`).
    pub extra: Vec<(Attr, Value)>,
}

impl AgentSpec {
    pub fn new(name: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            goal: goal.into(),
            policy: Policy::External,
            tools: Vec::new(),
            max_steps: None,
            timeout_ticks: None,
            extra: Vec::new(),
        }
    }

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_max_steps(mut self, max_steps: i64) -> Self {
        self.max_steps = Some(max_steps);
        self
    }

    pub fn with_timeout_ticks(mut self, ticks: i64) -> Self {
        self.timeout_ticks = Some(ticks);
        self
    }

    /// Añade una propiedad propia de la app (ver `AgentConfig::attr`).
    pub fn with_extra(mut self, attr: Attr, value: Value) -> Self {
        self.extra.push((attr, value));
        self
    }
}

/// Construye el evento de dominio que pide crear un agente.
///
/// Se entrega por el bus como cualquier otro evento (el llamador lo empuja, o
/// lo emite un sistema). Si `parent` existe, el agente nuevo queda enlazado a él.
pub fn spawn_event(vocab: &Vocab, spec: &AgentSpec, parent: Option<ConceptId>) -> OnyxEvent {
    let mut data = vec![
        (vocab.name, Value::Text(spec.name.clone())),
        (vocab.goal, Value::Text(spec.goal.clone())),
        (vocab.policy, Value::Text(spec.policy.as_str().into())),
        (vocab.tools_csv, Value::Text(spec.tools.join(","))),
    ];
    if let Some(n) = spec.max_steps {
        data.push((vocab.max_steps, Value::Int(n)));
    }
    if let Some(n) = spec.timeout_ticks {
        data.push((vocab.timeout, Value::Int(n)));
    }
    data.extend(spec.extra.iter().cloned());
    domain(vocab.ev_spawn, parent, data)
}
