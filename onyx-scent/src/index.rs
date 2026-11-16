use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::lang::{ParsedFile, parse_source};
use crate::ontology::{Edge, Entity, RelationKind, ScentArena, ScentError, SupportedLang};

const IGNORED_DIRS: &[&str] = &[".git", "target", "node_modules"];
const MAX_MULTI_TARGET_MATCHES: usize = 6;

/// Summary metrics for an incremental workspace indexing pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub total_files: usize,
    pub cache_hits: usize,
    pub reparsed_files: usize,
    pub total_entities: usize,
    pub total_edges: usize,
}

/// Incremental deterministic AST index across a multi-file workspace.
#[derive(Debug, Default)]
pub struct WorkspaceIndex {
    cache: BTreeMap<String, ParsedFile>,
    pub arena: ScentArena,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Indexes all supported source files under `root` deterministically,
    /// reusing cached AST subgraphs whenever a file's `blake3` hash matches.
    pub fn index_workspace(&mut self, root: &Path) -> Result<IndexStats, ScentError> {
        self.index_target(root, None)
    }

    /// Indexes either the whole workspace (`scope = None`) or a specific file/directory under `root`.
    pub fn index_target(
        &mut self,
        root: &Path,
        scope: Option<&Path>,
    ) -> Result<IndexStats, ScentError> {
        let canon_root = root.canonicalize().map_err(|e| ScentError::Io {
            path: root.display().to_string(),
            message: e.to_string(),
        })?;

        let start_dir = match scope {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => canon_root.join(p),
            None => canon_root.clone(),
        };

        let meta = fs::symlink_metadata(&start_dir).map_err(|e| ScentError::Io {
            path: start_dir.display().to_string(),
            message: e.to_string(),
        })?;
        if meta.file_type().is_symlink() {
            return Err(ScentError::Io {
                path: start_dir.display().to_string(),
                message: "symlink targets are ignored by policy".to_string(),
            });
        }

        let mut discovered = BTreeSet::new();
        if meta.is_file() {
            if let Some((rel, lang)) = supported_rel_file(&canon_root, &start_dir)? {
                discovered.insert((rel, lang));
            }
        } else if meta.is_dir() {
            collect_files_sorted(&canon_root, &start_dir, &mut discovered)?;
        }

        let mut stats = IndexStats {
            total_files: discovered.len(),
            ..IndexStats::default()
        };

        // Prune removed files only during full-workspace scans so scoped calls keep full cache warm.
        if scope.is_none() {
            let live_paths: BTreeSet<&str> = discovered.iter().map(|(p, _)| p.as_str()).collect();
            self.cache.retain(|k, _| live_paths.contains(k.as_str()));
        }

        for (rel_path, lang) in &discovered {
            let abs_path = canon_root.join(rel_path);
            let bytes = fs::read(&abs_path).map_err(|e| ScentError::Io {
                path: rel_path.clone(),
                message: e.to_string(),
            })?;
            let hash: [u8; 32] = *blake3::hash(&bytes).as_bytes();
            let cache_hit = self
                .cache
                .get(rel_path)
                .is_some_and(|cached| cached.blake3_hash == hash && cached.lang == *lang);

            if cache_hit {
                stats.cache_hits += 1;
            } else {
                let source = String::from_utf8(bytes).map_err(|e| ScentError::Parse {
                    path: rel_path.clone(),
                    message: format!("non-UTF-8 source file: {e}"),
                })?;
                let parsed = parse_source(rel_path, *lang, &source, hash)?;
                self.cache.insert(rel_path.clone(), parsed);
                stats.reparsed_files += 1;
            }
        }

        let active_paths: Vec<String> = discovered.into_iter().map(|(p, _)| p).collect();
        self.rebuild_arena(&active_paths);
        stats.total_entities = self.arena.nodes.len();
        stats.total_edges = self.arena.edges.len();
        Ok(stats)
    }

