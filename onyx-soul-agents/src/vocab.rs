//! Vocabulary for agent concepts.
//!
//! The application can extend this by resolving additional attributes.

use onyx_core::{Attr, AttrRegistry};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Thinking,
    Acting,
    Done,
    Failed,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Thinking => "thinking",
            Self::Acting => "acting",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "idle" => Self::Idle,
            "thinking" => Self::Thinking,
            "acting" => Self::Acting,
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Vocab {
    pub name: Attr,
    pub goal: Attr,
    pub state: Attr,
    pub steps: Attr,
    pub max_steps: Attr,
    pub timeout: Attr,
    pub born: Attr,
    pub result: Attr,
    pub tools_csv: Attr,

    pub rel_tool: Attr,
    pub rel_memory: Attr,
    pub rel_parent: Attr,

    pub ev_spawn: Attr,
    pub ev_spawned: Attr,
    pub ev_rejected: Attr,
    pub ev_tool_call: Attr,
    pub ev_tool_result: Attr,
    pub ev_finished: Attr,
    pub ev_failed: Attr,

    pub request: Attr,
    pub tool: Attr,
    pub args: Attr,
    pub text: Attr,
    pub reason: Attr,
}

impl Vocab {
    pub fn resolve(attrs: &mut dyn AttrRegistry) -> Self {
        let mut a = |name: &str| attrs.attr(name);
        Self {
            name: a("agent.name"),
            goal: a("agent.goal"),
            state: a("agent.state"),
            steps: a("agent.steps"),
            max_steps: a("agent.max_steps"),
            timeout: a("agent.timeout"),
            born: a("agent.born"),
            result: a("agent.result"),
            tools_csv: a("agent.tools_csv"),

            rel_tool: a("agent.has_tool"),
            rel_memory: a("agent.has_memory"),
            rel_parent: a("agent.parent"),

            ev_spawn: a("agent.ev.spawn"),
            ev_spawned: a("agent.ev.spawned"),
            ev_rejected: a("agent.ev.spawn_rejected"),
            ev_tool_call: a("agent.ev.tool_call"),
            ev_tool_result: a("agent.ev.tool_result"),
            ev_finished: a("agent.ev.finished"),
            ev_failed: a("agent.ev.failed"),

            request: a("agent.request"),
            tool: a("agent.tool"),
            args: a("agent.args"),
            text: a("agent.text"),
            reason: a("agent.reason"),
        }
    }
}
