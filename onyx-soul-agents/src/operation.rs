//! Graph-backed records for external tool operations.
//!
//! This module stores identity, the exact request and its known outcome. It
//! neither dispatches tools nor proves that an external effect is safe to repeat.
//! The agent part of the key is its logical [`AgentBirth`] from the execution
//! root ([`crate::root`]), never its `ConceptId`.
//! Runtime integration, receiver capabilities and strict replay are separate
//! work; the existing runtime does not create these records automatically yet.
//!
//! Mutating methods are intended for systems, using `Ctx.graph`. Drivers only
//! read projections and must return observations through events. Payloads are
//! opaque tool-owned strings; do not put credentials in persisted payloads.

use std::{collections::BTreeSet, fmt, num::NonZeroU64};

use onyx_core::{Attr, AttrRegistry, ConceptId, Graph, GraphView, Value};

use crate::config::DEFAULT_PREFIX;
use crate::root::{AgentBirth, parse_canonical};

/// App-supplied execution identity, not a clock or a random seed.
///
/// Reuse it when restoring the same execution; use a distinct value for new
/// work. The caller must provide uniqueness across executions at its receiver.
///
/// Begin each value once. The soul does not detect a second `begin` with the
/// same value, for example after restoring a snapshot taken before the first
/// `begin`. Births then start again at 1 for different work.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExecutionId(String);

impl ExecutionId {
    pub fn new(value: impl Into<String>) -> Result<Self, OperationError> {
        let value = value.into();
        if value.is_empty() {
            return Err(OperationError::EmptyExecutionId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identity in the tool channel: `(execution, birth, request)`. Decider
/// requests are a separate channel, so the channel is implicit in the type.
///
/// The agent's birth is historical identity, not a live graph reference: it
/// survives `destroy` and does not depend on the `ConceptId` the graph assigns
/// after a restore. Use [`crate::RootSchema::locate`] to find the live agent.
/// The birth and the request counter are persisted without narrowing to `i64`.
///
/// Both counters rewind with a snapshot. An equal key, or an equal key with
/// an equal descriptor, does not prove that a continuation is the same work.
/// Receiver deduplication by this key still needs the external frontier and
/// replay checks that are not implemented yet; see [`crate::root`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OperationKey {
    execution: ExecutionId,
    birth: AgentBirth,
    request: NonZeroU64,
}

impl OperationKey {
    pub fn new(execution: ExecutionId, birth: AgentBirth, request: NonZeroU64) -> Self {
        Self {
            execution,
            birth,
            request,
        }
    }

    pub fn execution(&self) -> &ExecutionId {
        &self.execution
    }
    pub fn birth(&self) -> AgentBirth {
        self.birth
    }
    pub fn request(&self) -> NonZeroU64 {
        self.request
    }
}

/// Exact tool-owned payload and the registered receiver route.
///
/// Strings are not trimmed, parsed or normalized. Changing any one of them for
/// the same key is a conflict, even when a tool would consider them equivalent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDescriptor {
    tool: String,
    args: String,
    route: String,
}

impl ToolDescriptor {
    pub fn new(
        tool: impl Into<String>,
        args: impl Into<String>,
        route: impl Into<String>,
    ) -> Result<Self, OperationError> {
        let tool = tool.into();
        let route = route.into();
        if tool.is_empty() {
            return Err(OperationError::EmptyTool);
        }
        if route.is_empty() {
            return Err(OperationError::EmptyRoute);
        }
        Ok(Self {
            tool,
            args: args.into(),
            route,
        })
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }
    pub fn args(&self) -> &str {
        &self.args
    }
    pub fn route(&self) -> &str {
        &self.route
    }
}

/// A known receiver outcome, including application-level tool failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    Success(String),
    Failure(String),
}

/// Uncertainty and conflict belong to an operation, not to the waiting agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationState {
    Pending,
    Uncertain,
    Resolved(ToolOutcome),
    /// Conflict that later writes to this graph do not clear; any previously
    /// known outcome remains available. Importing an older snapshot can bring
    /// back an earlier state: this is not an external, irreversible journal.
    Conflict {
        previous: Option<ToolOutcome>,
    },
}

