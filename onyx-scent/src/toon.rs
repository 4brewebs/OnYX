use crate::distiller::{DistillerConfig, distill_heat};
use crate::index::WorkspaceIndex;
use crate::ontology::ScentError;

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
