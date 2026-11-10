//! Explicit execution root and restorable logical agent births.
//!
//! An execution that will use operation identity starts with an explicit root
//! concept, created by [`begin_event`] on an empty graph before the first
//! recoverable snapshot. The root lives under fixed soul names
//! (`onyx.agents.root.*`), independent of `vocab_prefix`, so changing the
//! prefix cannot hide it. It stores the app-supplied [`ExecutionId`], the
//! exact prefix and configuration, an app-supplied compatibility identifier,
//! and `next_birth`.
//!
//! Each agent created with an execution configuration stores its execution
//! and its [`AgentBirth`] as text under `onyx.agents.agent.*`. The spawner
//! reserves the birth and creates the agent in the same `run`, so both belong
//! to the same snapshot. Births are never returned on destroy and never wrap.
//! Restoring a snapshot and repeating the same spawn history reproduces the
//! same births, even when the graph assigns different `ConceptId`s.
//!
//! # What this does not establish
//!
//! A birth counter travels with the snapshot, so it rewinds with it; so does
//! the request counter of an agent. Equal numbers or equal descriptors do not
//! prove that a continuation is the same work. Proving the receiver's
//! external frontier (complete inventory at rest, a single dispatcher,
//! exact atomic deduplication before every send) and verified TR2 replay are
//! separate, unimplemented work. Until then, a continuation whose external
//! history is ahead of the snapshot or unknown must be rejected before any
//! dispatch, on a new host and on the same host. Nothing here enables
//! external effects, and no fork or new execution is created implicitly.

use std::{fmt, num::NonZeroU64};

use onyx_core::{Attr, AttrRegistry, ConceptId, GraphView, OnyxEvent, Value};

use crate::config::AgentConfig;
use crate::operation::{ExecutionId, OperationError};
use crate::util::domain;
use crate::vocab::Vocab;

/// Fixed soul namespace for the root and agent identity, never prefixed.
pub const ROOT_NAMESPACE: &str = "onyx.agents";

const ROOT_SCHEMA: &str = "root.v1";

/// Logical birth number of an agent within one execution. Starts at 1.
///
/// Unlike `ConceptId`, it is restored with the graph and reproduced by the
/// same spawn history. It only locates history; it is not proof of intent.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentBirth(NonZeroU64);

impl AgentBirth {
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    pub fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub fn get(self) -> NonZeroU64 {
        self.0
    }

    /// Canonical decimal text, without sign or leading zeros.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        parse_canonical(text).and_then(NonZeroU64::new).map(Self)
    }
}

impl fmt::Display for AgentBirth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Execution identity carried by an agent: what an operation key needs
/// instead of its `ConceptId`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentity {
    pub execution: ExecutionId,
    pub birth: AgentBirth,
}

/// Operational part of [`AgentConfig`]: the receiver scope and an
/// app-supplied compatibility identifier.
///
/// The soul compares the stored configuration exactly, but cannot inspect
/// policies, memory, tools or their versions. `compat` is the app's
/// declaration that such opaque semantics are compatible; the soul does not
/// verify it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionConfig {
    execution: ExecutionId,
    compat: String,
}

impl ExecutionConfig {
    pub fn new(execution: ExecutionId, compat: impl Into<String>) -> Result<Self, RootError> {
        let compat = compat.into();
        if compat.is_empty() {
            return Err(RootError::EmptyCompat);
        }
        Ok(Self { execution, compat })
    }

    pub fn execution(&self) -> &ExecutionId {
        &self.execution
    }

    pub fn compat(&self) -> &str {
        &self.compat
    }
}

/// Validated projection of the root. Owned; changing it does not change the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionRoot {
    pub concept: ConceptId,
    pub execution: ExecutionId,
    /// Birth the next valid spawn receives. Exhausted at `u64::MAX`, which is
    /// never assigned.
    pub next_birth: AgentBirth,
}

