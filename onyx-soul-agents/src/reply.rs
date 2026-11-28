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
    /// Aprobar la herramienta pendiente en `WaitingApproval`.
    Approve,
    /// Denegar la herramienta pendiente en `WaitingApproval`. Payload: motivo opcional.
    Deny,
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
            Self::Approve => "approve",
            Self::Deny => "deny",
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
            "approve" => Self::Approve,
            "deny" => Self::Deny,
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
    /// Identificador opcional de correlación para llamadas a herramientas (v2).
    pub call_id: Option<String>,
    /// `FIX-31`: época de sesión en que se produjo la respuesta (restores
    /// aplicados con éxito; contador compartido por el `AgentHost` y los
    /// drivers). `None` = sin época (v1/v2: el runtime no la comprueba);
    /// `Some(e)` = v3: el runtime la descarta si `e` difiere de la época
    /// actual de la sesión (ni tarde, ni duplicada, ni ajena).
    pub epoch: Option<u64>,
}

impl Reply {
    pub fn new(agent: ConceptId, request: u64, verb: Verb, payload: impl Into<String>) -> Self {
        Self {
            agent,
            request,
            verb,
            payload: payload.into(),
            call_id: None,
            epoch: None,
        }
    }

    pub fn with_call_id(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }

    /// `FIX-31`: stampea la época de sesión (formato v3). Las transcripciones
    /// graban una copia con `epoch = None` (v1/v2 byte-idénticos); el
    /// transporte lleva la época para que el runtime descarte respuestas que
    /// cruzan un restore.
    pub fn with_epoch(mut self, epoch: u64) -> Self {
        self.epoch = Some(epoch);
        self
    }

