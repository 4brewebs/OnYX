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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use onyx_core::{EventBus, GraphView, IngressEvent, OnyxEvent, PlatformDriver, SurfaceBinding};

use crate::policy::{AgentView, PendingApproval, agent_view};
use crate::reply::{REPLY_SOURCE, Reply};
use crate::util::{int, text};
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

/// Aprobaciones humanas pendientes: agentes en `WaitingApproval`, en orden de id.
pub fn pending_approvals(view: &dyn GraphView, v: &Vocab) -> Vec<AgentView> {
    view.query(&[v.state, v.pending])
        .into_iter()
        .filter(|id| {
            text(view, *id, v.state).and_then(|s| AgentState::parse(&s))
                == Some(AgentState::WaitingApproval)
        })
        .map(|id| agent_view(view, v, id))
        .filter(|a| a.request > 0 && a.pending_approval.is_some())
        .collect()
}

/// Llamadas a herramientas en ejecución externa (`AgentState::Acting`), en orden de id.
pub fn pending_tool_calls(
    view: &dyn GraphView,
    v: &Vocab,
) -> Vec<(onyx_core::ConceptId, u64, PendingApproval)> {
    view.query(&[v.state, v.pending])
        .into_iter()
        .filter(|id| {
            text(view, *id, v.state).and_then(|s| AgentState::parse(&s)) == Some(AgentState::Acting)
        })
        .filter_map(|id| {
            let request = int(view, id, v.pending).and_then(|i| u64::try_from(i).ok())?;
            if request == 0 {
                return None;
            }
            let tool = text(view, id, v.pending_tool).filter(|s| !s.is_empty())?;
            let args = text(view, id, v.pending_args).unwrap_or_default();
            let call_id = text(view, id, v.pending_call_id).filter(|s| !s.is_empty());
            Some((
                id,
                request,
                PendingApproval {
                    tool,
                    args,
                    call_id,
                },
            ))
        })
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

/// Comando de usuario (`IngressEvent::Command`) grabado con su `poll`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoggedCommand {
    pub poll: u64,
    pub command: String,
}

/// Transcripción de una ejecución: lo único que hace falta para reproducirla
/// además del estado inicial y la semilla.
///
/// Soporta `ONYXTR1\n` (solo respuestas) y `ONYXTR2\n` (respuestas + comandos).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transcript {
    pub entries: Vec<Logged>,
    pub commands: Vec<LoggedCommand>,
}

const MAGIC_V1: &[u8] = b"ONYXTR1\n";
const MAGIC_V2: &[u8] = b"ONYXTR2\n";

