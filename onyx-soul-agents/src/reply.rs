//! Respuestas de un decisor externo, normalizadas como `IngressEvent::Raw`.
//!
//! Un LLM (o un humano, o un script) no es determinista. Su salida entra al
//! cerebro solo como este evento ya fijado; lo que se graba y se reproduce es
//! la respuesta, no la llamada.

use onyx_core::{ConceptId, IngressEvent, OnyxEvent};

use crate::policy::Decision;

/// Valor de `IngressEvent::Raw::source` para respuestas de agentes.
/// Es el nombre de un canal de entrada, no del vocabulario del grafo:
/// no depende de `vocab_prefix`.
pub const REPLY_SOURCE: &str = "agent.reply";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Anotar un pensamiento o mensaje en la memoria y volver a decidir.
    Say,
    /// Llamar a una herramienta. Payload: `herramienta argumentos...`.
    Call,
    /// Resultado de la herramienta pedida (vía externa).
    Observe,
    /// La herramienta pedida falló. Payload: motivo.
    ToolError,
    /// Terminar con éxito. Payload: resultado.
    Done,
    /// Terminar con error. Payload: motivo.
    Fail,
}

impl Verb {
    fn as_str(self) -> &'static str {
        match self {
            Self::Say => "say",
            Self::Call => "call",
            Self::Observe => "observe",
            Self::ToolError => "tool_error",
            Self::Done => "done",
            Self::Fail => "fail",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "say" => Self::Say,
            "call" => Self::Call,
            "observe" => Self::Observe,
            "tool_error" => Self::ToolError,
            "done" => Self::Done,
            "fail" => Self::Fail,
            _ => return None,
        })
    }
}

/// Una respuesta dirigida a una petición concreta (`agent`, `request`).
/// Si no coincide con la petición pendiente del agente, el runtime la descarta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub agent: ConceptId,
    pub request: u64,
    pub verb: Verb,
    pub payload: String,
}

impl Reply {
    pub fn new(agent: ConceptId, request: u64, verb: Verb, payload: impl Into<String>) -> Self {
        Self {
            agent,
            request,
            verb,
            payload: payload.into(),
        }
    }

    pub fn say(agent: ConceptId, request: u64, text: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Say, text)
    }

    pub fn call(agent: ConceptId, request: u64, tool_and_args: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Call, tool_and_args)
    }

    pub fn observe(agent: ConceptId, request: u64, text: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Observe, text)
    }

    pub fn done(agent: ConceptId, request: u64, result: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Done, result)
    }

    pub fn fail(agent: ConceptId, request: u64, reason: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Fail, reason)
    }

    /// Convierte una `Decision` en la respuesta equivalente.
    /// Útil para decisores que comparten lógica con un `ScriptedPolicy`.
    pub fn from_decision(agent: ConceptId, request: u64, decision: Decision) -> Self {
        match decision {
            Decision::Say(t) => Self::say(agent, request, t),
            Decision::Call { tool, args } => {
                let line = if args.is_empty() {
                    tool
                } else {
                    format!("{tool} {args}")
                };
                Self::call(agent, request, line)
            }
            Decision::Done(r) => Self::done(agent, request, r),
            Decision::Fail(r) => Self::fail(agent, request, r),
        }
    }

    /// Formato: `agent \t request \t verb \t payload` en UTF-8. El payload es
    /// el resto de la línea y puede contener tabuladores.
    pub fn to_bytes(&self) -> Vec<u8> {
        format!(
            "{}\t{}\t{}\t{}",
            self.agent.0,
            self.request,
            self.verb.as_str(),
            self.payload
        )
        .into_bytes()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let s = std::str::from_utf8(bytes).map_err(|_| "reply is not UTF-8".to_string())?;
        let mut parts = s.splitn(4, '\t');
        let agent = parts.next().and_then(|p| p.parse::<u64>().ok());
        let request = parts.next().and_then(|p| p.parse::<u64>().ok());
        let verb = parts.next().and_then(Verb::parse);
        let payload = parts.next();
        match (agent, request, verb, payload) {
            (Some(a), Some(r), Some(v), Some(p)) => Ok(Self::new(ConceptId(a), r, v, p)),
            _ => Err("malformed reply".to_string()),
        }
    }

    /// El evento de ingress que un driver debe empujar al bus.
    pub fn to_event(&self) -> OnyxEvent {
        OnyxEvent::Ingress(IngressEvent::Raw {
            source: REPLY_SOURCE.to_string(),
            bytes: self.to_bytes(),
        })
    }
}
