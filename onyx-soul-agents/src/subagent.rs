//! Sistema determinista de delegación a subagentes (`spawn_agent`).
//!
//! Vive dentro del cerebro (0 dependencias externas): escucha `ev.tool_call`
//! para [`SPAWN_AGENT_TOOL`], suspende al agente padre en
//! [`AgentState::WaitingChildren`] (sin consumir su timeout) mientras emite
//! `ev.spawn` enlazado por `rel_parent`, y reanuda al padre con `ev.tool_result`
//! cuando todos los subagentes hijos de esa petición terminan.

use onyx_core::{Attr, AttrRegistry, ConceptId, Ctx, OnyxError, OnyxEvent, OnyxSystem, Value};

use crate::config::AgentConfig;
use crate::spec::{AgentSpec, Policy, spawn_event};
use crate::util::{current_tick, data, domain, int, text};
use crate::vocab::{AgentState, Vocab};

pub const SUBAGENT_NAME: &str = "agent_subagents";
pub const SPAWN_AGENT_TOOL: &str = "spawn_agent";

pub struct SubagentSystem {
    config: AgentConfig,
    vocab: Option<Vocab>,
    parent_req_attr: Option<Attr>,
    resolved_req_attr: Option<Attr>,
}

impl SubagentSystem {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            vocab: None,
            parent_req_attr: None,
            resolved_req_attr: None,
        }
    }
}

impl Default for SubagentSystem {
    fn default() -> Self {
        Self::new(AgentConfig::default())
    }
}

impl OnyxSystem for SubagentSystem {
    fn name(&self) -> &str {
        SUBAGENT_NAME
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.vocab = Some(Vocab::resolve_with(attrs, &self.config.vocab_prefix));
        self.parent_req_attr =
            Some(attrs.attr(&format!("{}.subagent.parent_req", self.config.vocab_prefix)));
        self.resolved_req_attr = Some(attrs.attr(&format!(
            "{}.subagent.resolved_req",
            self.config.vocab_prefix
        )));
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let (Some(v), Some(parent_req_attr), Some(resolved_req_attr)) =
            (self.vocab, self.parent_req_attr, self.resolved_req_attr)
        else {
            return;
        };
        if current_tick(ctx.inbox).is_none() {
            return;
        }

        // 1. Procesar peticiones `spawn_agent` y rechazos de spawn en `inbox`.
        for event in ctx.inbox {
            let OnyxEvent::Domain(d) = event else {
                continue;
            };
            if d.kind == v.ev_tool_call {
                self.handle_tool_call(ctx, &v, parent_req_attr, d);
            } else if d.kind == v.ev_rejected {
                self.handle_rejected_spawn(ctx, &v, resolved_req_attr, d);
            }
        }

        // 2. Comprobar agentes en `WaitingChildren` cuyos hijos hayan terminado.
        for parent in ctx.graph.query(&[v.state, v.pending]) {
            if text(&*ctx.graph, parent, v.state).as_deref()
                != Some(AgentState::WaitingChildren.as_str())
            {
                continue;
            }
            let Some(req) = int(&*ctx.graph, parent, v.pending) else {
                continue;
            };
            if req <= 0 {
                continue;
            }
            if int(&*ctx.graph, parent, resolved_req_attr) == Some(req) {
                continue;
            }

            let mut children = Vec::new();
            for child in ctx.graph.query(&[parent_req_attr, v.state]) {
                if int(&*ctx.graph, child, parent_req_attr) == Some(req)
                    && ctx.graph.related(child, v.rel_parent).contains(&parent)
                {
                    children.push(child);
                }
            }
            if children.is_empty() {
                continue;
            }

            let mut all_terminal = true;
            let mut any_failed = false;
            let mut lines = Vec::with_capacity(children.len());
            for child in children {
                let Some(state) =
                    text(&*ctx.graph, child, v.state).and_then(|s| AgentState::parse(&s))
                else {
                    all_terminal = false;
                    break;
                };
                if !state.is_terminal() {
                    all_terminal = false;
                    break;
                }
                if state == AgentState::Failed {
                    any_failed = true;
                }
                let cname = text(&*ctx.graph, child, v.name).unwrap_or_else(|| child.0.to_string());
                let cresult = text(&*ctx.graph, child, v.result).unwrap_or_default();
                lines.push(format!(
                    "[subagent `{cname}` state={}]: {cresult}",
                    state.as_str()
                ));
            }

            if all_terminal {
                ctx.graph.set(parent, resolved_req_attr, Value::Int(req));
                emit_tool_result(ctx, &v, parent, req, !any_failed, lines.join("\n"));
            }
        }
    }
}

