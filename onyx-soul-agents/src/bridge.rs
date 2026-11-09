//! Puente con decisores externos (LLM, humano, script) como `PlatformDriver`.
//!
//! Es el punto de extensión principal del framework. El cerebro nunca llama al
//! decisor: un agente `Thinking` ya es, en el grafo, una petición pendiente. El
//! driver la lee en `present` (solo `GraphView`) y la contesta con un `Reply`
//! que inyecta como ingress en un `poll` posterior.
//!
//! * `RecordingBridge` consulta un `Responder` y graba cada respuesta con el
//!   número de `poll` en que entró al bus.
//! * `ReplayBridge` reinyecta esa `Transcript` en los mismos `poll`, sin
//!   decisor. Con el mismo estado inicial y semilla, la ejecución se reproduce.
//! * Ambos pueden envolver otro `PlatformDriver` (TUI, red, headless) con
//!   `wrapping`: el puente añade su canal sin quitar el del resto de la app.
//!
//! El número de `poll` coincide con el número de tick mientras el llamador
//! haga un tick por `poll` desde el principio. Tras un restore, `starting_at`
//! alinea el cursor con el siguiente `poll` sin reescribir la transcripción;
//! el host elige el offset. El core aún no define un protocolo de recuperación.

use std::collections::BTreeSet;

use onyx_core::{EventBus, GraphView, PlatformDriver, SurfaceBinding};

use crate::policy::{AgentView, agent_view};
use crate::reply::Reply;
use crate::util::text;
use crate::vocab::{AgentState, Vocab};

/// Peticiones pendientes: agentes en `Thinking`, en orden de id.
pub fn pending_requests(view: &dyn GraphView, v: &Vocab) -> Vec<AgentView> {
    view.query(&[v.state, v.pending])
        .into_iter()
        .filter(|id| {
            text(view, *id, v.state).and_then(|s| AgentState::parse(&s))
                == Some(AgentState::Thinking)
        })
        .map(|id| agent_view(view, v, id))
        .filter(|a| a.request > 0)
        .collect()
}

/// Quien decide. Un cliente de LLM implementa esto fuera de este crate.
/// Devolver `None` deja la petición sin respuesta (el agente acabará en timeout).
pub trait Responder {
    fn respond(&mut self, request: &AgentView) -> Option<Reply>;
}

impl<F: FnMut(&AgentView) -> Option<Reply>> Responder for F {
    fn respond(&mut self, request: &AgentView) -> Option<Reply> {
        self(request)
    }
}

impl Responder for Box<dyn Responder> {
    fn respond(&mut self, request: &AgentView) -> Option<Reply> {
        (**self).respond(request)
    }
}

/// Respuesta grabada y el `poll` en que se inyectó.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logged {
    pub poll: u64,
    pub reply: Reply,
}

/// Transcripción de una ejecución: lo único que hace falta para reproducirla
/// además del estado inicial y la semilla.
///
/// Formato binario propio: `ONYXTR1\n`, luego por entrada `poll: u64 LE`,
/// `len: u32 LE` y los bytes de `Reply::to_bytes`. Sin suma de comprobación.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transcript {
    pub entries: Vec<Logged>,
}

const MAGIC: &[u8] = b"ONYXTR1\n";

impl Transcript {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        for e in &self.entries {
            let body = e.reply.to_bytes();
            let len = u32::try_from(body.len()).expect("reply exceeds 4 GiB");
            out.extend_from_slice(&e.poll.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&body);
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let mut rest = bytes.strip_prefix(MAGIC).ok_or("not a transcript")?;
        let mut entries = Vec::new();
        let mut last_poll = 0u64;
        while !rest.is_empty() {
            if rest.len() < 12 {
                return Err("truncated transcript".into());
            }
            let poll = u64::from_le_bytes(rest[..8].try_into().unwrap());
            let len = usize::try_from(u32::from_le_bytes(rest[8..12].try_into().unwrap()))
                .expect("usize fits a u32 length");
            rest = &rest[12..];
            if rest.len() < len {
                return Err("truncated transcript".into());
            }
            if poll < last_poll {
                return Err("transcript out of order".into());
            }
            last_poll = poll;
            entries.push(Logged {
                poll,
                reply: Reply::from_bytes(&rest[..len])?,
            });
            rest = &rest[len..];
        }
        Ok(Self { entries })
    }
}

pub struct RecordingBridge<R: Responder> {
    vocab: Vocab,
    responder: R,
    inner: Option<Box<dyn PlatformDriver>>,
    queued: Vec<Reply>,
    dispatched: BTreeSet<(u64, u64)>,
    polls: u64,
    transcript: Transcript,
}

impl<R: Responder> RecordingBridge<R> {
    pub fn new(vocab: Vocab, responder: R) -> Self {
        Self {
            vocab,
            responder,
            inner: None,
            queued: Vec::new(),
            dispatched: BTreeSet::new(),
            polls: 0,
            transcript: Transcript::default(),
        }
    }

    /// Envuelve otro driver: sus eventos y su `present` siguen funcionando.
    pub fn wrapping(mut self, inner: impl PlatformDriver + 'static) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// Continúa grabando desde un `poll` concreto, por ejemplo tras un restore.
    /// No reescribe la transcripción ya guardada; las entradas nuevas usarán
    /// este valor y los siguientes.
    pub fn starting_at(mut self, poll: u64) -> Self {
        self.polls = poll;
        self
    }
}

impl<R: Responder> PlatformDriver for RecordingBridge<R> {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Some(inner) = self.inner.as_mut() {
            inner.poll(bus);
        }
        for reply in self.queued.drain(..) {
            bus.push(reply.to_event());
            self.transcript.entries.push(Logged {
                poll: self.polls,
                reply,
            });
        }
        self.polls += 1;
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
        for request in pending_requests(graph, &self.vocab) {
            // Cada petición se atiende una vez, aunque siga pendiente varios ticks.
            if self.dispatched.insert((request.agent.0, request.request)) {
                if let Some(reply) = self.responder.respond(&request) {
                    self.queued.push(reply);
                }
            }
        }
    }
}

pub struct ReplayBridge {
    transcript: Vec<Logged>,
    inner: Option<Box<dyn PlatformDriver>>,
    cursor: usize,
    polls: u64,
}

impl ReplayBridge {
    pub fn new(transcript: Transcript) -> Self {
        Self {
            transcript: transcript.entries,
            inner: None,
            cursor: 0,
            polls: 0,
        }
    }

    /// Envuelve otro driver: sus eventos y su `present` siguen funcionando.
    pub fn wrapping(mut self, inner: impl PlatformDriver + 'static) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

    /// Continúa el replay desde un `poll` concreto sin reescribir la
    /// transcripción. Las entradas con `Logged.poll` anterior no se reinyectan
    /// y no cuentan en [`remaining`](Self::remaining). El host elige el offset.
    pub fn starting_at(mut self, poll: u64) -> Self {
        self.polls = poll;
        self.cursor = self.transcript.partition_point(|entry| entry.poll < poll);
        self
    }

    /// Cuántas respuestas grabadas quedan sin reinyectar.
    pub fn remaining(&self) -> usize {
        self.transcript.len() - self.cursor
    }
}

impl PlatformDriver for ReplayBridge {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Some(inner) = self.inner.as_mut() {
            inner.poll(bus);
        }
        while let Some(entry) = self.transcript.get(self.cursor) {
            if entry.poll != self.polls {
                break;
            }
            bus.push(entry.reply.to_event());
            self.cursor += 1;
        }
        self.polls += 1;
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
    }
}