/// Owned read projection; changing it does not change the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOperation {
    /// Reacquire this graph id by key after importing a snapshot.
    pub id: ConceptId,
    pub key: OperationKey,
    pub descriptor: ToolDescriptor,
    pub state: OperationState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationError {
    EmptyExecutionId,
    EmptyTool,
    EmptyRoute,
    Missing(OperationKey),
    DuplicateKey(OperationKey),
    DescriptorConflict(OperationKey),
    OutcomeConflict(OperationKey),
    Blocked(OperationKey),
    Corrupt {
        concept: ConceptId,
        field: &'static str,
    },
}

impl fmt::Display for OperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExecutionId => f.write_str("empty execution identity"),
            Self::EmptyTool => f.write_str("empty tool name"),
            Self::EmptyRoute => f.write_str("empty receiver route"),
            Self::Missing(key) => write!(f, "missing tool operation: {key:?}"),
            Self::DuplicateKey(key) => write!(f, "duplicate tool operation key: {key:?}"),
            Self::DescriptorConflict(key) => write!(f, "tool request conflict: {key:?}"),
            Self::OutcomeConflict(key) => write!(f, "tool outcome conflict: {key:?}"),
            Self::Blocked(key) => write!(f, "tool operation has a conflict in this graph: {key:?}"),
            Self::Corrupt { concept, field } => {
                write!(f, "invalid tool operation {}: {field}", concept.0)
            }
        }
    }
}

impl std::error::Error for OperationError {}

const SCHEMA: &str = "tool.v2";

/// Soul-owned schema under `<prefix>.operation.*`, version `tool.v2`.
///
/// Records of the earlier `tool.v1` schema, keyed by `ConceptId`, are
/// rejected as `Corrupt`, not reinterpreted. The log creates a separate
/// concept for each key. It never deletes records, touches conversation
/// memory, or changes an agent. No `Value::Concept` or
/// relation to the agent is stored: destroying it cannot erase this identity.
/// Tool payloads cannot supply control attributes through this API.
///
/// Attribute handles belong to the graph registry. Resolve a new log for a
/// different graph; importing into the same graph preserves registered handles.
/// Corrupt records and duplicate keys are rejected, not silently repaired.
#[derive(Copy, Clone, Debug)]
pub struct OperationLog {
    schema: Attr,
    execution: Attr,
    birth: Attr,
    request: Attr,
    tool: Attr,
    args: Attr,
    route: Attr,
    state: Attr,
    outcome_kind: Attr,
    outcome_text: Attr,
}

impl OperationLog {
    pub fn resolve<A: AttrRegistry + ?Sized>(attrs: &mut A) -> Self {
        Self::resolve_with(attrs, DEFAULT_PREFIX)
    }

    pub fn resolve_with<A: AttrRegistry + ?Sized>(attrs: &mut A, prefix: &str) -> Self {
        let mut a = |name: &str| attrs.attr(&format!("{prefix}.operation.{name}"));
        Self {
            schema: a("schema"),
            execution: a("execution"),
            birth: a("birth"),
            request: a("request"),
            tool: a("tool"),
            args: a("args"),
            route: a("route"),
            state: a("state"),
            outcome_kind: a("outcome_kind"),
            outcome_text: a("outcome_text"),
        }
    }

    /// Control fields owned by the soul, not tool payload or agent extras.
    ///
    /// Matches only this log's ten resolved fields. It neither reserves the whole
    /// textual namespace nor fields belonging to another vocabulary prefix.
    pub fn is_reserved(&self, attr: Attr) -> bool {
        [
            self.schema,
            self.execution,
            self.birth,
            self.request,
            self.tool,
            self.args,
            self.route,
            self.state,
            self.outcome_kind,
            self.outcome_text,
        ]
        .contains(&attr)
    }

