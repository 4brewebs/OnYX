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

#[cfg(test)]
mod incoming_index_tests {
    //! The incoming index is not observable through the public API, so stale
    //! entries (left by `unrelate` or by `destroy` on its targets) only show up
    //! here: after every operation each node's index must equal a fresh
    //! `rebuild_incoming` over the same `props` and `links`.
    use super::*;

    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    type Index = BTreeMap<u64, (BTreeSet<(u64, u32)>, BTreeSet<(u64, u32)>)>;

    fn actual(g: &WorldGraph) -> Index {
        g.nodes
            .iter()
            .map(|(&id, n)| (id, (n.incoming_props.clone(), n.incoming_links.clone())))
            .collect()
    }

    fn expected(g: &WorldGraph) -> Index {
        let mut copy: BTreeMap<u64, Node> = g
            .nodes
            .iter()
            .map(|(&id, n)| {
                let node = Node {
                    props: n.props.clone(),
                    links: n.links.clone(),
                    ..Node::default()
                };
                (id, node)
            })
            .collect();
        rebuild_incoming(&mut copy);
        copy.into_iter()
            .map(|(id, n)| (id, (n.incoming_props, n.incoming_links)))
            .collect()
    }

    #[test]
    fn incoming_index_never_holds_stale_entries() {
        for seed in 0..200u64 {
            let mut rng = SplitMix64(seed);
            let mut g = WorldGraph::new();
            let attrs = [g.attr("p"), g.attr("q"), g.attr("r")];
            let mut ids: Vec<ConceptId> = Vec::new();
            for step in 0..300 {
                if ids.is_empty() || rng.below(6) == 0 {
                    ids.push(g.create());
                    continue;
                }
                let a = ids[rng.below(ids.len())];
                let b = ids[rng.below(ids.len())];
                let attr = attrs[rng.below(attrs.len())];
                match rng.below(7) {
                    0 => g.destroy(a),
                    1 | 2 => g.set(a, attr, Value::Concept(b)),
                    3 => g.set(a, attr, Value::Int(step)),
                    4 => g.relate(a, attr, b),
                    5 => g.unrelate(a, attr, b),
                    _ => {
                        let bytes = g.export();
                        let mut fresh = WorldGraph::new();
                        fresh.attr("r");
                        fresh.attr("q");
                        fresh.attr("p");
                        fresh.import(&bytes).expect("own snapshot");
                        g = fresh;
                    }
                }
                assert_eq!(actual(&g), expected(&g), "seed {seed}, step {step}");
            }
        }
    }
}