/// Typed reasons to refuse a root, a begin or a spawn. The spawner publishes
/// [`RootError::reason`] in its rejection events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootError {
    EmptyCompat,
    /// The configuration has no execution, or a legacy spawner found a root.
    NotConfigured,
    /// Begin is only valid on a graph without concepts.
    NotEmpty,
    Missing,
    Duplicate(Vec<ConceptId>),
    Corrupt {
        concept: ConceptId,
        field: &'static str,
    },
    Incompatible {
        field: &'static str,
        stored: String,
        current: String,
    },
    /// An agent of this prefix without execution and birth.
    MissingBirth(ConceptId),
    /// A record of another execution: forks are not supported.
    ForeignExecution(ConceptId),
    DuplicateBirth(AgentBirth),
    /// A birth not below the root counter.
    BirthAhead {
        concept: ConceptId,
        birth: AgentBirth,
    },
    Exhausted,
    Operation(OperationError),
}

impl RootError {
    /// Stable text for the `reason` of rejection events.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::EmptyCompat => "empty_compat",
            Self::NotConfigured => "execution_not_configured",
            Self::NotEmpty => "graph_not_empty",
            Self::Missing => "no_execution_root",
            Self::Duplicate(_) => "duplicate_execution_root",
            Self::Corrupt { .. } => "corrupt_execution_root",
            Self::Incompatible { .. } => "incompatible_execution_root",
            Self::MissingBirth(_) => "missing_birth",
            Self::ForeignExecution(_) => "foreign_execution",
            Self::DuplicateBirth(_) => "duplicate_birth",
            Self::BirthAhead { .. } => "birth_ahead",
            Self::Exhausted => "birth_exhausted",
            Self::Operation(_) => "invalid_operation_log",
        }
    }
}

impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(ids) => write!(f, "duplicate execution root: {ids:?}"),
            Self::Corrupt { concept, field } => {
                write!(f, "invalid execution identity {}: {field}", concept.0)
            }
            Self::Incompatible {
                field,
                stored,
                current,
            } => write!(
                f,
                "incompatible {field}: stored {stored:?}, current {current:?}"
            ),
            Self::MissingBirth(id) => write!(f, "agent {} has no birth", id.0),
            Self::ForeignExecution(id) => write!(f, "record {} belongs to another execution", id.0),
            Self::DuplicateBirth(b) => write!(f, "duplicate agent birth {b}"),
            Self::BirthAhead { concept, birth } => {
                write!(f, "birth {birth} of {} is not below next_birth", concept.0)
            }
            Self::Operation(e) => write!(f, "invalid operation log: {e}"),
            other => f.write_str(other.reason()),
        }
    }
}

impl std::error::Error for RootError {}

impl From<OperationError> for RootError {
    fn from(error: OperationError) -> Self {
        Self::Operation(error)
    }
}

/// Fixed-name attributes of the root and of agent identity.
#[derive(Copy, Clone, Debug)]
pub struct RootSchema {
    schema: Attr,
    execution: Attr,
    prefix: Attr,
    config: Attr,
    compat: Attr,
    next_birth: Attr,
    agent_execution: Attr,
    agent_birth: Attr,
}

impl RootSchema {
    pub fn resolve<A: AttrRegistry + ?Sized>(attrs: &mut A) -> Self {
        let mut a = |name: &str| attrs.attr(&format!("{ROOT_NAMESPACE}.{name}"));
        Self {
            schema: a("root.schema"),
            execution: a("root.execution"),
            prefix: a("root.prefix"),
            config: a("root.config"),
            compat: a("root.compat"),
            next_birth: a("root.next_birth"),
            agent_execution: a("agent.execution"),
            agent_birth: a("agent.birth"),
        }
    }

    /// The eight fixed control attributes. Spawn extras cannot set them.
    pub fn is_reserved(&self, attr: Attr) -> bool {
        [
            self.schema,
            self.execution,
            self.prefix,
            self.config,
            self.compat,
            self.next_birth,
            self.agent_execution,
            self.agent_birth,
        ]
        .contains(&attr)
    }