    /// All records in graph-id order, validated before returning any projection.
    pub fn entries<G: GraphView + ?Sized>(
        &self,
        graph: &G,
    ) -> Result<Vec<ToolOperation>, OperationError> {
        let mut seen = BTreeSet::new();
        let mut entries = Vec::new();
        for id in graph.query(&[self.schema]) {
            let operation = self.read(graph, id)?;
            if !seen.insert(operation.key.clone()) {
                return Err(OperationError::DuplicateKey(operation.key));
            }
            entries.push(operation);
        }
        Ok(entries)
    }

    pub fn find<G: GraphView + ?Sized>(
        &self,
        graph: &G,
        key: &OperationKey,
    ) -> Result<Option<ToolOperation>, OperationError> {
        Ok(self
            .entries(graph)?
            .into_iter()
            .find(|operation| &operation.key == key))
    }

    /// Persist before attempting an effect. Equal repeated requests reuse the
    /// record. A changed descriptor marks a conflict in this graph, keeps the
    /// original request/outcome and returns an error. No dispatch happens here.
    /// Equality of descriptors does not prove equality of intent.
    pub fn record(
        &self,
        graph: &mut dyn Graph,
        key: OperationKey,
        descriptor: ToolDescriptor,
    ) -> Result<ToolOperation, OperationError> {
        if let Some(mut operation) = self.find(graph, &key)? {
            if operation.descriptor != descriptor {
                let previous = known_outcome(&operation.state).cloned();
                operation.state = OperationState::Conflict { previous };
                self.write_state(graph, &operation);
                return Err(OperationError::DescriptorConflict(key));
            }
            if matches!(operation.state, OperationState::Conflict { .. }) {
                return Err(OperationError::Blocked(key));
            }
            return Ok(operation);
        }
        let id = graph.create();
        graph.set(
            id,
            self.execution,
            Value::Text(key.execution.as_str().into()),
        );
        graph.set(id, self.birth, Value::Text(key.birth.to_string()));
        graph.set(id, self.request, Value::Text(key.request.to_string()));
        graph.set(id, self.tool, Value::Text(descriptor.tool.clone()));
        graph.set(id, self.args, Value::Text(descriptor.args.clone()));
        graph.set(id, self.route, Value::Text(descriptor.route.clone()));
        let operation = ToolOperation {
            id,
            key,
            descriptor,
            state: OperationState::Pending,
        };
        self.write_state(graph, &operation);
        graph.set(id, self.schema, Value::Text(SCHEMA.into()));
        Ok(operation)
    }

    /// Record unknown outcome without discarding identity or overwriting a known
    /// result. This does not grant permission to retry an external effect.
    pub fn mark_uncertain(
        &self,
        graph: &mut dyn Graph,
        key: &OperationKey,
    ) -> Result<ToolOperation, OperationError> {
        let mut operation = self.required(graph, key)?;
        match operation.state {
            OperationState::Conflict { .. } => return Err(OperationError::Blocked(key.clone())),
            OperationState::Pending => {
                operation.state = OperationState::Uncertain;
                self.write_state(graph, &operation);
            }
            OperationState::Uncertain | OperationState::Resolved(_) => {}
        }
        Ok(operation)
    }

    /// Record a late or repeated observation independently of the agent's life.
    /// Equal outcomes are idempotent; different outcomes mark a conflict while
    /// preserving the first known outcome. No method of this log clears a
    /// conflict, but importing an older snapshot replaces the whole graph and
    /// can return the record to an earlier state.
    pub fn complete(
        &self,
        graph: &mut dyn Graph,
        key: &OperationKey,
        outcome: ToolOutcome,
    ) -> Result<ToolOperation, OperationError> {
        let mut operation = self.required(graph, key)?;
        match &operation.state {
            OperationState::Conflict { .. } => return Err(OperationError::Blocked(key.clone())),
            OperationState::Resolved(previous) if previous != &outcome => {
                operation.state = OperationState::Conflict {
                    previous: Some(previous.clone()),
                };
                self.write_state(graph, &operation);
                return Err(OperationError::OutcomeConflict(key.clone()));
            }
            OperationState::Resolved(_) => return Ok(operation),
            OperationState::Pending | OperationState::Uncertain => {}
        }
        operation.state = OperationState::Resolved(outcome);
        self.write_state(graph, &operation);
        Ok(operation)
    }

