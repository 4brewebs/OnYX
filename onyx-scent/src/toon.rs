use std::collections::BTreeSet;

use crate::distiller::{DistillerConfig, distill_heat};
use crate::index::WorkspaceIndex;
use crate::ontology::{RelationKind, ScentError};

pub const TOON_HEADER: &str = "@TOON v1";
pub const DEFAULT_MAX_ROWS: usize = 250;
pub const DEFAULT_MAX_BYTES: usize = 24 * 1024;

/// A single decoded TOON row: `(subject, predicate, object, meta)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToonRow {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub meta: String,
}

/// Paging options for `@TOON v1` outputs to fit agent context budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToonPageOptions {
    pub cursor: usize,
    pub max_rows: usize,
    pub max_bytes: usize,
    pub public_only: bool,
    pub exclude_tests: bool,
}

impl Default for ToonPageOptions {
    fn default() -> Self {
        Self {
            cursor: 0,
            max_rows: DEFAULT_MAX_ROWS,
            max_bytes: DEFAULT_MAX_BYTES,
            public_only: false,
            exclude_tests: false,
        }
    }
}

/// Encodes one positional tuple `subject | predicate | object | meta` with deterministic escaping.
pub fn encode_row(row: &ToonRow) -> String {
    format!(
        "{} | {} | {} | {}",
        escape_field(&row.subject),
        escape_field(&row.predicate),
        escape_field(&row.object),
        escape_field(&row.meta),
    )
}

/// Paginates and formats rows into a complete `@TOON v1` document.
pub fn render_toon(rows: &[ToonRow], opts: ToonPageOptions) -> String {
    let mut out = String::from(TOON_HEADER);
    out.push('\n');
    out.push_str("# cols: subject | predicate | object | meta\n");

    let total = rows.len();
    let start = opts.cursor.min(total);
    let max_rows = opts.max_rows.max(1);
    let max_bytes = opts.max_bytes.max(512);

    let mut emitted = 0usize;
    for row in &rows[start..] {
        if emitted >= max_rows {
            break;
        }
        let line = encode_row(row);
        if emitted > 0 && out.len() + line.len() + 64 > max_bytes {
            break;
        }
        out.push_str(&line);
        out.push('\n');
        emitted += 1;
    }

    let next_cursor = start + emitted;
    if next_cursor < total {
        out.push_str(&format!(
            "# page: next_cursor={next_cursor} total={total}\n"
        ));
    } else {
        out.push_str(&format!("# total: {total}\n"));
    }
    out
}

/// Parses a `@TOON v1` document back into rows (for verification and round-trip tests).
pub fn decode_toon(input: &str) -> Result<Vec<ToonRow>, ScentError> {
    let mut lines = input.lines().enumerate();
    let Some((_, first)) = lines.next() else {
        return Err(ScentError::InvalidToon {
            line: 1,
            message: "empty TOON input".to_string(),
        });
    };
    if first.trim() != TOON_HEADER {
        return Err(ScentError::InvalidToon {
            line: 1,
            message: format!("expected `{TOON_HEADER}`, found `{first}`"),
        });
    }

    let mut rows = Vec::new();
    for (idx, line) in lines {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fields = split_escaped_fields(line);
        if fields.len() != 4 {
            return Err(ScentError::InvalidToon {
                line: line_no,
                message: format!("expected 4 pipe-delimited fields, found {}", fields.len()),
            });
        }
        rows.push(ToonRow {
            subject: fields[0].trim().to_string(),
            predicate: fields[1].trim().to_string(),
            object: fields[2].trim().to_string(),
            meta: fields[3].trim().to_string(),
        });
    }
    Ok(rows)
}

impl WorkspaceIndex {
    /// Serializes structural AST outlines (`fn`, `method`, `struct`, `enum`, `trait`, `class`)
    /// from the current arena into compact `@TOON v1` positional tuples.
    pub fn outline_toon(&self, opts: ToonPageOptions) -> String {
        let rows: Vec<ToonRow> = self
            .arena
            .nodes
            .iter()
            .filter(|n| !opts.public_only || n.is_public)
            .filter(|n| !opts.exclude_tests || !n.is_test)
            .map(|n| ToonRow {
                subject: n.qualified_name.clone(),
                predicate: n.kind.as_str().to_string(),
                object: n.signature.clone(),
                meta: format!(
                    "lang:{},lines:{}-{},vis:{},scope:{}",
                    n.lang.as_str(),
                    n.start_line,
                    n.end_line,
                    if n.is_public { "pub" } else { "priv" },
                    if n.is_test { "test" } else { "src" }
                ),
            })
            .collect();

        render_toon(&rows, opts)
    }

