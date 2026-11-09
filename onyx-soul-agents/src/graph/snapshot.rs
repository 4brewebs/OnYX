//! Codec privado de AgentGraph. No se expone ni impone al core.
use super::{AgentGraph, Node};
use onyx_core::{ConceptId, OnyxError, Value};

const MAGIC: &[u8; 8] = b"ONYXAG\x01\0";
fn invalid() -> OnyxError {
    OnyxError::Format("invalid AgentGraph snapshot".into())
}
fn word(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn text(out: &mut Vec<u8>, s: &str) {
    word(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

pub(super) fn encode(g: &AgentGraph) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    word(&mut out, g.high_water);
    word(&mut out, g.registry.names.len() as u64);
    for name in &g.registry.names {
        text(&mut out, name);
    }
    word(&mut out, g.nodes.len() as u64);
    for (&id, node) in &g.nodes {
        word(&mut out, id);
        word(&mut out, node.props.len() as u64);
        for (&attr, value) in &node.props {
            out.extend_from_slice(&attr.to_le_bytes());
            match value {
                Value::Bool(v) => {
                    out.push(0);
                    out.push(u8::from(*v));
                }
                Value::Int(v) => {
                    out.push(1);
                    out.extend_from_slice(&v.to_le_bytes());
                }
                Value::Float(v) => {
                    out.push(2);
                    word(&mut out, v.to_bits());
                }
                Value::Text(v) => {
                    out.push(3);
                    text(&mut out, v);
                }
                Value::Concept(v) => {
                    out.push(4);
                    word(&mut out, v.0);
                }
            }
        }
        word(
            &mut out,
            node.links
                .values()
                .map(|targets| targets.len() as u64)
                .sum(),
        );
        for (&attr, targets) in &node.links {
            for &to in targets {
                out.extend_from_slice(&attr.to_le_bytes());
                word(&mut out, to);
            }
        }
    }
    out
}

struct Reader<'a> {
    bytes: &'a [u8],
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], OnyxError> {
        let data = self.bytes.get(..n).ok_or_else(invalid)?;
        self.bytes = &self.bytes[n..];
        Ok(data)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], OnyxError> {
        self.take(N)?.try_into().map_err(|_| invalid())
    }
    fn byte(&mut self) -> Result<u8, OnyxError> {
        Ok(self.array::<1>()?[0])
    }
    fn word(&mut self) -> Result<u64, OnyxError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn count(&mut self, min_bytes: usize) -> Result<usize, OnyxError> {
        let n = usize::try_from(self.word()?).map_err(|_| invalid())?;
        if n > self.bytes.len() / min_bytes {
            return Err(invalid());
        }
        Ok(n)
    }
    fn text(&mut self) -> Result<String, OnyxError> {
        let n = usize::try_from(self.word()?).map_err(|_| invalid())?;
        let bytes = self.take(n)?;
        Ok(std::str::from_utf8(bytes).map_err(|_| invalid())?.into())
    }
    fn attr(&mut self, names: usize) -> Result<u32, OnyxError> {
        let a = u32::from_le_bytes(self.array()?);
        if a as usize >= names {
            return Err(invalid());
        }
        Ok(a)
    }
    fn value(&mut self) -> Result<Value, OnyxError> {
        match self.byte()? {
            0 => match self.byte()? {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                _ => Err(invalid()),
            },
            1 => Ok(Value::Int(i64::from_le_bytes(self.array()?))),
            2 => Ok(Value::Float(f64::from_bits(self.word()?))),
            3 => Ok(Value::Text(self.text()?)),
            4 => Ok(Value::Concept(ConceptId(self.word()?))),
            _ => Err(invalid()),
        }
    }
}

