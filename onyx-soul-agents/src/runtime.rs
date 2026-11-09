//! Sistema que hace avanzar a los agentes.
//!
//! Máquina de estados (todo el estado vive en el grafo, no en este struct):
//!
//! ```text
//!            Idle ──(external)──► Thinking ──say──► Idle
//!             │                      │
//!             │                      ├──call──► Acting ──observe / tool_error──► Idle
//!             │                      ├──done──► Done
//!             │                      └──fail──► Failed
//!             └──(scripted: ScriptedPolicy decide, con DetRng)──► igual que arriba
//!
//!   Thinking / Acting ──(sin respuesta en `timeout + 1` ticks vacíos)──► Failed("timeout")
//!   Idle ──(steps >= max_steps)──► Failed("max_steps")
//! ```
//!
//! Cada tick, si hay un `Tick { n }`, recorre tres fases:
//!
//! 0. Decrementa `remaining` de quien espera (`Thinking` o `Acting`), antes
//!    de procesar respuestas.
//! 1. Aplica replies (`agent.reply`) y resultados de herramienta
//!    (`ev_tool_result`). Un `Say`, un `observe`, un `tool_error` o una
//!    herramienta denegada vuelven el agente a `Idle`.
//! 2. Comprueba plazos vencidos y planifica, en orden ascendente de id. Un
//!    agente que volvió a `Idle` en la fase 1 puede abrir la siguiente
//!    petición en este mismo tick.
//!
//! Una decisión externa se proyecta como estado: un agente `Thinking` con su
//! `pending` es la petición. El [`crate::AgentDriver`] la lee en `present`
//! (solo `GraphView`) y responde con un [`crate::Reply`] en un tick posterior.
//!
//! `ev_finished` y `ev_failed` llevan el resultado en la clave `text`
//! (no en `reason`, que usa el spawner en `ev_rejected`).
//!
//! El plazo se inicia con `remaining = timeout + 1` (saturando) para que una
//! espera que empieza este tick no caduque al instante, y se guarda como ticks
//! restantes, no como tick absoluto: un snapshot restaurado en otro proceso
//! no hace expirar de golpe a los agentes que esperaban.

use onyx_core::{
    AttrRegistry, ConceptId, Ctx, IngressEvent, OnyxError, OnyxEvent, OnyxSystem, Value,
};

use crate::config::AgentConfig;
use crate::policy::{
    Decision, DefaultMemory, MemoryEntry, MemoryPolicy, ScriptedPolicy, UniformTool, agent_view,
};
use crate::reply::{REPLY_SOURCE, Reply, Verb};
use crate::spec::Policy;
use crate::util::{current_tick, data, domain, fault, int, text};
use crate::vocab::{AgentState, Vocab};

pub const RUNTIME_NAME: &str = "agent_runtime";

pub struct AgentRuntime {
    config: AgentConfig,
    memory: Box<dyn MemoryPolicy>,
    scripted: Box<dyn ScriptedPolicy>,
    vocab: Option<Vocab>,
}

impl AgentRuntime {
    /// Memoria y política de simulación por defecto.
    pub fn new(config: AgentConfig) -> Self {
        let memory = Box::new(DefaultMemory::new(config.memory_keep_last));
        Self {
            config,
            memory,
            scripted: Box::new(UniformTool),
            vocab: None,
        }
    }

    /// Sustituye la política de memoria.
    pub fn with_memory(mut self, memory: Box<dyn MemoryPolicy>) -> Self {
        self.memory = memory;
        self
    }

    /// Sustituye el decisor de los agentes `Policy::Scripted`.
    pub fn with_scripted(mut self, scripted: Box<dyn ScriptedPolicy>) -> Self {
        self.scripted = scripted;
        self
    }
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::new(AgentConfig::default())
    }
}