    pub fn say(agent: ConceptId, request: u64, text: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Say, text)
    }

    pub fn call(agent: ConceptId, request: u64, tool_and_args: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Call, tool_and_args)
    }

    /// Llamada a herramienta con separación sin ambigüedad (`\0`), segura para
    /// argumentos multi-línea, JSON o rutas con espacios.
    pub fn call_structured(
        agent: ConceptId,
        request: u64,
        tool: impl Into<String>,
        args: impl Into<String>,
    ) -> Self {
        let tool = tool.into();
        let args = args.into();
        let payload = if args.is_empty() {
            tool
        } else {
            format!("{tool}\0{args}")
        };
        Self::new(agent, request, Verb::Call, payload)
    }

    /// Extrae `(tool, args)` de un payload de `Verb::Call`.
    /// Prioriza el separador estructurado `\0` y cae al primer espacio `' '`
    /// por compatibilidad con el formato simple.
    pub fn parse_call_payload(payload: &str) -> (&str, &str) {
        if let Some((t, a)) = payload.split_once('\0') {
            (t, a)
        } else if let Some((t, a)) = payload.split_once(' ') {
            (t, a)
        } else {
            (payload, "")
        }
    }

    pub fn observe(agent: ConceptId, request: u64, text: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Observe, text)
    }

    pub fn tool_error(agent: ConceptId, request: u64, error: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::ToolError, error)
    }

    pub fn done(agent: ConceptId, request: u64, result: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Done, result)
    }

    pub fn fail(agent: ConceptId, request: u64, reason: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Fail, reason)
    }

    pub fn approve(agent: ConceptId, request: u64) -> Self {
        Self::new(agent, request, Verb::Approve, "")
    }

    pub fn deny(agent: ConceptId, request: u64, reason: impl Into<String>) -> Self {
        Self::new(agent, request, Verb::Deny, reason)
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

    /// Formato v1: `agent \t request \t verb \t payload` en UTF-8.
    /// Formato v2 (cuando hay `call_id`): `v2 \t agent \t request \t verb \t len:call_id \t payload`.
    /// Formato v3 (cuando hay `epoch`): `v3 \t agent \t request \t verb \t epoch \t len:call_id \t payload`;
    /// el slot de `call_id` va SIEMPRE presente (un cid vacío es `0:` → sin `call_id`).
    pub fn to_bytes(&self) -> Vec<u8> {
        if let Some(e) = self.epoch {
            let (cid_len, cid) = match &self.call_id {
                None => (0usize, ""),
                Some(c) => (c.len(), c.as_str()),
            };
            return format!(
                "v3\t{}\t{}\t{}\t{}\t{}:{}\t{}",
                self.agent.0,
                self.request,
                self.verb.as_str(),
                e,
                cid_len,
                cid,
                self.payload
            )
            .into_bytes();
        }
        match &self.call_id {
            None => format!(
                "{}\t{}\t{}\t{}",
                self.agent.0,
                self.request,
                self.verb.as_str(),
                self.payload
            )
            .into_bytes(),
            Some(cid) => format!(
                "v2\t{}\t{}\t{}\t{}:{}\t{}",
                self.agent.0,
                self.request,
                self.verb.as_str(),
                cid.len(),
                cid,
                self.payload
            )
            .into_bytes(),
        }
    }

    /// `FIX-31`: v1/v2 no llevan época (`epoch = None`); v3 la lleva
    /// (`epoch = Some(e)`) con el slot de `call_id` siempre presente
    /// (`0:` → sin `call_id`).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let s = std::str::from_utf8(bytes).map_err(|_| "reply is not UTF-8".to_string())?;
        if let Some(rest) = s.strip_prefix("v3\t") {
            let mut parts = rest.splitn(5, '\t');
            let agent = parts.next().and_then(|p| p.parse::<u64>().ok());
            let request = parts.next().and_then(|p| p.parse::<u64>().ok());
            let verb = parts.next().and_then(Verb::parse);
            let epoch = parts.next().and_then(|p| p.parse::<u64>().ok());
            let tail = parts.next();
            let (Some(a), Some(r), Some(v), Some(e), Some(tail)) =
                (agent, request, verb, epoch, tail)
            else {
                return Err("malformed reply".to_string());
            };
            let Some((len_str, after_colon)) = tail.split_once(':') else {
                return Err("malformed reply".to_string());
            };
            let Ok(cid_len) = len_str.parse::<usize>() else {
                return Err("malformed reply".to_string());
            };
            if after_colon.len() <= cid_len
                || !after_colon.is_char_boundary(cid_len)
                || after_colon.as_bytes()[cid_len] != b'\t'
            {
                return Err("malformed reply".to_string());
            }
            let mut reply =
                Self::new(ConceptId(a), r, v, &after_colon[cid_len + 1..]).with_epoch(e);
            if cid_len > 0 {
                reply = reply.with_call_id(&after_colon[..cid_len]);
            }
            return Ok(reply);
        }
        if let Some(rest) = s.strip_prefix("v2\t") {
            let mut parts = rest.splitn(4, '\t');
            let agent = parts.next().and_then(|p| p.parse::<u64>().ok());
            let request = parts.next().and_then(|p| p.parse::<u64>().ok());
            let verb = parts.next().and_then(Verb::parse);
            let tail = parts.next();
            let (Some(a), Some(r), Some(v), Some(tail)) = (agent, request, verb, tail) else {
                return Err("malformed reply".to_string());
            };
            let Some((len_str, after_colon)) = tail.split_once(':') else {
                return Err("malformed reply".to_string());
            };
            let Ok(cid_len) = len_str.parse::<usize>() else {
                return Err("malformed reply".to_string());
            };
            if after_colon.len() <= cid_len
                || !after_colon.is_char_boundary(cid_len)
                || after_colon.as_bytes()[cid_len] != b'\t'
            {
                return Err("malformed reply".to_string());
            }
            let call_id = &after_colon[..cid_len];
            let payload = &after_colon[cid_len + 1..];
            return Ok(Self::new(ConceptId(a), r, v, payload).with_call_id(call_id));
        }

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
