//! Herramientas síncronas y deterministas que se ejecutan dentro del cerebro.
//!
//! [`ToolSystem`] escucha `ev.tool_call`, ejecuta el [`ToolExecutor`] cuyo
//! nombre coincide y emite `ev.tool_result`. Una herramienta sin ejecutor
//! aquí se asume externa: otro sistema o un driver contestará con su propio
//! `ev.tool_result`, y si nadie lo hace el agente falla por timeout.
//!
//! El runtime ya validó que el agente tiene permiso de usar esa herramienta.
//! El nombre y los argumentos llegan separados: el runtime parte el payload
//! de un `Call` por el primer espacio (el nombre no puede contener espacios).
//! En [`crate::AgentSpec`] los nombres tampoco pueden contener comas, porque
//! viajan juntos en `tools_csv`.
//!
//! Un ejecutor solo puede tocar el grafo y `DetRng`: nada de reloj, red ni
//! disco. Lo no determinista vive fuera del cerebro.
//!
//! [`ToolExecutor::init`] se llama desde [`ToolSystem::init`]. El tick vuelve
//! a llamar `init` tras un import correcto, así que el ejecutor puede
//! resolver otra vez sus atributos.

use onyx_core::{
    AttrRegistry, ConceptId, Ctx, DetRng, Graph, OnyxError, OnyxEvent, OnyxSystem, Value,
};

use crate::config::AgentConfig;
use crate::util::{current_tick, data, domain, fault};
use crate::vocab::Vocab;

pub const TOOLS_NAME: &str = "agent_tools";

/// Llamada ya validada por el runtime (el agente tiene permiso de usarla).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub agent: ConceptId,
    pub request: i64,
    pub tool: String,
    pub args: String,
}

/// Lo que un ejecutor puede usar durante `call`.
pub struct ToolCtx<'a> {
    pub graph: &'a mut dyn Graph,
    pub rng: &'a mut DetRng,
    pub vocab: &'a Vocab,
    pub tick: u64,
}

pub trait ToolExecutor {
    /// Nombre con el que los agentes la declaran en `AgentSpec::tools`.
    fn name(&self) -> &str;

    /// Resuelve los atributos propios. Se llama en cada `init` del sistema,
    /// incluido el que el tick dispara tras un import.
    fn init(&mut self, _attrs: &mut dyn AttrRegistry) {}

    /// `Err` llega al agente como `tool_error` con ese mensaje.
    fn call(&mut self, call: &ToolCall, ctx: &mut ToolCtx) -> Result<String, String>;
}

pub struct ToolSystem {
    config: AgentConfig,
    vocab: Option<Vocab>,
    tools: Vec<Box<dyn ToolExecutor>>,
}

impl ToolSystem {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            vocab: None,
            tools: Vec::new(),
        }
    }

    /// Añade un ejecutor. Si dos comparten nombre, se usa el primero.
    pub fn with_tool(mut self, tool: Box<dyn ToolExecutor>) -> Self {
        self.tools.push(tool);
        self
    }
}

impl OnyxSystem for ToolSystem {
    fn name(&self) -> &str {
        TOOLS_NAME
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.vocab = Some(Vocab::resolve_with(attrs, &self.config.vocab_prefix));
        for tool in self.tools.iter_mut() {
            tool.init(attrs);
        }
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let Some(v) = self.vocab else {
            return;
        };
        let tick = current_tick(ctx.inbox).unwrap_or(0);
        let inbox = ctx.inbox;
        for event in inbox {
            let OnyxEvent::Domain(d) = event else {
                continue;
            };
            if d.kind != v.ev_tool_call {
                continue;
            }
            let (Some(agent), Some(Value::Int(request)), Some(Value::Text(tool))) =
                (d.subject, data(d, v.request), data(d, v.tool))
            else {
                ctx.outbox.push(fault("malformed tool call".into()));
                continue;
            };
            let args = match data(d, v.args) {
                Some(Value::Text(a)) => a.clone(),
                _ => String::new(),
            };
            let Some(exec) = self.tools.iter_mut().find(|t| t.name() == tool.as_str()) else {
                continue;
            };
            let call = ToolCall {
                agent,
                request: *request,
                tool: tool.clone(),
                args,
            };
            let outcome = {
                let mut tctx = ToolCtx {
                    graph: &mut *ctx.graph,
                    rng: &mut *ctx.rng,
                    vocab: &v,
                    tick,
                };
                exec.call(&call, &mut tctx)
            };
            let (ok, body) = match outcome {
                Ok(text) => (true, text),
                Err(e) => (false, e),
            };
            ctx.outbox.push(domain(
                v.ev_tool_result,
                Some(agent),
                vec![
                    (v.request, Value::Int(*request)),
                    (v.text, Value::Text(body)),
                    (v.ok, Value::Bool(ok)),
                ],
            ));
        }
    }
}