pub(super) fn decode(bytes: &[u8]) -> Result<AgentGraph, OnyxError> {
    let mut input = Reader { bytes };
    if input.take(MAGIC.len())? != MAGIC {
        return Err(invalid());
    }
    let mut g = AgentGraph {
        high_water: input.word()?,
        ..AgentGraph::new()
    };
    let names = input.count(8)?;
    for _ in 0..names {
        let name = input.text()?;
        if g.registry.ids.contains_key(&name) {
            return Err(invalid());
        }
        g.registry.intern(&name)?;
    }
    let concepts = input.count(24)?;
    for _ in 0..concepts {
        let id = input.word()?;
        if id == 0 || id > g.high_water || g.nodes.contains_key(&id) {
            return Err(invalid());
        }
        let mut node = Node::default();
        let props = input.count(6)?;
        for _ in 0..props {
            let attr = input.attr(names)?;
            let value = input.value()?;
            if node.props.insert(attr, value).is_some() {
                return Err(invalid());
            }
        }
        let links = input.count(12)?;
        for _ in 0..links {
            let attr = input.attr(names)?;
            let to = input.word()?;
            if !node.links.entry(attr).or_default().insert(to) {
                return Err(invalid());
            }
        }
        g.nodes.insert(id, node);
    }
    if !input.bytes.is_empty() {
        return Err(invalid());
    }
    for node in g.nodes.values() {
        for value in node.props.values() {
            if matches!(value,Value::Concept(to) if !g.nodes.contains_key(&to.0)) {
                return Err(invalid());
            }
        }
        if node
            .links
            .values()
            .flatten()
            .any(|to| !g.nodes.contains_key(to))
        {
            return Err(invalid());
        }
    }
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onyx_core::{AttrRegistry, Graph, GraphView};

    fn fixture() -> AgentGraph {
        let mut g = AgentGraph::new();
        let value = g.attr("value");
        let rel = g.attr("rel");
        g.attr("unused");
        let x = g.create();
        let y = g.create();
        let dead = g.create();
        g.destroy(dead);
        g.set(
            x,
            value,
            Value::Float(f64::from_bits(0xfff8_0000_0000_0042)),
        );
        g.set(y, value, Value::Concept(x));
        g.relate(x, rel, y);
        g
    }
    fn check_rejection(bytes: &[u8]) {
        let mut g = fixture();
        g.attr("receiver-only");
        let before = g.export();
        let high = g.high_water;
        let names = g.registry.names.clone();
        let ids = g.registry.ids.clone();
        assert!(g.import(bytes).is_err());
        assert_eq!(g.export(), before);
        assert_eq!(g.high_water, high);
        assert_eq!(g.registry.names, names);
        assert_eq!(g.registry.ids, ids);
    }
    fn node(id: u64, props: &[(u32, Vec<u8>)], links: &[(u32, u64)]) -> Vec<u8> {
        let mut out = Vec::new();
        word(&mut out, id);
        word(&mut out, props.len() as u64);
        for (attr, value) in props {
            out.extend_from_slice(&attr.to_le_bytes());
            out.extend_from_slice(value);
        }
        word(&mut out, links.len() as u64);
        for (attr, to) in links {
            out.extend_from_slice(&attr.to_le_bytes());
            word(&mut out, *to);
        }
        out
    }
    fn wire(high: u64, names: &[&str], nodes: &[Vec<u8>]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        word(&mut out, high);
        word(&mut out, names.len() as u64);
        for name in names {
            text(&mut out, name);
        }
        word(&mut out, nodes.len() as u64);
        for node in nodes {
            out.extend_from_slice(node);
        }
        out
    }

    #[test]
    fn every_truncated_prefix_is_rejected_atomically() {
        let bytes = fixture().export();
        for end in 0..bytes.len() {
            check_rejection(&bytes[..end]);
        }
        let mut restored = AgentGraph::new();
        restored.import(&bytes).unwrap();
        assert_eq!(restored.export(), bytes);
    }

    #[test]
    fn late_failure_leaks_neither_new_names_nor_incoming_high_water() {
        let mut incoming = AgentGraph::new();
        let new_name = incoming.attr("nuevo");
        for _ in 0..10 {
            incoming.create();
        }
        let id = incoming.create();
        incoming.set(id, new_name, Value::Text("final".into()));
        let mut bytes = incoming.export();
        bytes.pop();
        check_rejection(&bytes);
        let mut receiver = fixture();
        let mut control = fixture();
        assert!(receiver.import(&bytes).is_err());
        assert_eq!(receiver.attr("nuevo"), control.attr("nuevo"));
        assert_eq!(receiver.create(), control.create());
    }

    #[test]
    fn incompatible_header_and_trailing_bytes_are_rejected() {
        let bytes = fixture().export();
        for i in 0..MAGIC.len() {
            let mut bad = bytes.clone();
            bad[i] ^= 0xff;
            check_rejection(&bad);
        }
        let mut bad = bytes;
        bad.push(0);
        check_rejection(&bad);
    }

    #[test]
    fn untrusted_counts_and_string_lengths_are_bounded_by_input() {
        let mut bad = MAGIC.to_vec();
        word(&mut bad, 0);
        word(&mut bad, u64::MAX);
        check_rejection(&bad);
        let mut bad = MAGIC.to_vec();
        word(&mut bad, 0);
        word(&mut bad, 1);
        word(&mut bad, u64::MAX);
        check_rejection(&bad);
        let mut bad = MAGIC.to_vec();
        word(&mut bad, 0);
        word(&mut bad, 0);
        word(&mut bad, u64::MAX);
        check_rejection(&bad);
        let mut huge = vec![3];
        word(&mut huge, u64::MAX);
        check_rejection(&wire(1, &["v"], &[node(1, &[(0, huge)], &[])]));
        let mut badnode = Vec::new();
        word(&mut badnode, 1);
        word(&mut badnode, u64::MAX);
        word(&mut badnode, 0);
        check_rejection(&wire(1, &["v"], &[badnode]));
        let mut badnode = Vec::new();
        word(&mut badnode, 1);
        word(&mut badnode, 0);
        word(&mut badnode, u64::MAX);
        check_rejection(&wire(1, &["v"], &[badnode]));
    }

    #[test]
    fn duplicate_names_concepts_properties_and_edges_are_rejected() {
        check_rejection(&wire(0, &["v", "v"], &[]));
        check_rejection(&wire(1, &[], &[node(1, &[], &[]), node(1, &[], &[])]));
        check_rejection(&wire(
            1,
            &["v"],
            &[node(1, &[(0, vec![0, 0]), (0, vec![0, 1])], &[])],
        ));
        check_rejection(&wire(1, &["v"], &[node(1, &[], &[(0, 1), (0, 1)])]));
    }

    #[test]
    fn dangling_references_unknown_attrs_and_impossible_ids_are_rejected() {
        let mut reference = vec![4];
        word(&mut reference, 2);
        check_rejection(&wire(1, &["v"], &[node(1, &[(0, reference)], &[])]));
        check_rejection(&wire(1, &["v"], &[node(1, &[], &[(0, 2)])]));
        check_rejection(&wire(1, &["v"], &[node(1, &[(1, vec![0, 0])], &[])]));
        check_rejection(&wire(1, &["v"], &[node(1, &[], &[(1, 1)])]));
        check_rejection(&wire(0, &[], &[node(1, &[], &[])]));
        check_rejection(&wire(1, &[], &[node(0, &[], &[])]));
    }

    #[test]
    fn invalid_tags_bool_and_utf8_are_rejected() {
        for value in [vec![255], vec![0, 2], vec![3, 1, 0, 0, 0, 0, 0, 0, 0, 255]] {
            check_rejection(&wire(1, &["v"], &[node(1, &[(0, value)], &[])]));
        }
        let mut bad = MAGIC.to_vec();
        word(&mut bad, 0);
        word(&mut bad, 1);
        word(&mut bad, 1);
        bad.push(255);
        word(&mut bad, 0);
        check_rejection(&bad);
    }

    #[test]
    fn exhausting_concept_ids_panics_without_changing_graph() {
        let mut g = fixture();
        g.import(&wire(u64::MAX, &[], &[])).unwrap();
        let before = g.export();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.create())).is_err());
        assert_eq!(g.export(), before);
        assert!(g.query(&[]).is_empty());
        assert_eq!(g.high_water, u64::MAX);
    }

    #[test]
    fn receiver_registry_and_deleted_high_water_survive_repeated_imports() {
        let mut g = AgentGraph::new();
        let unused = g.attr("unused");
        let own = g.attr("own");
        let bytes = fixture().export();
        g.import(&bytes).unwrap();
        let first = g.registry.names.clone();
        let high = g.create();
        g.destroy(high);
        g.import(&bytes).unwrap();
        assert_eq!(g.registry.names, first);
        assert_eq!(g.attr("unused"), unused);
        assert_eq!(g.attr("own"), own);
        assert_eq!(g.high_water, high.0);
        assert!(g.create().0 > high.0);
    }

    #[test]
    fn byte_mutations_never_panic_and_rejected_mutations_leave_everything_intact() {
        let bytes = fixture().export();
        for i in 0..bytes.len() {
            let mut mutated = bytes.clone();
            mutated[i] ^= 0xff;
            let mut g = fixture();
            g.attr("receiver-only");
            let before = g.export();
            let high = g.high_water;
            let names = g.registry.names.clone();
            if g.import(&mutated).is_err() {
                assert_eq!(g.export(), before);
                assert_eq!(g.high_water, high);
                assert_eq!(g.registry.names, names);
            } else {
                for id in g.query(&[]) {
                    for (_, to) in g.links(id) {
                        assert!(g.exists(to));
                    }
                    for (_, value) in g.props(id) {
                        if let Value::Concept(to) = value {
                            assert!(g.exists(to));
                        }
                    }
                }
            }
        }
    }
}
