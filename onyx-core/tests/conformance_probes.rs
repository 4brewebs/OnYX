#![cfg(feature = "conformance")]

use onyx_core::conformance::graph;
use onyx_core::{Attr, AttrRegistry, ConceptId, Graph, GraphView, OnyxError, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Defect {
    ReuseIds,
    ReverseQuery,
    SetCreatesOwner,
    PartialImport,
    AdvanceHigh,
    LeakName,
    KeepConceptReferences,
    KeepIncomingEdges,
}

// Doble limitado a los cuatro casos ejercitados: solo admite importaciones fallidas.
// No es una segunda implementación de snapshots ni una prueba de intercambio de souls.
struct MutationProbe {
    defect: Option<Defect>,
    names: Vec<String>,
    nodes: BTreeMap<u64, BTreeMap<u32, Value>>,
    edges: BTreeSet<(u64, u32, u64)>,
    high: u64,
}

impl MutationProbe {
    fn new(defect: Option<Defect>) -> Self {
        Self {
            defect,
            names: Vec::new(),
            nodes: BTreeMap::new(),
            edges: BTreeSet::new(),
            high: 35,
        }
    }
}

impl AttrRegistry for MutationProbe {
    fn attr(&mut self, name: &str) -> Attr {
        if let Some(index) = self.names.iter().position(|n| n == name) {
            return Attr::from_raw(u32::try_from(index).unwrap());
        }
        let id = Attr::from_raw(u32::try_from(self.names.len()).unwrap());
        self.names.push(name.into());
        id
    }
}

impl GraphView for MutationProbe {
    fn exists(&self, id: ConceptId) -> bool {
        self.nodes.contains_key(&id.0)
    }
    fn get(&self, id: ConceptId, attr: Attr) -> Option<Value> {
        self.nodes.get(&id.0)?.get(&attr.raw()).cloned()
    }
    fn related(&self, from: ConceptId, rel: Attr) -> Vec<ConceptId> {
        self.edges
            .iter()
            .filter(|(f, r, _)| *f == from.0 && *r == rel.raw())
            .map(|(_, _, to)| ConceptId(*to))
            .collect()
    }
    fn query(&self, with: &[Attr]) -> Vec<ConceptId> {
        let mut result: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, props)| with.iter().all(|a| props.contains_key(&a.raw())))
            .map(|(&id, _)| ConceptId(id))
            .collect();
        if self.defect == Some(Defect::ReverseQuery) {
            result.reverse();
        }
        result
    }
    fn props(&self, id: ConceptId) -> Vec<(String, Value)> {
        let mut result: Vec<_> = self
            .nodes
            .get(&id.0)
            .into_iter()
            .flat_map(|props| props.iter())
            .map(|(&a, v)| (self.names[a as usize].clone(), v.clone()))
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }
    fn links(&self, id: ConceptId) -> Vec<(String, ConceptId)> {
        let mut result: Vec<_> = self
            .edges
            .iter()
            .filter(|(from, _, _)| *from == id.0)
            .map(|(_, a, to)| (self.names[*a as usize].clone(), ConceptId(*to)))
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.0.cmp(&b.1.0)));
        result
    }
}

impl Graph for MutationProbe {
    fn create(&mut self) -> ConceptId {
        if self.defect == Some(Defect::ReuseIds) {
            self.high = self.nodes.keys().next_back().copied().unwrap_or(35);
        }
        self.high += 7;
        self.nodes.insert(self.high, BTreeMap::new());
        ConceptId(self.high)
    }
    fn destroy(&mut self, id: ConceptId) {
        self.nodes.remove(&id.0);
        self.edges.retain(|(from, _, to)| {
            *from != id.0 && (self.defect == Some(Defect::KeepIncomingEdges) || *to != id.0)
        });
        if self.defect != Some(Defect::KeepConceptReferences) {
            for props in self.nodes.values_mut() {
                props.retain(|_, v| !matches!(v,Value::Concept(to) if *to == id));
            }
        }
    }
    fn set(&mut self, id: ConceptId, attr: Attr, value: Value) {
        if matches!(&value,Value::Concept(to) if !self.exists(*to)) {
            return;
        }
        if self.defect == Some(Defect::SetCreatesOwner) {
            self.nodes
                .entry(id.0)
                .or_default()
                .insert(attr.raw(), value);
        } else if let Some(props) = self.nodes.get_mut(&id.0) {
            props.insert(attr.raw(), value);
        }
    }
    fn relate(&mut self, from: ConceptId, rel: Attr, to: ConceptId) {
        if self.exists(from) && self.exists(to) {
            self.edges.insert((from.0, rel.raw(), to.0));
        }
    }
    fn unrelate(&mut self, from: ConceptId, rel: Attr, to: ConceptId) {
        self.edges.remove(&(from.0, rel.raw(), to.0));
    }
    fn export(&self) -> Vec<u8> {
        panic!("MutationProbe only supports cases without successful snapshots")
    }
    fn import(&mut self, _: &[u8]) -> Result<(), OnyxError> {
        match self.defect {
            Some(Defect::PartialImport) => {
                self.nodes.clear();
                self.attr("leaked-name");
            }
            Some(Defect::AdvanceHigh) => {
                self.high += 7;
            }
            Some(Defect::LeakName) => {
                self.attr("leaked-name");
            }
            _ => {}
        }
        Err(OnyxError::Format("unsupported test snapshot".into()))
    }
}

fn assert_detected(defect: Defect, exercise: impl Fn(Option<Defect>)) {
    // El mismo caso debe pasar sin defecto; así un fixture incompleto no da un falso verde.
    exercise(None);
    assert!(
        catch_unwind(AssertUnwindSafe(|| exercise(Some(defect)))).is_err(),
        "suite failed to detect {defect:?}"
    );
}

#[test]
fn suite_detects_reused_ids() {
    assert_detected(Defect::ReuseIds, |d| {
        graph::creation_and_queries(|| MutationProbe::new(d))
    });
}
#[test]
fn suite_detects_unsorted_queries() {
    assert_detected(Defect::ReverseQuery, |d| {
        graph::creation_and_queries(|| MutationProbe::new(d))
    });
}
#[test]
fn suite_detects_set_creating_a_missing_owner() {
    assert_detected(Defect::SetCreatesOwner, |d| {
        graph::set_and_missing_references(|| MutationProbe::new(d))
    });
}
#[test]
fn suite_detects_non_atomic_import() {
    assert_detected(Defect::PartialImport, |d| {
        graph::rejected_import_is_atomic(
            || MutationProbe::new(d),
            b"invalid",
            |g| (g.names.clone(), g.high),
        )
    });
}

#[test]
fn suite_detects_failed_import_advancing_only_the_counter() {
    assert_detected(Defect::AdvanceHigh, |d| {
        graph::rejected_import_is_atomic(
            || MutationProbe::new(d),
            b"invalid",
            |g| (g.names.clone(), g.high),
        )
    });
}

#[test]
fn suite_detects_failed_import_leaking_only_a_name() {
    assert_detected(Defect::LeakName, |d| {
        graph::rejected_import_is_atomic(
            || MutationProbe::new(d),
            b"invalid",
            |g| (g.names.clone(), g.high),
        )
    });
}

#[test]
fn suite_detects_destroy_leaving_concept_properties() {
    assert_detected(Defect::KeepConceptReferences, |d| {
        graph::destruction(|| MutationProbe::new(d))
    });
}

#[test]
fn suite_detects_destroy_leaving_incoming_edges() {
    assert_detected(Defect::KeepIncomingEdges, |d| {
        graph::destruction(|| MutationProbe::new(d))
    });
}
