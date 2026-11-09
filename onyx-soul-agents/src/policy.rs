//! Puntos de extensión de comportamiento del runtime.
//!
//! * `MemoryPolicy`: qué se recuerda y cuánto.
//! * `ScriptedPolicy`: cómo decide un agente de simulación (sin decisor externo).
//!
//! Los valores por defecto (`DefaultMemory`, `UniformTool`) son razonables y
//! sustituibles: la app los cambia con `AgentRuntime::with_memory` y
//! `AgentRuntime::with_scripted`.

use onyx_core::{ConceptId, DetRng, GraphView, Value};

use crate::util::{int, text};
use crate::vocab::Vocab;

/// Una anotación de la memoria de un agente.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryEntry {
    /// `say`, `call`, `denied`, `observe` o `tool_error`.
    pub kind: String,
    pub text: String,
}

pub trait MemoryPolicy {
    /// Decide qué se anota. `None` descarta la entrada; devolver otra la
    /// transforma (por ejemplo, para recortar o censurar texto).
    fn filter(&self, entry: MemoryEntry) -> Option<MemoryEntry> {
        Some(entry)
    }

    /// Entradas a conservar por agente. 0 = sin poda.
    /// Podar usa `destroy`, que recorre el grafo.
    fn keep_last(&self) -> usize {
        0
    }
}

/// Anota todo y conserva las últimas `keep_last` entradas (0 = todas).
#[derive(Copy, Clone, Debug, Default)]
pub struct DefaultMemory {
    keep_last: usize,
}

impl DefaultMemory {
    pub fn new(keep_last: usize) -> Self {
        Self { keep_last }
    }
}

impl MemoryPolicy for DefaultMemory {
    fn keep_last(&self) -> usize {
        self.keep_last
    }
}

/// Lo que un agente ve de sí mismo: el estado del grafo en el instante de decidir.
/// También es el contenido de una petición a un decisor externo.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentView {
    pub agent: ConceptId,
    /// Identificador de la petición pendiente. Una respuesta debe citarlo.
    pub request: u64,
    pub name: String,
    pub goal: String,
    pub tools: Vec<String>,
    /// Memoria cronológica: `(kind, text)`.
    pub memory: Vec<(String, String)>,
    /// Todas las propiedades del agente por nombre, también las de la app.
    pub props: Vec<(String, Value)>,
}

pub(crate) fn agent_view<G: GraphView + ?Sized>(g: &G, v: &Vocab, id: ConceptId) -> AgentView {
    let tools = g
        .related(id, v.rel_tool)
        .into_iter()
        .filter_map(|t| text(g, t, v.tool_name))
        .collect();
    // Los ids no se reciclan: el orden de id es el cronológico.
    let memory = g
        .related(id, v.rel_memory)
        .into_iter()
        .map(|m| {
            (
                text(g, m, v.mem_kind).unwrap_or_default(),
                text(g, m, v.mem_text).unwrap_or_default(),
            )
        })
        .collect();
    AgentView {
        agent: id,
        request: int(g, id, v.pending)
            .and_then(|i| u64::try_from(i).ok())
            .unwrap_or(0),
        name: text(g, id, v.name).unwrap_or_default(),
        goal: text(g, id, v.goal).unwrap_or_default(),
        tools,
        memory,
        props: g.props(id),
    }
}

/// Resultado de una decisión, venga de un decisor externo o de un `ScriptedPolicy`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Anotar un mensaje y volver a decidir.
    Say(String),
    /// Llamar a una herramienta del agente.
    Call { tool: String, args: String },
    /// Terminar con éxito.
    Done(String),
    /// Terminar con error.
    Fail(String),
}

/// Decisor interno para agentes `Policy::Scripted`.
///
/// Debe ser determinista: solo puede usar `view` y `rng`. Leer reloj, entorno
/// o aleatoriedad del proceso rompe la reproducción.
pub trait ScriptedPolicy {
    fn decide(&mut self, view: &AgentView, rng: &mut DetRng) -> Decision;
}

/// Por defecto: llama a una herramienta elegida con `DetRng`; sin herramientas, termina.
#[derive(Copy, Clone, Debug, Default)]
pub struct UniformTool;

impl ScriptedPolicy for UniformTool {
    fn decide(&mut self, view: &AgentView, rng: &mut DetRng) -> Decision {
        if view.tools.is_empty() {
            return Decision::Done("no_tools".into());
        }
        let n = u64::try_from(view.tools.len()).unwrap_or(u64::MAX);
        let pick = usize::try_from(rng.gen_range(0, n)).unwrap_or(0);
        Decision::Call {
            tool: view.tools[pick].clone(),
            args: String::new(),
        }
    }
}