impl Transcript {
    pub fn to_bytes(&self) -> Vec<u8> {
        if self.commands.is_empty() {
            let mut out = MAGIC_V1.to_vec();
            for e in &self.entries {
                let body = e.reply.to_bytes();
                let len = u32::try_from(body.len()).expect("reply exceeds 4 GiB");
                out.extend_from_slice(&e.poll.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&body);
            }
            return out;
        }

        let mut out = MAGIC_V2.to_vec();
        let mut ei = 0usize;
        let mut ci = 0usize;
        while ei < self.entries.len() || ci < self.commands.len() {
            let take_cmd = match (self.entries.get(ei), self.commands.get(ci)) {
                (Some(e), Some(c)) => c.poll <= e.poll,
                (None, Some(_)) => true,
                _ => false,
            };
            if take_cmd {
                let c = &self.commands[ci];
                let body = c.command.as_bytes();
                let len = u32::try_from(body.len()).expect("command exceeds 4 GiB");
                out.push(1);
                out.extend_from_slice(&c.poll.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(body);
                ci += 1;
            } else {
                let e = &self.entries[ei];
                let body = e.reply.to_bytes();
                let len = u32::try_from(body.len()).expect("reply exceeds 4 GiB");
                out.push(0);
                out.extend_from_slice(&e.poll.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&body);
                ei += 1;
            }
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if let Some(mut rest) = bytes.strip_prefix(MAGIC_V2) {
            let mut entries = Vec::new();
            let mut commands = Vec::new();
            let mut last_poll = 0u64;
            while !rest.is_empty() {
                if rest.len() < 13 {
                    return Err("truncated transcript".into());
                }
                let tag = rest[0];
                let poll = u64::from_le_bytes(rest[1..9].try_into().unwrap());
                let len = usize::try_from(u32::from_le_bytes(rest[9..13].try_into().unwrap()))
                    .expect("usize fits a u32 length");
                rest = &rest[13..];
                if rest.len() < len {
                    return Err("truncated transcript".into());
                }
                if poll < last_poll {
                    return Err("transcript out of order".into());
                }
                last_poll = poll;
                match tag {
                    0 => entries.push(Logged {
                        poll,
                        reply: Reply::from_bytes(&rest[..len])?,
                    }),
                    1 => {
                        let cmd = std::str::from_utf8(&rest[..len])
                            .map_err(|_| "command is not UTF-8".to_string())?;
                        commands.push(LoggedCommand {
                            poll,
                            command: cmd.to_string(),
                        });
                    }
                    _ => return Err("unknown transcript record tag".into()),
                }
                rest = &rest[len..];
            }
            return Ok(Self { entries, commands });
        }

        let mut rest = bytes.strip_prefix(MAGIC_V1).ok_or("not a transcript")?;
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
        Ok(Self {
            entries,
            commands: Vec::new(),
        })
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
    /// `FIX-31`: handle de la época de sesión del host (opcional).
    restore_epoch: Option<Arc<AtomicU64>>,
    /// Última época vista en `present` (solo con handle).
    seen_epoch: u64,
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
            restore_epoch: None,
            seen_epoch: 0,
        }
    }

    /// `FIX-31`: comparte la época de sesión del `AgentHost`. Al cambiar la
    /// época (un restore aplicado), las peticiones pendientes son peticiones
    /// NUEVAS: `dispatched` se limpia para re-despacharlas, y las respuestas
    /// encoladas se stampean con la época en que nacen (criterio 3 de la spec:
    /// cada respuesta se compara con la época). Sin el handle (por ejemplo en
    /// replays y tests sin restore en vivo) el comportamiento es el anterior.
    pub fn with_restore_epoch(mut self, epoch: Arc<AtomicU64>) -> Self {
        self.restore_epoch = Some(epoch);
        self
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

struct RecordingBus<'a> {
    inner: &'a mut dyn EventBus,
    poll: u64,
    entries: &'a mut Vec<Logged>,
    commands: &'a mut Vec<LoggedCommand>,
}

impl EventBus for RecordingBus<'_> {
    fn push(&mut self, event: OnyxEvent) {
        match &event {
            OnyxEvent::Ingress(IngressEvent::Raw { source, bytes }) if source == REPLY_SOURCE => {
                if let Ok(mut reply) = Reply::from_bytes(bytes) {
                    // `FIX-31` (c): la transcripción no viaja con época (v1/v2
                    // byte-idénticos); el evento al bus la conserva.
                    reply.epoch = None;
                    self.entries.push(Logged {
                        poll: self.poll,
                        reply,
                    });
                }
            }
            OnyxEvent::Ingress(IngressEvent::Command(cmd)) => {
                self.commands.push(LoggedCommand {
                    poll: self.poll,
                    command: cmd.clone(),
                });
            }
            _ => {}
        }
        self.inner.push(event);
    }

    fn drain(&mut self) -> Vec<OnyxEvent> {
        self.inner.drain()
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl<R: Responder> PlatformDriver for RecordingBridge<R> {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Some(inner) = self.inner.as_mut() {
            let mut rec = RecordingBus {
                inner: bus,
                poll: self.polls,
                entries: &mut self.transcript.entries,
                commands: &mut self.transcript.commands,
            };
            inner.poll(&mut rec);
        }
        for reply in self.queued.drain(..) {
            bus.push(reply.to_event());
            // `FIX-31` (c): el evento al bus conserva la época; la copia
            // grabada la pierde (transcripción v1/v2 byte-idéntica).
            let mut logged = reply.clone();
            logged.epoch = None;
            self.transcript.entries.push(Logged {
                poll: self.polls,
                reply: logged,
            });
        }
        self.polls += 1;
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
        let pending = pending_requests(graph, &self.vocab);
        // `FIX-31`: cambio de época (restore aplicado) → línea temporal nueva:
        // las peticiones pendientes son peticiones NUEVAS y el `dispatched`
        // anterior ya no las protege (sin esto, la petición restaurada se
        // salta y el agente queda colgado: el bug de FIX-31).
        if let Some(epoch) = &self.restore_epoch {
            let now = epoch.load(Ordering::Acquire);
            if now != self.seen_epoch {
                self.seen_epoch = now;
                self.dispatched.clear();
            }
        }
        // `FIX-31` (d): una petición que ya no está pendiente deja de estar
        // protegida: si reaparece (rebobinado de contadores sin restore) debe
        // re-despacharse.
        let pending_keys: BTreeSet<(u64, u64)> =
            pending.iter().map(|r| (r.agent.0, r.request)).collect();
        self.dispatched.retain(|key| pending_keys.contains(key));
        for request in &pending {
            // Cada petición se atiende una vez, aunque siga pendiente varios ticks.
            if self.dispatched.insert((request.agent.0, request.request)) {
                if let Some(reply) = self.responder.respond(request) {
                    // Cierra la ventana `present` → restore → `poll`: la respuesta
                    // encolada viaja con la época en que nació, y el runtime la
                    // descarta si cruza un restore.
                    let reply = match (&self.restore_epoch, reply.epoch) {
                        (Some(epoch), None) => reply.with_epoch(epoch.load(Ordering::Acquire)),
                        _ => reply,
                    };
                    self.queued.push(reply);
                }
            }
        }
    }
}

pub struct ReplayBridge {
    transcript: Vec<Logged>,
    commands: Vec<LoggedCommand>,
    inner: Option<Box<dyn PlatformDriver>>,
    cursor: usize,
    cmd_cursor: usize,
    polls: u64,
}

impl ReplayBridge {
    pub fn new(transcript: Transcript) -> Self {
        Self {
            transcript: transcript.entries,
            commands: transcript.commands,
            inner: None,
            cursor: 0,
            cmd_cursor: 0,
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
        self.cmd_cursor = self.commands.partition_point(|cmd| cmd.poll < poll);
        self
    }

    /// Cuántas entradas grabadas (respuestas y comandos) quedan sin reinyectar.
    pub fn remaining(&self) -> usize {
        (self.transcript.len() - self.cursor) + (self.commands.len() - self.cmd_cursor)
    }
}

impl PlatformDriver for ReplayBridge {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Some(inner) = self.inner.as_mut() {
            inner.poll(bus);
        }
        while let Some(cmd) = self.commands.get(self.cmd_cursor) {
            if cmd.poll != self.polls {
                break;
            }
            bus.push(OnyxEvent::Ingress(IngressEvent::Command(
                cmd.command.clone(),
            )));
            self.cmd_cursor += 1;
        }
        while let Some(entry) = self.transcript.get(self.cursor) {
            if entry.poll != self.polls {
                break;
            }
            // `FIX-31` (c, defensivo): el replay nunca reinyecta épocas
            // viejas: una transcripción grabada no las lleva (v1/v2), y una
            // construida a mano no debe contaminar la sesión en curso.
            let mut reply = entry.reply.clone();
            reply.epoch = None;
            bus.push(reply.to_event());
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
