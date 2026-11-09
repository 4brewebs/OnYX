//! Grafo EAV en memoria del soul de agentes.
//! Sin esquema de negocio ni inferencia. Colecciones ordenadas.
use onyx_core::{Attr, AttrRegistry, ConceptId, Graph, GraphView, OnyxError, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::vocab::{AgentState, Vocab};

mod snapshot;

#[derive(Clone, Default)]
struct Registry {
    names: Vec<String>,
    ids: BTreeMap<String, Attr>,
}

impl Registry {
    fn intern(&mut self, name: &str) -> Result<Attr, OnyxError> {
        if let Some(id) = self.ids.get(name) {
            return Ok(*id);
        }
        let raw = u32::try_from(self.names.len())
            .map_err(|_| OnyxError::Format("attribute IDs exhausted".into()))?;
        let id = Attr::from_raw(raw);
        self.names.push(name.into());
        self.ids.insert(name.into(), id);
        Ok(id)
    }
    fn check(&self, attr: Attr) {
        assert!(
            (attr.raw() as usize) < self.names.len(),
            "attribute not registered in this graph"
        );
    }
}

#[derive(Default)]
struct Node {
    props: BTreeMap<u32, Value>,
    links: BTreeMap<u32, BTreeSet<u64>>,
    /// Owners that hold us as `Value::Concept`: `(source_id, attr_raw)`.
    incoming_props: BTreeSet<(u64, u32)>,
    /// Owners that point at us with a relation: `(source_id, rel_raw)`.
    incoming_links: BTreeSet<(u64, u32)>,
}

/// Rebuilds the incoming-edge index from `props` and `links`.
/// Not serialized: the codec writes only those two maps.
fn rebuild_incoming(nodes: &mut BTreeMap<u64, Node>) {
    for node in nodes.values_mut() {
        node.incoming_props.clear();
        node.incoming_links.clear();
    }
    let mut props = Vec::new();
    let mut links = Vec::new();
    for (&id, node) in nodes.iter() {
        for (&attr, value) in &node.props {
            if let Value::Concept(to) = value {
                props.push((id, attr, to.0));
            }
        }
        for (&rel, targets) in &node.links {
            for &to in targets {
                links.push((id, rel, to));
            }
        }
    }
    for (src, attr, to) in props {
        if let Some(node) = nodes.get_mut(&to) {
            node.incoming_props.insert((src, attr));
        }
    }
    for (src, rel, to) in links {
        if let Some(node) = nodes.get_mut(&to) {
            node.incoming_links.insert((src, rel));
        }
    }
}

/// Implementación EAV de `Graph` para el soul de agentes.
/// Usa únicamente la biblioteca estándar. `new` crea un grafo vacío.
/// El snapshot es privado de este soul.
///
/// # Panics
/// `create` al agotar IDs; `attr` al agotar identificadores de atributo.
/// `set` y `relate` requieren atributos obtenidos del registro de esta instancia.
#[derive(Default)]
pub struct AgentGraph {
    registry: Registry,
    nodes: BTreeMap<u64, Node>,
    high_water: u64,
}

impl AgentGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Helper de dominio. No forma parte del contrato `Graph`.
    pub fn spawn_agent(&mut self, vocab: &Vocab, name: &str, goal: &str) -> ConceptId {
        let id = self.create();
        self.set(id, vocab.name, Value::Text(name.into()));
        self.set(id, vocab.goal, Value::Text(goal.into()));
        self.set(
            id,
            vocab.state,
            Value::Text(AgentState::Idle.as_str().into()),
        );
        self.set(id, vocab.steps, Value::Int(0));
        id
    }

    /// Cuenta conceptos que tienen estado de agente.
    pub fn agent_count(&self, vocab: &Vocab) -> usize {
        self.query(&[])
            .into_iter()
            .filter(|id| self.get(*id, vocab.state).is_some())
            .count()
    }
}

impl AttrRegistry for AgentGraph {
    fn attr(&mut self, name: &str) -> Attr {
        self.registry.intern(name).expect("attribute IDs exhausted")
    }
}

impl GraphView for AgentGraph {
    fn exists(&self, id: ConceptId) -> bool {
        self.nodes.contains_key(&id.0)
    }
    fn get(&self, id: ConceptId, attr: Attr) -> Option<Value> {
        self.nodes.get(&id.0)?.props.get(&attr.raw()).cloned()
    }
    fn related(&self, from: ConceptId, rel: Attr) -> Vec<ConceptId> {
        self.nodes
            .get(&from.0)
            .and_then(|n| n.links.get(&rel.raw()))
            .map(|targets| targets.iter().copied().map(ConceptId).collect())
            .unwrap_or_default()
    }
    fn query(&self, with: &[Attr]) -> Vec<ConceptId> {
        self.nodes
            .iter()
            .filter(|(_, node)| with.iter().all(|a| node.props.contains_key(&a.raw())))
            .map(|(&id, _)| ConceptId(id))
            .collect()
    }
    fn props(&self, id: ConceptId) -> Vec<(String, Value)> {
        let Some(node) = self.nodes.get(&id.0) else {
            return Vec::new();
        };
        let mut props: Vec<_> = node
            .props
            .iter()
            .map(|(&a, v)| (self.registry.names[a as usize].clone(), v.clone()))
            .collect();
        props.sort_by(|a, b| a.0.cmp(&b.0));
        props
    }
    fn links(&self, id: ConceptId) -> Vec<(String, ConceptId)> {
        let Some(node) = self.nodes.get(&id.0) else {
            return Vec::new();
        };
        let mut links: Vec<_> = node
            .links
            .iter()
            .flat_map(|(&a, targets)| {
                targets
                    .iter()
                    .map(move |&to| (self.registry.names[a as usize].clone(), ConceptId(to)))
            })
            .collect();
        links.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.0.cmp(&b.1.0)));
        links
    }
}

