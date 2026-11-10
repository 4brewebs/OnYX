//! Sistema que crea agentes a partir de eventos `<prefix>.ev.spawn`.

use onyx_core::{
    AttrRegistry, ConceptId, Ctx, DomainEvent, Graph, OnyxError, OnyxEvent, OnyxSystem, Value,
};

use crate::config::AgentConfig;
use crate::spec::Policy;
use crate::util::{current_tick, data, domain, text};
use crate::vocab::{AgentState, Vocab};

pub const SPAWNER_NAME: &str = "agent_spawner";

/// Crea conceptos agente. No ejecuta agentes: eso lo hace `AgentRuntime`.
///
/// Debe ir antes que el runtime en el slice de sistemas para que un agente
/// recién creado pueda decidir en el mismo tick.
pub struct AgentSpawner {
    config: AgentConfig,
    vocab: Option<Vocab>,
}

impl AgentSpawner {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            vocab: None,
        }
    }
}

impl OnyxSystem for AgentSpawner {
    fn name(&self) -> &str {
        SPAWNER_NAME
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
        let inbox = ctx.inbox;
        for event in inbox {
            if let OnyxEvent::Domain(d) = event {
                if d.kind == v.ev_spawn {
                    self.spawn(ctx, &v, now, d);
                }
            }
        }
    }
}

impl AgentSpawner {
    /// Motivo y, si el evento traía un nombre en texto, ese nombre.
    fn reject(&self, ctx: &mut Ctx, v: &Vocab, d: &DomainEvent, reason: &str) {
        let mut payload = vec![(v.reason, Value::Text(reason.to_string()))];
        if let Some(Value::Text(name)) = data(d, v.name) {
            payload.push((v.name, Value::Text(name.clone())));
        }
        ctx.outbox.push(domain(v.ev_rejected, d.subject, payload));
    }

    fn live_agents(&self, ctx: &Ctx, v: &Vocab) -> usize {
        ctx.graph
            .query(&[v.state])
            .into_iter()
            .filter(|id| {
                let state = text(&*ctx.graph, *id, v.state).and_then(|s| AgentState::parse(&s));
                !state.is_some_and(AgentState::is_terminal)
            })
            .count()
    }

    fn spawn(&self, ctx: &mut Ctx, v: &Vocab, now: u64, d: &DomainEvent) {
        // El cupo se comprueba antes que la spec: una spec inválida con el
        // cupo lleno se rechaza como `max_agents`.
        if self.live_agents(ctx, v) >= self.config.max_agents {
            return self.reject(ctx, v, d, "max_agents");
        }

        let (Some(Value::Text(name)), Some(Value::Text(goal))) = (data(d, v.name), data(d, v.goal))
        else {
            return self.reject(ctx, v, d, "invalid_spec");
        };
        let policy = match data(d, v.policy) {
            None => Policy::External,
            Some(Value::Text(p)) => match Policy::parse(p) {
                Some(p) => p,
                None => return self.reject(ctx, v, d, "invalid_policy"),
            },
            Some(_) => return self.reject(ctx, v, d, "invalid_policy"),
        };
        let max_steps = match data(d, v.max_steps) {
            None => self.config.default_max_steps,
            Some(Value::Int(i)) if *i >= 1 => *i,
            Some(_) => return self.reject(ctx, v, d, "invalid_spec"),
        };
        let timeout = match data(d, v.timeout) {
            None => self.config.default_timeout_ticks,
            Some(Value::Int(i)) if *i >= 1 => *i,
            Some(_) => return self.reject(ctx, v, d, "invalid_spec"),
        };
        let tool_names: Vec<String> = match data(d, v.tools_csv) {
            Some(Value::Text(csv)) => csv
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };

        let id = ctx.graph.create();
        // Las propiedades de la app van primero: las del framework las pisan.
        for (key, value) in &d.data {
            if !v.is_reserved(*key) {
                ctx.graph.set(id, *key, value.clone());
            }
        }
        ctx.graph.set(id, v.name, Value::Text(name.clone()));
        ctx.graph.set(id, v.goal, Value::Text(goal.clone()));
        ctx.graph
            .set(id, v.policy, Value::Text(policy.as_str().into()));
        ctx.graph
            .set(id, v.state, Value::Text(AgentState::Idle.as_str().into()));
        ctx.graph.set(id, v.steps, Value::Int(0));
        ctx.graph.set(id, v.max_steps, Value::Int(max_steps));
        ctx.graph.set(id, v.pending, Value::Int(0));
        ctx.graph.set(id, v.remaining, Value::Int(0));
        ctx.graph.set(id, v.timeout, Value::Int(timeout));
        // Un tick mayor que `i64::MAX` se satura en vez de volverse negativo.
        let born = i64::try_from(now).unwrap_or(i64::MAX);
        ctx.graph.set(id, v.born, Value::Int(born));
        ctx.graph.set(id, v.mem_count, Value::Int(0));

        for tool in &tool_names {
            let tool_id = find_or_create_tool(ctx.graph, v, tool);
            ctx.graph.relate(id, v.rel_tool, tool_id);
        }

        if let Some(parent) = d.subject {
            if ctx.graph.exists(parent) && ctx.graph.get(parent, v.state).is_some() {
                ctx.graph.relate(id, v.rel_parent, parent);
            }
        }

        ctx.outbox.push(domain(
            v.ev_spawned,
            Some(id),
            vec![(v.name, Value::Text(name.clone()))],
        ));
    }
}

/// Las herramientas son conceptos compartidos: un nombre, un concepto.
/// La búsqueda recorre en orden de id, así que el resultado es determinista.
fn find_or_create_tool(graph: &mut dyn Graph, v: &Vocab, name: &str) -> ConceptId {
    for id in graph.query(&[v.tool_name]) {
        if text(&*graph, id, v.tool_name).as_deref() == Some(name) {
            return id;
        }
    }
    let id = graph.create();
    graph.set(id, v.tool_name, Value::Text(name.to_string()));
    id
}