    fn required<G: GraphView + ?Sized>(
        &self,
        graph: &G,
        key: &OperationKey,
    ) -> Result<ToolOperation, OperationError> {
        self.find(graph, key)?
            .ok_or_else(|| OperationError::Missing(key.clone()))
    }

    fn read<G: GraphView + ?Sized>(
        &self,
        graph: &G,
        id: ConceptId,
    ) -> Result<ToolOperation, OperationError> {
        let invalid = |field| OperationError::Corrupt { concept: id, field };
        let text = |attr, field| match graph.get(id, attr) {
            Some(Value::Text(value)) => Ok(value),
            _ => Err(invalid(field)),
        };
        if text(self.schema, "schema")? != SCHEMA {
            return Err(invalid("schema"));
        }
        let execution = ExecutionId::new(text(self.execution, "execution")?)
            .map_err(|_| invalid("execution"))?;
        let birth =
            AgentBirth::parse(&text(self.birth, "birth")?).ok_or_else(|| invalid("birth"))?;
        let request = parse_canonical(&text(self.request, "request")?)
            .and_then(NonZeroU64::new)
            .ok_or_else(|| invalid("request"))?;
        let descriptor = ToolDescriptor::new(
            text(self.tool, "tool")?,
            text(self.args, "args")?,
            text(self.route, "route")?,
        )
        .map_err(|error| {
            invalid(match error {
                OperationError::EmptyTool => "tool",
                _ => "route",
            })
        })?;
        let outcome_text = text(self.outcome_text, "outcome_text")?;
        let outcome = match text(self.outcome_kind, "outcome_kind")?.as_str() {
            "none" if outcome_text.is_empty() => None,
            "success" => Some(ToolOutcome::Success(outcome_text)),
            "failure" => Some(ToolOutcome::Failure(outcome_text)),
            _ => return Err(invalid("outcome_kind")),
        };
        let state = match (text(self.state, "state")?.as_str(), outcome) {
            ("pending", None) => OperationState::Pending,
            ("uncertain", None) => OperationState::Uncertain,
            ("resolved", Some(outcome)) => OperationState::Resolved(outcome),
            ("conflict", previous) => OperationState::Conflict { previous },
            _ => return Err(invalid("state")),
        };
        Ok(ToolOperation {
            id,
            key: OperationKey::new(execution, birth, request),
            descriptor,
            state,
        })
    }

    fn write_state(&self, graph: &mut dyn Graph, operation: &ToolOperation) {
        let state = match &operation.state {
            OperationState::Pending => "pending",
            OperationState::Uncertain => "uncertain",
            OperationState::Resolved(_) => "resolved",
            OperationState::Conflict { .. } => "conflict",
        };
        let (kind, text) = match known_outcome(&operation.state) {
            None => ("none", ""),
            Some(ToolOutcome::Success(text)) => ("success", text.as_str()),
            Some(ToolOutcome::Failure(text)) => ("failure", text.as_str()),
        };
        graph.set(operation.id, self.state, Value::Text(state.into()));
        graph.set(operation.id, self.outcome_kind, Value::Text(kind.into()));
        graph.set(operation.id, self.outcome_text, Value::Text(text.into()));
    }
}

fn known_outcome(state: &OperationState) -> Option<&ToolOutcome> {
    match state {
        OperationState::Resolved(outcome) => Some(outcome),
        OperationState::Conflict { previous } => previous.as_ref(),
        OperationState::Pending | OperationState::Uncertain => None,
    }
}