impl OnyxSystem for AgentRuntime {
    fn name(&self) -> &str {
        RUNTIME_NAME
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.vocab = Some(Vocab::resolve_with(attrs, &self.config.vocab_prefix));
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let Some(v) = self.vocab else { return };
        let Some(now) = current_tick(ctx.inbox) else {
            return;
        };
        let step = Step {
            v,
            mem: &*self.memory,
            now,
        };
        let inbox = ctx.inbox;

        // 0. Los plazos avanzan antes de procesar nada: una espera que empieza
        //    en este tick siempre queda con `remaining >= 2`.
        for id in ctx.graph.query(&[v.state, v.remaining]) {
            if step.is_waiting(ctx, id) {
                let left = int(&*ctx.graph, id, v.remaining).unwrap_or(0);
                if left > 0 {
                    ctx.graph.set(id, v.remaining, Value::Int(left - 1));
                }
            }
        }

        // 1. Respuestas externas y resultados de herramientas, en orden de llegada.
        for event in inbox {
            match event {
                OnyxEvent::Ingress(IngressEvent::Raw { source, bytes })
                    if source == REPLY_SOURCE =>
                {
                    match Reply::from_bytes(bytes) {
                        Ok(reply) => step.apply(ctx, reply),
                        Err(e) => ctx.outbox.push(fault(format!("agent reply rejected: {e}"))),
                    }
                }
                OnyxEvent::Domain(d) if d.kind == v.ev_tool_result => {
                    let request = match data(d, v.request) {
                        Some(Value::Int(r)) => match u64::try_from(*r) {
                            Ok(n) => n,
                            Err(_) => {
                                ctx.outbox.push(fault("tool result without request".into()));
                                continue;
                            }
                        },
                        _ => {
                            ctx.outbox.push(fault("tool result without request".into()));
                            continue;
                        }
                    };
                    let body = match data(d, v.text) {
                        Some(Value::Text(t)) => t.clone(),
                        _ => String::new(),
                    };
                    let verb = match data(d, v.ok) {
                        Some(Value::Bool(false)) => Verb::ToolError,
                        _ => Verb::Observe,
                    };
                    match d.subject {
                        Some(agent) => step.apply(ctx, Reply::new(agent, request, verb, body)),
                        None => ctx.outbox.push(fault("tool result without agent".into())),
                    }
                }
                _ => {}
            }
        }

        // 2. Plazos vencidos y planificación, en orden ascendente de id: el
        //    orden de consumo de `DetRng` es el mismo en cada ejecución.
        for id in ctx.graph.query(&[v.state, v.policy]) {
            step.step_agent(ctx, id, &mut *self.scripted);
        }
    }
}

/// Contexto de un tick: vocabulario, política de memoria y número de tick.
struct Step<'a> {
    v: Vocab,
    mem: &'a dyn MemoryPolicy,
    now: u64,
}