    /// Execution and birth stored on `agent`. `None` if it has neither;
    /// a partial or malformed identity is `Corrupt`. Reads one concept only:
    /// it does not validate the root, uniqueness, or that `agent` is an agent.
    /// Use [`execution_root`] first when the answer feeds an operation key.
    pub fn identity<G: GraphView + ?Sized>(
        &self,
        graph: &G,
        agent: ConceptId,
    ) -> Result<Option<AgentIdentity>, RootError> {
        let execution = graph.get(agent, self.agent_execution);
        let birth = graph.get(agent, self.agent_birth);
        let corrupt = |field| RootError::Corrupt {
            concept: agent,
            field,
        };
        match (execution, birth) {
            (None, None) => Ok(None),
            (Some(Value::Text(e)), Some(Value::Text(b))) => Ok(Some(AgentIdentity {
                execution: ExecutionId::new(e).map_err(|_| corrupt("agent.execution"))?,
                birth: AgentBirth::parse(&b).ok_or_else(|| corrupt("agent.birth"))?,
            })),
            (Some(Value::Text(_)), _) => Err(corrupt("agent.birth")),
            _ => Err(corrupt("agent.execution")),
        }
    }

    /// The single live concept that carries `identity`, if any.
    ///
    /// Never returns an ambiguous answer: two concepts with the same identity
    /// are `DuplicateBirth`, and a malformed identity on any concept is
    /// `Corrupt`. It does not check the root, the configuration, or that the
    /// concept is an agent of a given prefix: call it only after
    /// [`execution_root`] succeeds on the same graph state. A located agent is
    /// not proof that a result belongs to the current attempt.
    pub fn locate<G: GraphView + ?Sized>(
        &self,
        graph: &G,
        identity: &AgentIdentity,
    ) -> Result<Option<ConceptId>, RootError> {
        let mut found = None;
        let mut candidates = graph.query(&[self.agent_execution]);
        candidates.extend(graph.query(&[self.agent_birth]));
        candidates.sort_by_key(|id| id.0);
        candidates.dedup();
        for id in candidates {
            if self.identity(graph, id)?.as_ref() == Some(identity) {
                if found.is_some() {
                    return Err(RootError::DuplicateBirth(identity.birth));
                }
                found = Some(id);
            }
        }
        Ok(found)
    }

    pub(crate) fn exists<G: GraphView + ?Sized>(&self, graph: &G) -> bool {
        !graph.query(&[self.schema]).is_empty()
    }

    pub(crate) fn write_root(
        &self,
        graph: &mut dyn onyx_core::Graph,
        config: &AgentConfig,
        execution: &ExecutionConfig,
    ) -> ConceptId {
        let id = graph.create();
        let text = |s: &str| Value::Text(s.to_string());
        graph.set(id, self.execution, text(execution.execution.as_str()));
        graph.set(id, self.prefix, text(&config.vocab_prefix));
        graph.set(id, self.config, text(&config_text(config)));
        graph.set(id, self.compat, text(&execution.compat));
        graph.set(id, self.next_birth, text(&AgentBirth::FIRST.to_string()));
        graph.set(id, self.schema, text(ROOT_SCHEMA));
        id
    }

    /// Writes the agent identity and advances the root. The caller has
    /// already checked that `root.next_birth` can advance.
    pub(crate) fn write_birth(
        &self,
        graph: &mut dyn onyx_core::Graph,
        root: &ExecutionRoot,
        next: AgentBirth,
        agent: ConceptId,
    ) {
        graph.set(
            agent,
            self.agent_execution,
            Value::Text(root.execution.as_str().into()),
        );
        graph.set(
            agent,
            self.agent_birth,
            Value::Text(root.next_birth.to_string()),
        );
        graph.set(root.concept, self.next_birth, Value::Text(next.to_string()));
    }
}

/// Event that asks the spawner of `vocab`'s prefix to create the root.
///
/// Push it on a fresh, empty graph, and let it be served before requesting
/// the first recoverable snapshot. Resuming never uses it: a restored graph
/// must already contain its root.
pub fn begin_event(vocab: &Vocab) -> OnyxEvent {
    domain(vocab.ev_begin, None, Vec::new())
}