    /// Runs deterministic thermal impact diffusion around `target` and encodes the ranked subgraph
    /// as `@TOON v1` tuples (`epicenter | relation | neighbor | temp_bp:...,hop:...,lines:...`).
    pub fn impact_toon(
        &self,
        target: &str,
        config: &DistillerConfig,
        opts: ToonPageOptions,
    ) -> Result<String, ScentError> {
        let epicenters = self.resolve_targets(target);
        if epicenters.is_empty() {
            return Err(ScentError::TargetNotFound {
                target: target.to_string(),
            });
        }

        let hits = distill_heat(&self.arena, &epicenters, config);
        let rows: Vec<ToonRow> = hits
            .into_iter()
            .filter_map(|hit| {
                let node = self.arena.nodes.get(hit.node_id)?;
                if opts.public_only && !node.is_public && hit.min_hop > 0 {
                    return None;
                }
                if opts.exclude_tests && node.is_test && hit.min_hop > 0 {
                    return None;
                }
                let subject = if epicenters.len() > 1 {
                    self.arena
                        .nodes
                        .get(hit.epicenter_id)
                        .map_or_else(|| target.trim().to_string(), |e| e.qualified_name.clone())
                } else {
                    target.trim().to_string()
                };
                let predicate = match hit.dominant_relation {
                    None => "epicenter".to_string(),
                    Some(rel) => rel.as_str().to_string(),
                };
                Some(ToonRow {
                    subject,
                    predicate,
                    object: node.qualified_name.clone(),
                    meta: format!(
                        "temp_bp:{},hop:{},lines:{}-{},vis:{},scope:{},sig:{}",
                        hit.temperature_bp,
                        hit.min_hop,
                        node.start_line,
                        node.end_line,
                        if node.is_public { "pub" } else { "priv" },
                        if node.is_test { "test" } else { "src" },
                        node.signature
                    ),
                })
            })
            .collect();

        Ok(render_toon(&rows, opts))
    }

    /// Resolves definitions and direct cross-references (callers, callees, implementors,
    /// and type users) for `target` from `ScentArena` and encodes them as `@TOON v1` tuples.
    pub fn references_toon(
        &self,
        target: &str,
        opts: ToonPageOptions,
    ) -> Result<String, ScentError> {
        let targets = self.resolve_targets(target);
        if targets.is_empty() {
            return Err(ScentError::TargetNotFound {
                target: target.to_string(),
            });
        }

        let impl_pairs: BTreeSet<(usize, usize)> = self
            .arena
            .edges
            .iter()
            .filter(|e| e.kind == RelationKind::Implements)
            .map(|e| (e.source_id, e.target_id))
            .collect();

        let mut rows: Vec<ToonRow> = Vec::new();
        let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();

        for &tid in &targets {
            let Some(node) = self.arena.nodes.get(tid) else {
                continue;
            };
            let key = (
                node.qualified_name.clone(),
                "defined_at".to_string(),
                node.signature.clone(),
            );
            if seen.insert(key) {
                rows.push(ToonRow {
                    subject: node.qualified_name.clone(),
                    predicate: "defined_at".to_string(),
                    object: node.signature.clone(),
                    meta: format!(
                        "kind:{},lang:{},lines:{}-{},vis:{},scope:{}",
                        node.kind.as_str(),
                        node.lang.as_str(),
                        node.start_line,
                        node.end_line,
                        if node.is_public { "pub" } else { "priv" },
                        if node.is_test { "test" } else { "src" }
                    ),
                });
            }
        }

        let mut ref_entries: Vec<(u8, String, String, String, String)> = Vec::new();
        for &tid in &targets {
            let Some(src_node) = self.arena.nodes.get(tid) else {
                continue;
            };
            for edge in &self.arena.edges {
                if edge.source_id != tid {
                    continue;
                }
                let Some(neighbor) = self.arena.nodes.get(edge.target_id) else {
                    continue;
                };
                if opts.public_only && !neighbor.is_public {
                    continue;
                }
                if opts.exclude_tests && neighbor.is_test {
                    continue;
                }
                let (rank, predicate) = match edge.kind {
                    RelationKind::CalledBy => (1u8, RelationKind::CalledBy.as_str()),
                    RelationKind::Implements => (2u8, RelationKind::Implements.as_str()),
                    RelationKind::UsedByType => {
                        if impl_pairs.contains(&(edge.target_id, tid)) {
                            (2u8, "implemented_by")
                        } else {
                            (3u8, RelationKind::UsedByType.as_str())
                        }
                    }
                    RelationKind::Calls => (4u8, RelationKind::Calls.as_str()),
                    RelationKind::UsesType => (5u8, RelationKind::UsesType.as_str()),
                };
                let meta = format!(
                    "kind:{},lang:{},lines:{}-{},vis:{},scope:{},sig:{}",
                    neighbor.kind.as_str(),
                    neighbor.lang.as_str(),
                    neighbor.start_line,
                    neighbor.end_line,
                    if neighbor.is_public { "pub" } else { "priv" },
                    if neighbor.is_test { "test" } else { "src" },
                    neighbor.signature
                );
                ref_entries.push((
                    rank,
                    src_node.qualified_name.clone(),
                    predicate.to_string(),
                    neighbor.qualified_name.clone(),
                    meta,
                ));
            }
        }

        ref_entries.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.3.cmp(&b.3))
                .then_with(|| a.2.cmp(&b.2))
        });

        for (_, subject, predicate, object, meta) in ref_entries {
            if seen.insert((subject.clone(), predicate.clone(), object.clone())) {
                rows.push(ToonRow {
                    subject,
                    predicate,
                    object,
                    meta,
                });
            }
        }

        Ok(render_toon(&rows, opts))
    }
}

fn escape_field(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out
}

fn split_escaped_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::with_capacity(4);
    let mut current = String::new();
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => current.push('\\'),
                Some('|') => current.push('|'),
                Some('n') => current.push('\n'),
                Some('r') => current.push('\r'),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => current.push('\\'),
            }
        } else if ch == '|' {
            fields.push(current);
            current = String::new();
        } else {
            current.push(ch);
        }
    }
    fields.push(current);
    fields
}