impl Step<'_> {
    fn state_of(&self, ctx: &Ctx, id: ConceptId) -> Option<AgentState> {
        text(&*ctx.graph, id, self.v.state).and_then(|s| AgentState::parse(&s))
    }

    fn is_waiting(&self, ctx: &Ctx, id: ConceptId) -> bool {
        matches!(
            self.state_of(ctx, id),
            Some(AgentState::Thinking | AgentState::Acting)
        )
    }

    fn set_state(&self, ctx: &mut Ctx, id: ConceptId, state: AgentState) {
        ctx.graph
            .set(id, self.v.state, Value::Text(state.as_str().into()));
    }

    fn remember(&self, ctx: &mut Ctx, id: ConceptId, kind: &str, body: &str) {
        let v = &self.v;
        let entry = MemoryEntry {
            kind: kind.to_string(),
            text: body.to_string(),
        };
        let Some(entry) = self.mem.filter(entry) else {
            return;
        };
        let seq = int(&*ctx.graph, id, v.mem_count)
            .unwrap_or(0)
            .saturating_add(1);
        let mem = ctx.graph.create();
        ctx.graph.set(mem, v.mem_seq, Value::Int(seq));
        ctx.graph.set(mem, v.mem_kind, Value::Text(entry.kind));
        ctx.graph.set(mem, v.mem_text, Value::Text(entry.text));
        ctx.graph.set(
            mem,
            v.mem_tick,
            Value::Int(i64::try_from(self.now).unwrap_or(i64::MAX)),
        );
        ctx.graph.relate(id, v.rel_memory, mem);
        ctx.graph.set(id, v.mem_count, Value::Int(seq));

        // Los ids no se reciclan: el orden ascendente es el cronológico.
        let keep = self.mem.keep_last();
        if keep > 0 {
            let all = ctx.graph.related(id, v.rel_memory);
            if all.len() > keep {
                for old in &all[..all.len() - keep] {
                    ctx.graph.destroy(*old);
                }
            }
        }
    }

    fn finish(&self, ctx: &mut Ctx, id: ConceptId, state: AgentState, body: &str) {
        let v = &self.v;
        self.set_state(ctx, id, state);
        ctx.graph.set(id, v.pending, Value::Int(0));
        ctx.graph.set(id, v.result, Value::Text(body.to_string()));
        let kind = if state == AgentState::Done {
            v.ev_finished
        } else {
            v.ev_failed
        };
        ctx.outbox.push(domain(
            kind,
            Some(id),
            vec![(v.text, Value::Text(body.to_string()))],
        ));
    }

    fn has_tool(&self, ctx: &Ctx, agent: ConceptId, name: &str) -> bool {
        ctx.graph
            .related(agent, self.v.rel_tool)
            .into_iter()
            .any(|t| text(&*ctx.graph, t, self.v.tool_name).as_deref() == Some(name))
    }

    fn enter_wait(&self, ctx: &mut Ctx, id: ConceptId, state: AgentState) {
        let timeout = int(&*ctx.graph, id, self.v.timeout).unwrap_or(1);
        ctx.graph
            .set(id, self.v.remaining, Value::Int(timeout.saturating_add(1)));
        self.set_state(ctx, id, state);
    }

    /// Aplica una decisión (de un decisor externo o de un `ScriptedPolicy`)
    /// a un agente que tiene la petición `request` pendiente.
    fn act(&self, ctx: &mut Ctx, id: ConceptId, request: i64, decision: Decision) {
        let v = &self.v;
        match decision {
            Decision::Say(t) => {
                self.remember(ctx, id, "say", &t);
                ctx.graph.set(id, v.pending, Value::Int(0));
                self.set_state(ctx, id, AgentState::Idle);
            }
            Decision::Call { tool, args } => {
                if self.has_tool(ctx, id, &tool) {
                    let line = if args.is_empty() {
                        tool.clone()
                    } else {
                        format!("{tool} {args}")
                    };
                    self.remember(ctx, id, "call", &line);
                    ctx.outbox.push(domain(
                        v.ev_tool_call,
                        Some(id),
                        vec![
                            (v.request, Value::Int(request)),
                            (v.tool, Value::Text(tool)),
                            (v.args, Value::Text(args)),
                        ],
                    ));
                    self.enter_wait(ctx, id, AgentState::Acting);
                } else {
                    self.remember(ctx, id, "denied", &tool);
                    ctx.graph.set(id, v.pending, Value::Int(0));
                    self.set_state(ctx, id, AgentState::Idle);
                }
            }
            Decision::Done(r) => self.finish(ctx, id, AgentState::Done, &r),
            Decision::Fail(r) => self.finish(ctx, id, AgentState::Failed, &r),
        }
    }

    /// Aplica una respuesta. Una respuesta que no coincide con la petición
    /// pendiente se descarta con un `Fault`: ni tarde, ni duplicada, ni ajena.
    fn apply(&self, ctx: &mut Ctx, r: Reply) {
        let v = &self.v;
        let id = r.agent;
        let Some(state) = self.state_of(ctx, id) else {
            return ctx
                .outbox
                .push(fault(format!("reply for unknown agent {}", id.0)));
        };
        let pending = int(&*ctx.graph, id, v.pending).unwrap_or(0);
        if !self.is_waiting(ctx, id) || i64::try_from(r.request).ok() != Some(pending) {
            return ctx.outbox.push(fault(format!(
                "stale reply for agent {} (request {}, pending {})",
                id.0, r.request, pending
            )));
        }

        let back_to_idle = |me: &Self, ctx: &mut Ctx, kind: &str, body: &str| {
            me.remember(ctx, id, kind, body);
            ctx.graph.set(id, v.pending, Value::Int(0));
            me.set_state(ctx, id, AgentState::Idle);
        };
        match (state, r.verb) {
            (AgentState::Thinking, Verb::Say) => {
                self.act(ctx, id, pending, Decision::Say(r.payload))
            }
            (AgentState::Thinking, Verb::Call) => {
                let (tool, args) = match r.payload.split_once(' ') {
                    Some((t, a)) => (t.to_string(), a.to_string()),
                    None => (r.payload.clone(), String::new()),
                };
                self.act(ctx, id, pending, Decision::Call { tool, args })
            }
            (AgentState::Thinking, Verb::Done) => {
                self.act(ctx, id, pending, Decision::Done(r.payload))
            }
            (_, Verb::Fail) => self.act(ctx, id, pending, Decision::Fail(r.payload)),
            (AgentState::Acting, Verb::Observe) => back_to_idle(self, ctx, "observe", &r.payload),
            (AgentState::Acting, Verb::ToolError) => {
                back_to_idle(self, ctx, "tool_error", &r.payload)
            }
            (state, verb) => ctx.outbox.push(fault(format!(
                "unexpected {:?} for agent {} in state {}",
                verb,
                id.0,
                state.as_str()
            ))),
        }
    }

    fn step_agent(&self, ctx: &mut Ctx, id: ConceptId, scripted: &mut dyn ScriptedPolicy) {
        let v = &self.v;
        let Some(state) = self.state_of(ctx, id) else {
            return;
        };
        match state {
            AgentState::Done | AgentState::Failed => {}
            AgentState::Thinking | AgentState::Acting => {
                if int(&*ctx.graph, id, v.remaining).unwrap_or(0) <= 0 {
                    self.finish(ctx, id, AgentState::Failed, "timeout");
                }
            }
            AgentState::Idle => {
                let steps = int(&*ctx.graph, id, v.steps).unwrap_or(0);
                let max = int(&*ctx.graph, id, v.max_steps).unwrap_or(0);
                if steps >= max {
                    return self.finish(ctx, id, AgentState::Failed, "max_steps");
                }
                let request = steps.saturating_add(1);
                match text(&*ctx.graph, id, v.policy).and_then(|p| Policy::parse(&p)) {
                    Some(Policy::External) => {
                        ctx.graph.set(id, v.steps, Value::Int(request));
                        ctx.graph.set(id, v.pending, Value::Int(request));
                        self.enter_wait(ctx, id, AgentState::Thinking);
                    }
                    Some(Policy::Scripted) => {
                        ctx.graph.set(id, v.steps, Value::Int(request));
                        ctx.graph.set(id, v.pending, Value::Int(request));
                        let view = agent_view(&*ctx.graph, v, id);
                        let decision = scripted.decide(&view, ctx.rng);
                        self.act(ctx, id, request, decision);
                    }
                    None => self.finish(ctx, id, AgentState::Failed, "invalid_policy"),
                }
            }
        }
    }
}