    fn rebuild_arena(&mut self, active_paths: &[String]) {
        self.arena = ScentArena::new();

        let mut qualified_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_short: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_type_member: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        let mut by_file_short: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        let mut file_local_map: Vec<(String, Vec<usize>, Vec<crate::lang::RawCall>)> = Vec::new();

        for rel_path in active_paths {
            let Some(parsed) = self.cache.get(rel_path) else {
                continue;
            };
            let mut local_to_global = Vec::with_capacity(parsed.entities.len());

            for ent in &parsed.entities {
                let base_q = match &ent.owner_type {
                    Some(owner) => format!("{rel_path}::{owner}::{}", ent.short_name),
                    None => format!("{rel_path}::{}", ent.short_name),
                };
                let seen = qualified_counts.entry(base_q.clone()).or_insert(0);
                *seen += 1;
                let qualified_name = if *seen == 1 {
                    base_q
                } else {
                    format!("{base_q}#{}", *seen)
                };

                let id = self.arena.nodes.len();
                self.arena.nodes.push(Entity {
                    id,
                    qualified_name,
                    short_name: ent.short_name.clone(),
                    owner_type: ent.owner_type.clone(),
                    file: rel_path.clone(),
                    lang: parsed.lang,
                    kind: ent.kind,
                    signature: ent.signature.clone(),
                    is_public: ent.is_public,
                    start_line: ent.start_line,
                    end_line: ent.end_line,
                });
                local_to_global.push(id);

                if ent.kind.is_callable() {
                    by_short.entry(ent.short_name.clone()).or_default().push(id);
                    by_file_short
                        .entry((rel_path.clone(), ent.short_name.clone()))
                        .or_default()
                        .push(id);
                    if let Some(owner) = &ent.owner_type {
                        by_type_member
                            .entry((owner.clone(), ent.short_name.clone()))
                            .or_default()
                            .push(id);
                    }
                }
            }

            file_local_map.push((rel_path.clone(), local_to_global, parsed.raw_calls.clone()));
        }

        let mut unique_edges: BTreeSet<(usize, usize, RelationKind)> = BTreeSet::new();

        for (rel_path, local_to_global, raw_calls) in file_local_map {
            for call in raw_calls {
                let Some(&caller_id) = local_to_global.get(call.caller_entity_idx) else {
                    continue;
                };

                let candidates: Option<&Vec<usize>> = if let Some(qual) = &call.qualifier {
                    if qual == "self" || qual == "Self" {
                        self.arena.nodes[caller_id]
                            .owner_type
                            .as_ref()
                            .and_then(|owner| {
                                by_type_member.get(&(owner.clone(), call.callee_name.clone()))
                            })
                            .or_else(|| by_file_short.get(&(rel_path.clone(), call.callee_name)))
                    } else {
                        by_type_member
                            .get(&(qual.clone(), call.callee_name.clone()))
                            .or_else(|| by_short.get(&call.callee_name))
                    }
                } else {
                    by_file_short
                        .get(&(rel_path.clone(), call.callee_name.clone()))
                        .or_else(|| by_short.get(&call.callee_name))
                };

                let Some(targets) = candidates else {
                    continue;
                };
                if targets.len() > MAX_MULTI_TARGET_MATCHES {
                    continue;
                }

                for &target_id in targets {
                    if caller_id == target_id {
                        continue;
                    }
                    unique_edges.insert((caller_id, target_id, RelationKind::Calls));
                    unique_edges.insert((target_id, caller_id, RelationKind::CalledBy));
                }
            }
        }

        for (source_id, target_id, kind) in unique_edges {
            self.arena.edges.push(Edge {
                source_id,
                target_id,
                kind,
                weight_bp: kind.default_weight_bp(),
            });
        }
    }

    /// Resolves matching arena node IDs for a symbol target query deterministically.
    pub fn resolve_targets(&self, target: &str) -> Vec<usize> {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }

        let exact: Vec<usize> = self
            .arena
            .nodes
            .iter()
            .filter(|n| n.qualified_name == trimmed)
            .map(|n| n.id)
            .collect();
        if !exact.is_empty() {
            return exact;
        }

        let suffix_pattern = format!("::{trimmed}");
        let suffix: Vec<usize> = self
            .arena
            .nodes
            .iter()
            .filter(|n| n.qualified_name.ends_with(&suffix_pattern))
            .map(|n| n.id)
            .collect();
        if !suffix.is_empty() {
            return suffix;
        }

        self.arena
            .nodes
            .iter()
            .filter(|n| n.short_name == trimmed)
            .map(|n| n.id)
            .collect()
    }
}

fn collect_files_sorted(
    root: &Path,
    dir: &Path,
    out: &mut BTreeSet<(String, SupportedLang)>,
) -> Result<(), ScentError> {
    let entries = fs::read_dir(dir).map_err(|e| ScentError::Io {
        path: dir.display().to_string(),
        message: e.to_string(),
    })?;

    let mut children: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        children.push(entry.path());
    }
    children.sort();

    for path in children {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }
            collect_files_sorted(root, &path, out)?;
        } else if meta.is_file() {
            if let Some(item) = supported_rel_file(root, &path)? {
                out.insert(item);
            }
        }
    }
    Ok(())
}

fn supported_rel_file(
    root: &Path,
    path: &Path,
) -> Result<Option<(String, SupportedLang)>, ScentError> {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return Ok(None);
    };
    let Some(lang) = SupportedLang::from_extension(ext) else {
        return Ok(None);
    };

    let canon = path.canonicalize().map_err(|e| ScentError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    if !canon.starts_with(root) {
        return Ok(None);
    }
    let rel = canon
        .strip_prefix(root)
        .map(normalize_rel)
        .map_err(|e| ScentError::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
    Ok(Some((rel, lang)))
}

fn normalize_rel(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    parts.join("/")
}