impl Graph for AgentGraph {
    fn create(&mut self) -> ConceptId {
        let id = self
            .high_water
            .checked_add(1)
            .expect("concept IDs exhausted");
        self.nodes.insert(id, Node::default());
        self.high_water = id;
        ConceptId(id)
    }
    fn destroy(&mut self, id: ConceptId) {
        let Some(node) = self.nodes.remove(&id.0) else {
            return;
        };
        for (src, attr) in node.incoming_props {
            if let Some(src_node) = self.nodes.get_mut(&src) {
                src_node.props.remove(&attr);
            }
        }
        for (src, rel) in node.incoming_links {
            if let Some(src_node) = self.nodes.get_mut(&src) {
                if let Some(targets) = src_node.links.get_mut(&rel) {
                    targets.remove(&id.0);
                    if targets.is_empty() {
                        src_node.links.remove(&rel);
                    }
                }
            }
        }
        for (&attr, value) in &node.props {
            if let Value::Concept(to) = value {
                if let Some(to_node) = self.nodes.get_mut(&to.0) {
                    to_node.incoming_props.remove(&(id.0, attr));
                }
            }
        }
        for (&rel, targets) in &node.links {
            for &to in targets {
                if let Some(to_node) = self.nodes.get_mut(&to) {
                    to_node.incoming_links.remove(&(id.0, rel));
                }
            }
        }
    }
    fn set(&mut self, id: ConceptId, attr: Attr, value: Value) {
        if !self.exists(id) || matches!(&value, Value::Concept(to) if !self.exists(*to)) {
            return;
        }
        self.registry.check(attr);
        let raw = attr.raw();
        let new_concept = match &value {
            Value::Concept(c) => Some(*c),
            _ => None,
        };
        let old_concept = match self
            .nodes
            .get_mut(&id.0)
            .expect("live owner")
            .props
            .insert(raw, value)
        {
            Some(Value::Concept(c)) => Some(c),
            _ => None,
        };
        if old_concept == new_concept {
            return;
        }
        if let Some(old) = old_concept {
            if let Some(node) = self.nodes.get_mut(&old.0) {
                node.incoming_props.remove(&(id.0, raw));
            }
        }
        if let Some(new) = new_concept {
            if let Some(node) = self.nodes.get_mut(&new.0) {
                node.incoming_props.insert((id.0, raw));
            }
        }
    }
    fn relate(&mut self, from: ConceptId, rel: Attr, to: ConceptId) {
        if !self.exists(from) || !self.exists(to) {
            return;
        }
        self.registry.check(rel);
        let inserted = self
            .nodes
            .get_mut(&from.0)
            .expect("live source")
            .links
            .entry(rel.raw())
            .or_default()
            .insert(to.0);
        if inserted {
            if let Some(node) = self.nodes.get_mut(&to.0) {
                node.incoming_links.insert((from.0, rel.raw()));
            }
        }
    }
    fn unrelate(&mut self, from: ConceptId, rel: Attr, to: ConceptId) {
        let Some(node) = self.nodes.get_mut(&from.0) else {
            return;
        };
        let removed = if let Some(targets) = node.links.get_mut(&rel.raw()) {
            let removed = targets.remove(&to.0);
            if targets.is_empty() {
                node.links.remove(&rel.raw());
            }
            removed
        } else {
            false
        };
        if removed {
            if let Some(node) = self.nodes.get_mut(&to.0) {
                node.incoming_links.remove(&(from.0, rel.raw()));
            }
        }
    }
    fn export(&self) -> Vec<u8> {
        snapshot::encode(self)
    }
    fn import(&mut self, bytes: &[u8]) -> Result<(), OnyxError> {
        let source = snapshot::decode(bytes)?;
        let mut registry = self.registry.clone();
        let remap: Vec<_> = source
            .registry
            .names
            .iter()
            .map(|name| registry.intern(name))
            .collect::<Result<_, _>>()?;
        let mut nodes = BTreeMap::new();
        for (id, node) in source.nodes {
            nodes.insert(
                id,
                Node {
                    props: node
                        .props
                        .into_iter()
                        .map(|(a, v)| (remap[a as usize].raw(), v))
                        .collect(),
                    links: node
                        .links
                        .into_iter()
                        .map(|(a, to)| (remap[a as usize].raw(), to))
                        .collect(),
                    ..Node::default()
                },
            );
        }
        rebuild_incoming(&mut nodes);
        *self = Self {
            registry,
            nodes,
            high_water: self.high_water.max(source.high_water),
        };
        Ok(())
    }
}
