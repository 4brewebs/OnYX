use std::cmp::Reverse;

use crate::ontology::{RelationKind, ScentArena};

pub const MAX_TEMPERATURE_BP: u16 = 10_000;

/// Deterministic integer-only heat diffusion configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistillerConfig {
    /// Maximum hop distance from any epicenter node.
    pub iterations: u8,
    /// Heat retention factor per edge traversal in basis points (`0..=10_000`).
    pub decay_bp: u16,
    /// Minimum temperature in basis points (`0..=10_000`) to include in distilled results.
    pub threshold_bp: u16,
}

impl Default for DistillerConfig {
    fn default() -> Self {
        Self {
            iterations: 3,
            decay_bp: 7_500,
            threshold_bp: 2_500,
        }
    }
}

/// A ranked symbol reached during thermal impact diffusion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactHit {
    pub node_id: usize,
    /// Temperature in basis points (`0..=10_000`).
    pub temperature_bp: u16,
    /// Discrete shortest hop count from any epicenter (`0` = epicenter itself).
    pub min_hop: u16,
    /// Dominant relation leading from an epicenter to this symbol (`None` for epicenters).
    pub dominant_relation: Option<RelationKind>,
}

/// Runs deterministic Max-Heat diffusion using pure integer basis-point arithmetic.
pub fn distill_heat(
    arena: &ScentArena,
    epicenter_ids: &[usize],
    config: &DistillerConfig,
) -> Vec<ImpactHit> {
    let n = arena.nodes.len();
    if n == 0 || epicenter_ids.is_empty() {
        return Vec::new();
    }

    let mut temps = vec![0_u16; n];
    let mut hops: Vec<Option<u16>> = vec![None; n];
    let mut rels: Vec<Option<RelationKind>> = vec![None; n];

    for &id in epicenter_ids {
        if id < n {
            temps[id] = MAX_TEMPERATURE_BP;
            hops[id] = Some(0);
        }
    }

    for step in 1..=u16::from(config.iterations) {
        let mut next_temps = temps.clone();
        let mut next_hops = hops.clone();
        let mut next_rels = rels.clone();

        for edge in &arena.edges {
            let src_temp = temps[edge.source_id];
            if src_temp == 0 {
                continue;
            }
            let propagated =
                ((u64::from(src_temp) * u64::from(edge.weight_bp) * u64::from(config.decay_bp))
                    / 100_000_000)
                    .min(u64::from(MAX_TEMPERATURE_BP)) as u16;

            if propagated > next_temps[edge.target_id] {
                next_temps[edge.target_id] = propagated;
                let hop = hops[edge.source_id].map_or(step, |h| h.saturating_add(1));
                next_hops[edge.target_id] = Some(hop);
                next_rels[edge.target_id] = Some(edge.kind);
            }
        }

        temps = next_temps;
        hops = next_hops;
        rels = next_rels;
    }

    let mut hits = Vec::new();
    for id in 0..n {
        let temp = temps[id];
        let Some(hop) = hops[id] else {
            continue;
        };
        if hop == 0 || temp >= config.threshold_bp {
            hits.push(ImpactHit {
                node_id: id,
                temperature_bp: temp,
                min_hop: hop,
                dominant_relation: rels[id],
            });
        }
    }

    hits.sort_by_key(|hit| {
        (
            hit.min_hop > 0,
            Reverse(hit.temperature_bp),
            hit.min_hop,
            arena.nodes[hit.node_id].qualified_name.as_str(),
        )
    });
    hits
}