/// Validates the single root against `config` and every identity it governs.
///
/// Rejects, without repair: missing configuration, a missing root (even on an
/// empty graph), duplicate or malformed roots, a different execution, prefix,
/// configuration or compatibility identifier, agents of this prefix without a
/// birth, records of another execution, duplicate live births, and births of
/// agents or of this prefix's operations that are not below `next_birth`.
/// Read-only. Success is not permission to dispatch external effects.
pub fn execution_root<G: GraphView + ?Sized>(
    graph: &G,
    vocab: &Vocab,
    config: &AgentConfig,
) -> Result<ExecutionRoot, RootError> {
    let expected = config.execution.as_ref().ok_or(RootError::NotConfigured)?;
    let r = &vocab.root;
    let root = match graph.query(&[r.schema]).as_slice() {
        [] => return Err(RootError::Missing),
        [one] => *one,
        many => return Err(RootError::Duplicate(many.to_vec())),
    };
    let corrupt = |field| RootError::Corrupt {
        concept: root,
        field,
    };
    let text = |attr, field| match graph.get(root, attr) {
        Some(Value::Text(value)) => Ok(value),
        _ => Err(corrupt(field)),
    };
    if text(r.schema, "root.schema")? != ROOT_SCHEMA {
        return Err(corrupt("root.schema"));
    }
    let execution = ExecutionId::new(text(r.execution, "root.execution")?)
        .map_err(|_| corrupt("root.execution"))?;
    let next_birth = AgentBirth::parse(&text(r.next_birth, "root.next_birth")?)
        .ok_or_else(|| corrupt("root.next_birth"))?;
    let compare = |field, stored: String, current: &str| {
        if stored == current {
            Ok(())
        } else {
            Err(RootError::Incompatible {
                field,
                stored,
                current: current.to_string(),
            })
        }
    };
    compare(
        "execution",
        execution.as_str().to_string(),
        expected.execution.as_str(),
    )?;
    compare(
        "prefix",
        text(r.prefix, "root.prefix")?,
        &config.vocab_prefix,
    )?;
    compare(
        "config",
        text(r.config, "root.config")?,
        &config_text(config),
    )?;
    compare("compat", text(r.compat, "root.compat")?, &expected.compat)?;

    let check_birth = |concept, identity: &AgentIdentity| {
        if identity.execution != execution {
            Err(RootError::ForeignExecution(concept))
        } else if identity.birth >= next_birth {
            Err(RootError::BirthAhead {
                concept,
                birth: identity.birth,
            })
        } else {
            Ok(())
        }
    };
    let mut births = std::collections::BTreeSet::new();
    let mut agents = graph.query(&[vocab.state]);
    agents.extend(graph.query(&[r.agent_execution]));
    agents.extend(graph.query(&[r.agent_birth]));
    agents.sort_by_key(|id| id.0);
    agents.dedup();
    for agent in agents {
        if graph.get(agent, vocab.state).is_none() {
            return Err(RootError::Corrupt {
                concept: agent,
                field: "agent.state",
            });
        }
        let identity = r
            .identity(graph, agent)?
            .ok_or(RootError::MissingBirth(agent))?;
        check_birth(agent, &identity)?;
        if !births.insert(identity.birth) {
            return Err(RootError::DuplicateBirth(identity.birth));
        }
    }
    for operation in vocab.operations.entries(graph)? {
        let identity = AgentIdentity {
            execution: operation.key.execution().clone(),
            birth: operation.key.birth(),
        };
        check_birth(operation.id, &identity)?;
    }
    Ok(ExecutionRoot {
        concept: root,
        execution,
        next_birth,
    })
}

/// Exact configuration, without the saturation of `AgentConfig::to_params`.
/// The prefix and execution fields are compared separately.
fn config_text(c: &AgentConfig) -> String {
    format!(
        "v1;max_agents={};default_max_steps={};default_timeout_ticks={};memory_keep_last={}",
        c.max_agents, c.default_max_steps, c.default_timeout_ticks, c.memory_keep_last
    )
}

pub(crate) fn parse_canonical(text: &str) -> Option<u64> {
    let value: u64 = text.parse().ok()?;
    (value.to_string() == text).then_some(value)
}
