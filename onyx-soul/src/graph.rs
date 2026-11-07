//! Grafo EAV en memoria del soul por defecto. Sin esquema de negocio ni inferencia.
//! Colecciones ordenadas; no ofrece límites de latencia ni de asignación.
use onyx_core::{Attr, AttrRegistry, ConceptId, Graph, GraphView, OnyxError, Value};
use std::collections::{BTreeMap, BTreeSet};

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
}

/// Implementación EAV de `Graph`, con IDs crecientes y limpieza de referencias.
/// Usa únicamente la biblioteca estándar. `new` crea un grafo vacío.
/// El snapshot es privado de este soul, no un formato portable.
///
/// # Panics
/// `create` al agotar IDs; `attr` al agotar identificadores de atributo.
/// `set` y `relate` requieren atributos obtenidos del registro de esta instancia;
/// un número fuera del registro provoca un pánico, no crea un nombre implícito.
#[derive(Default)]
pub struct WorldGraph {
    registry: Registry,
    nodes: BTreeMap<u64, Node>,
    high_water: u64,
}

impl WorldGraph {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AttrRegistry for WorldGraph {
    fn attr(&mut self, name: &str) -> Attr {
        self.registry.intern(name).expect("attribute IDs exhausted")
    }
}

impl GraphView for WorldGraph {
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

impl Graph for WorldGraph {
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
        if self.nodes.remove(&id.0).is_none() {
            return;
        }
        for node in self.nodes.values_mut() {
            node.props
                .retain(|_, value| !matches!(value,Value::Concept(to) if *to==id));
            node.links.retain(|_, targets| {
                targets.remove(&id.0);
                !targets.is_empty()
            });
        }
    }
    fn set(&mut self, id: ConceptId, attr: Attr, value: Value) {
        if !self.exists(id) || matches!(&value,Value::Concept(to) if !self.exists(*to)) {
            return;
        }
        self.registry.check(attr);
        self.nodes
            .get_mut(&id.0)
            .expect("live owner")
            .props
            .insert(attr.raw(), value);
    }
    fn relate(&mut self, from: ConceptId, rel: Attr, to: ConceptId) {
        if !self.exists(from) || !self.exists(to) {
            return;
        }
        self.registry.check(rel);
        self.nodes
            .get_mut(&from.0)
            .expect("live source")
            .links
            .entry(rel.raw())
            .or_default()
            .insert(to.0);
    }
    fn unrelate(&mut self, from: ConceptId, rel: Attr, to: ConceptId) {
        let Some(node) = self.nodes.get_mut(&from.0) else {
            return;
        };
        if let Some(targets) = node.links.get_mut(&rel.raw()) {
            targets.remove(&to.0);
            if targets.is_empty() {
                node.links.remove(&rel.raw());
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
                },
            );
        }
        *self = Self {
            registry,
            nodes,
            high_water: self.high_water.max(source.high_water),
        };
        Ok(())
    }
}