impl SubagentSystem {
    fn handle_tool_call(
        &self,
        ctx: &mut Ctx,
        v: &Vocab,
        parent_req_attr: Attr,
        d: &onyx_core::DomainEvent,
    ) {
        let Some(Value::Text(tool)) = data(d, v.tool) else {
            return;
        };
        if tool != SPAWN_AGENT_TOOL {
            return;
        }
        let Some(parent) = d.subject else {
            return;
        };
        let Some(Value::Int(request)) = data(d, v.request) else {
            return;
        };
        if !ctx.graph.exists(parent)
            || text(&*ctx.graph, parent, v.state).as_deref() != Some(AgentState::Acting.as_str())
            || int(&*ctx.graph, parent, v.pending) != Some(*request)
        {
            return;
        }

        // Límite de profundidad a 1 nivel para evitar cascadas recursivas de subagentes.
        if !ctx.graph.related(parent, v.rel_parent).is_empty() {
            emit_tool_result(
                ctx,
                v,
                parent,
                *request,
                false,
                "nested subagent spawning is not permitted".to_string(),
            );
            return;
        }

        let raw_args = match data(d, v.args) {
            Some(Value::Text(s)) => s.as_str(),
            _ => "",
        };
        let (child_name, child_goal) = match parse_spawn_args(raw_args, parent, *request) {
            Ok(parsed) => parsed,
            Err(err) => {
                emit_tool_result(ctx, v, parent, *request, false, err);
                return;
            }
        };

        let tools: Vec<String> = ctx
            .graph
            .related(parent, v.rel_tool)
            .into_iter()
            .filter_map(|t| text(&*ctx.graph, t, v.tool_name))
            .filter(|name| name != SPAWN_AGENT_TOOL)
            .collect();
        let approval_tools: Vec<String> = ctx
            .graph
            .related(parent, v.rel_approval_tool)
            .into_iter()
            .filter_map(|t| text(&*ctx.graph, t, v.tool_name))
            .filter(|name| name != SPAWN_AGENT_TOOL)
            .collect();
        let policy = text(&*ctx.graph, parent, v.policy)
            .and_then(|p| Policy::parse(&p))
            .unwrap_or(Policy::External);

        let mut spec = AgentSpec::new(child_name, child_goal)
            .with_policy(policy)
            .with_tools(tools.iter().map(String::as_str))
            .with_approval_tools(approval_tools.iter().map(String::as_str))
            .with_extra(parent_req_attr, Value::Int(*request));
        if let Some(ms) = int(&*ctx.graph, parent, v.max_steps) {
            if ms > 0 {
                spec = spec.with_max_steps(ms);
            }
        }
        if let Some(tt) = int(&*ctx.graph, parent, v.timeout) {
            if tt > 0 {
                spec = spec.with_timeout_ticks(tt);
            }
        }

        ctx.graph.set(
            parent,
            v.state,
            Value::Text(AgentState::WaitingChildren.as_str().into()),
        );
        ctx.outbox.push(spawn_event(v, &spec, Some(parent)));
    }

    fn handle_rejected_spawn(
        &self,
        ctx: &mut Ctx,
        v: &Vocab,
        resolved_req_attr: Attr,
        d: &onyx_core::DomainEvent,
    ) {
        let Some(parent) = d.subject else {
            return;
        };
        if text(&*ctx.graph, parent, v.state).as_deref()
            != Some(AgentState::WaitingChildren.as_str())
        {
            return;
        }
        let Some(req) = int(&*ctx.graph, parent, v.pending) else {
            return;
        };
        if req <= 0 || int(&*ctx.graph, parent, resolved_req_attr) == Some(req) {
            return;
        }
        let reason = match data(d, v.reason) {
            Some(Value::Text(r)) => r.clone(),
            _ => "subagent spawn rejected".to_string(),
        };
        ctx.graph.set(parent, resolved_req_attr, Value::Int(req));
        emit_tool_result(
            ctx,
            v,
            parent,
            req,
            false,
            format!("failed to spawn subagent: {reason}"),
        );
    }
}

fn emit_tool_result(
    ctx: &mut Ctx,
    v: &Vocab,
    parent: ConceptId,
    request: i64,
    ok: bool,
    body: String,
) {
    let mut ev_data = vec![
        (v.request, Value::Int(request)),
        (v.ok, Value::Bool(ok)),
        (v.text, Value::Text(body)),
    ];
    if let Some(cid) = text(&*ctx.graph, parent, v.pending_call_id).filter(|s| !s.is_empty()) {
        ev_data.push((v.call_id, Value::Text(cid)));
    }
    ctx.outbox
        .push(domain(v.ev_tool_result, Some(parent), ev_data));
}

fn parse_spawn_args(
    raw: &str,
    parent: ConceptId,
    request: i64,
) -> Result<(String, String), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("spawn_agent requires a non-empty goal".to_string());
    }
    if trimmed.starts_with('{') {
        let goal = extract_json_string(trimmed, "goal")
            .ok_or_else(|| "spawn_agent JSON args missing string field `goal`".to_string())?;
        if goal.trim().is_empty() {
            return Err("spawn_agent `goal` cannot be empty".to_string());
        }
        let name = extract_json_string(trimmed, "name")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("subagent-{}-{request}", parent.0));
        return Ok((name, goal));
    }
    Ok((
        format!("subagent-{}-{request}", parent.0),
        trimmed.to_string(),
    ))
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let bytes = json.as_bytes();
    let needle = format!("\"{key}\"");
    let mut search_from = 0usize;
    while let Some(rel_idx) = json[search_from..].find(&needle) {
        let key_end = search_from + rel_idx + needle.len();
        let mut i = key_end;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b':' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'"' {
                return decode_json_string(&json[i + 1..]);
            }
        }
        search_from = key_end;
    }
    None
}

fn decode_json_string(rest: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            other => out.push(other),
        }
    }
    None
}
