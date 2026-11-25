use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cache;
use crate::distiller::{DistillerConfig, MAX_TEMPERATURE_BP, distill_heat};
use crate::lang::{ParsedFile, RawCall, RawTypeRef, parse_source};
use crate::ontology::{
    Edge, Entity, RelationKind, ScentArena, ScentError, SupportedLang, compute_signature_hash,
};

const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".onyx",
    ".agents",
    ".venv",
    "__pycache__",
    ".next",
];
const MAX_MULTI_TARGET_MATCHES: usize = 6;
/// Fast-path anti-racy: solo se puede saltar `fs::read` + `blake3` (stat-only hit) cuando el
/// `mtime` es idéntico al validado Y el archivo tiene ≥ 2 s de antigüedad (las ediciones
/// recientes, incluso de igual tamaño, siempre verifican el hash).
const STAT_ONLY_MIN_MTIME_AGE: Duration = Duration::from_secs(2);
const CACHE_FALLBACK_NAME: &str = "scent.idx";

/// Summary metrics for an incremental workspace indexing pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub total_files: usize,
    pub cache_hits: usize,
    pub reparsed_files: usize,
    pub total_entities: usize,
    pub total_edges: usize,
}

/// Aggregated semantic impact summary derived on demand (L3, never stored) for one or more
/// edited line ranges `(rel_path, line_start, line_end)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeImpactSummary {
    /// Maximum risk heat observed across non-epicenter callers/dependents (basis points `0..=10_000`).
    /// Returns `0` when the touched symbols have no external callers/dependents.
    pub max_heat_bp: u32,
    /// Number of unique non-epicenter callers/dependents reached within `cfg.iterations` hops.
    pub callers: usize,
    /// Maximum hop distance observed across reached non-epicenter dependents.
    pub hops: usize,
    /// `true` if any touched epicenter or reached dependent belongs to `scope:src` (`!is_test`)
    /// with public visibility.
    pub touches_public_src: bool,
    /// Qualified names of epicenter symbols whose line ranges intersected the edited hunks.
    pub epicenters: Vec<String>,
}

/// Incremental deterministic AST index across a multi-file workspace.
#[derive(Debug, Default)]
pub struct WorkspaceIndex {
    cache: BTreeMap<String, ParsedFile>,
    pub arena: ScentArena,
    dirty: bool,
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

    /// Indexes the whole workspace under `root`, persisting the incremental cache at
    /// `<root>/.onyx/scent.idx` (best-effort: a missing/corrupt file or a read-only
    /// workspace degrades silently to in-memory operation).
    pub fn index_workspace_with_cache(&mut self, root: &Path) -> Result<IndexStats, ScentError> {
        self.index_target_with_cache(root, None)
    }

    /// Scoped variant of [`Self::index_workspace_with_cache`] (one file/directory under `root`).
    pub fn index_target_with_cache(
        &mut self,
        root: &Path,
        scope: Option<&Path>,
    ) -> Result<IndexStats, ScentError> {
        let canon_root = root.canonicalize().map_err(|e| ScentError::Io {
            path: root.display().to_string(),
            message: e.to_string(),
        })?;
        let idx_path = cache::cache_path_for(&canon_root);
        if self.cache.is_empty() {
            let _ = self.load_cache(&idx_path);
        }
        let stats = self.index_target(&canon_root, scope)?;
        if self.dirty && self.save_cache(&idx_path).is_ok() {
            self.dirty = false;
        }
        Ok(stats)
    }

    /// Loads the persistent cache at `path` into memory (replacing the in-memory cache).
    /// Returns `true` on success; a missing, truncated or invalid file returns `false`
    /// and leaves the index clean.
    pub fn load_cache(&mut self, path: &Path) -> bool {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(_) => return false,
        };
        match cache::deserialize_cache(&bytes) {
            Ok(entries) => {
                self.cache = entries;
                self.dirty = false;
                true
            }
            Err(_) => {
                self.cache.clear();
                false
            }
        }
    }

    /// Atomically saves the in-memory cache to `path` (temp file + rename) and ensures
    /// `.gitignore` hygiene in the cache directory.
    pub fn save_cache(&self, path: &Path) -> Result<(), ScentError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ScentError::Io {
                path: parent.display().to_string(),
                message: e.to_string(),
            })?;
            cache::ensure_scent_gitignore(parent);
        }
        let payload = cache::serialize_cache(&self.cache);
        let stem = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| CACHE_FALLBACK_NAME.to_string());
        let tmp = path.with_file_name(format!("{stem}.tmp.{}", std::process::id()));
        fs::write(&tmp, &payload).map_err(|e| ScentError::Io {
            path: tmp.display().to_string(),
            message: e.to_string(),
        })?;
        match fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(first) => {
                // Windows: si el destino ya existe y el rename no lo reemplaza,
                // eliminarlo y reintentar el rename limpio.
                let _ = fs::remove_file(path);
                let result = fs::rename(&tmp, path).map_err(|e| {
                    let _ = fs::remove_file(&tmp);
                    ScentError::Io {
                        path: path.display().to_string(),
                        message: format!("rename failed ({first}); retry after remove failed: {e}"),
                    }
                });
                if result.is_err() {
                    let _ = fs::remove_file(&tmp);
                }
                result
            }
        }
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

        let raw_start = match scope {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => canon_root.join(p),
            None => canon_root.clone(),
        };

        let meta = fs::symlink_metadata(&raw_start).map_err(|e| ScentError::Io {
            path: raw_start.display().to_string(),
            message: e.to_string(),
        })?;
        if meta.file_type().is_symlink() {
            return Err(ScentError::Io {
                path: raw_start.display().to_string(),
                message: "symlink targets are ignored by policy".to_string(),
            });
        }
        let canon_start = raw_start.canonicalize().map_err(|e| ScentError::Io {
            path: raw_start.display().to_string(),
            message: e.to_string(),
        })?;
        if !canon_start.starts_with(&canon_root) {
            return Err(ScentError::Io {
                path: raw_start.display().to_string(),
                message: "scope path escapes workspace root".to_string(),
            });
        }

        let mut discovered = BTreeSet::new();
        if meta.is_file() {
            if let Some((rel, lang)) = supported_rel_file(&canon_root, &canon_start)? {
                discovered.insert((rel, lang));
            }
        } else if meta.is_dir() {
            collect_files_sorted(&canon_root, &canon_start, &mut discovered)?;
        }

        let mut stats = IndexStats {
            total_files: discovered.len(),
            ..IndexStats::default()
        };

        // Prune removed files only during full-workspace scans so scoped calls keep full cache warm.
        if scope.is_none() {
            let live_paths: BTreeSet<&str> = discovered.iter().map(|(p, _)| p.as_str()).collect();
            let before = self.cache.len();
            self.cache.retain(|k, _| live_paths.contains(k.as_str()));
            if self.cache.len() != before {
                self.dirty = true;
            }
        }

        let now = SystemTime::now();
        for (rel_path, lang) in &discovered {
            let abs_path = canon_root.join(rel_path);
            let meta = fs::metadata(&abs_path).map_err(|e| ScentError::Io {
                path: rel_path.clone(),
                message: e.to_string(),
            })?;
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let mtime_since_epoch = mtime.duration_since(UNIX_EPOCH).unwrap_or_default();
            let mtime_secs = mtime_since_epoch.as_secs();
            let mtime_nanos = mtime_since_epoch.subsec_nanos();
            let file_len = meta.len();

            // Stat-only fast path (anti-racy): stat idéntico al validado Y mtime ≥ 2 s
            // antiguo → sin `fs::read` ni `blake3`. Ediciones recientes siempre hash.
            let stat_matches_cache = self.cache.get(rel_path).is_some_and(|cached| {
                cached.lang == *lang
                    && cached.file_len == file_len
                    && cached.mtime_secs == mtime_secs
                    && cached.mtime_nanos == mtime_nanos
            });
            let mtime_old_enough = now
                .duration_since(mtime)
                .is_ok_and(|age| age >= STAT_ONLY_MIN_MTIME_AGE);
            if stat_matches_cache && mtime_old_enough {
                stats.cache_hits += 1;
                continue;
            }

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
                if let Some(cached) = self.cache.get_mut(rel_path) {
                    cached.file_len = file_len;
                    cached.mtime_secs = mtime_secs;
                    cached.mtime_nanos = mtime_nanos;
                }
                self.dirty = true;
                stats.cache_hits += 1;
            } else {
                let source = String::from_utf8(bytes).map_err(|e| ScentError::Parse {
                    path: rel_path.clone(),
                    message: format!("non-UTF-8 source file: {e}"),
                })?;
                let mut parsed = parse_source(rel_path, *lang, &source, hash)?;
                parsed.file_len = file_len;
                parsed.mtime_secs = mtime_secs;
                parsed.mtime_nanos = mtime_nanos;
                self.cache.insert(rel_path.clone(), parsed);
                self.dirty = true;
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
        struct FileLocalEntries {
            rel_path: String,
            local_to_global: Vec<usize>,
            raw_calls: Vec<RawCall>,
            raw_type_refs: Vec<RawTypeRef>,
        }

        let mut types_by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut file_local_map: Vec<FileLocalEntries> = Vec::new();

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
                let signature_hash =
                    compute_signature_hash(ent.kind, ent.is_public, &ent.signature);
                self.arena.nodes.push(Entity {
                    id,
                    qualified_name,
                    short_name: ent.short_name.clone(),
                    owner_type: ent.owner_type.clone(),
                    file: rel_path.clone(),
                    lang: parsed.lang,
                    kind: ent.kind,
                    signature: ent.signature.clone(),
                    signature_hash,
                    is_public: ent.is_public,
                    is_test: ent.is_test,
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
                        let type_short = owner.rsplit("::").next().unwrap_or(owner).to_string();
                        by_type_member
                            .entry((type_short, ent.short_name.clone()))
                            .or_default()
                            .push(id);
                    }
                } else {
                    types_by_name
                        .entry(ent.short_name.clone())
                        .or_default()
                        .push(id);
                }
            }

            file_local_map.push(FileLocalEntries {
                rel_path: rel_path.clone(),
                local_to_global,
                raw_calls: parsed.raw_calls.clone(),
                raw_type_refs: parsed.raw_type_refs.clone(),
            });
        }

        let mut unique_edges: BTreeSet<(usize, usize, RelationKind)> = BTreeSet::new();

        for FileLocalEntries {
            rel_path,
            local_to_global,
            raw_calls,
            raw_type_refs,
        } in file_local_map
        {
            for call in raw_calls {
                let Some(&caller_id) = local_to_global.get(call.caller_entity_idx) else {
                    continue;
                };

                let candidates: Option<&Vec<usize>> = if call.is_method_call {
                    if let Some(qual) = &call.qualifier {
                        if matches!(qual.as_str(), "self" | "Self" | "cls") {
                            self.arena.nodes[caller_id]
                                .owner_type
                                .as_ref()
                                .and_then(|owner| {
                                    let type_short =
                                        owner.rsplit("::").next().unwrap_or(owner).to_string();
                                    by_type_member.get(&(type_short, call.callee_name.clone()))
                                })
                                .or_else(|| {
                                    by_file_short.get(&(rel_path.clone(), call.callee_name.clone()))
                                })
                        } else {
                            by_type_member.get(&(qual.clone(), call.callee_name.clone()))
                        }
                    } else {
                        // Sin calificador en llamada de método (ej. `vec.push`, `x.parse`):
                        // limitar al mismo archivo y <= 2 coincidencias para evitar falsos positivos globales.
                        by_file_short
                            .get(&(rel_path.clone(), call.callee_name.clone()))
                            .filter(|v| v.len() <= 2)
                    }
                } else if let Some(qual) = &call.qualifier {
                    if matches!(qual.as_str(), "self" | "Self" | "cls") {
                        self.arena.nodes[caller_id]
                            .owner_type
                            .as_ref()
                            .and_then(|owner| {
                                let type_short =
                                    owner.rsplit("::").next().unwrap_or(owner).to_string();
                                by_type_member.get(&(type_short, call.callee_name.clone()))
                            })
                            .or_else(|| by_file_short.get(&(rel_path.clone(), call.callee_name)))
                    } else {
                        by_type_member
                            .get(&(qual.clone(), call.callee_name.clone()))
                            .or_else(|| {
                                by_file_short.get(&(rel_path.clone(), call.callee_name.clone()))
                            })
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

            for tref in raw_type_refs {
                let Some(&src_id) = local_to_global.get(tref.source_entity_idx) else {
                    continue;
                };
                let Some(targets) = types_by_name.get(&tref.target_type) else {
                    continue;
                };
                if targets.len() > MAX_MULTI_TARGET_MATCHES {
                    continue;
                }
                for &target_id in targets {
                    if src_id == target_id {
                        continue;
                    }
                    unique_edges.insert((src_id, target_id, tref.kind));
                    unique_edges.insert((target_id, src_id, RelationKind::UsedByType));
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

    /// Computes on-demand semantic impact (L3 derived, never stored) for one or more
    /// 1-based inclusive line ranges `(rel_path, line_start, line_end)`.
    ///
    /// Returns `None` when no indexed entity intersects any range (graceful degradation).
    pub fn impact_for_line_ranges(
        &self,
        ranges: &[(String, usize, usize)],
        cfg: &DistillerConfig,
    ) -> Option<RangeImpactSummary> {
        if ranges.is_empty() || self.arena.nodes.is_empty() {
            return None;
        }
        let norm_ranges: Vec<(String, usize, usize)> = ranges
            .iter()
            .map(|(p, s, e)| {
                let norm = p.trim().replace('\\', "/");
                let clean = norm
                    .trim_start_matches("./")
                    .trim_start_matches('/')
                    .to_string();
                let start = (*s).min(*e);
                let end = (*s).max(*e);
                (clean, start, end)
            })
            .collect();

        let mut epicenter_ids: Vec<usize> = Vec::new();
        let mut epicenters: Vec<String> = Vec::new();
        let mut touches_public_src = false;

        for node in &self.arena.nodes {
            let intersects = norm_ranges.iter().any(|(path, start, end)| {
                node.file == *path && node.start_line <= *end && node.end_line >= *start
            });
            if intersects {
                epicenter_ids.push(node.id);
                epicenters.push(node.qualified_name.clone());
                if node.is_public && !node.is_test {
                    touches_public_src = true;
                }
            }
        }

        if epicenter_ids.is_empty() {
            return None;
        }

        // Run thermal diffusion strictly over immutable `&self.arena` (L3 never stored).
        // For mutation blast-radius, count upstream/dependent callers (`CalledBy`, `UsedByType`, `Implements`)
        // as well as all non-epicenter hits reached within `cfg.iterations`.
        let hits = distill_heat(&self.arena, &epicenter_ids, cfg);

        let mut base_max_heat: u32 = 0;
        let mut callers: usize = 0;
        let mut max_hop: usize = 0;

        for hit in &hits {
            if hit.min_hop == 0 {
                continue;
            }
            // Only treat reverse-impact relations (callers / type users / implementors) as risk callers.
            let is_dependent = matches!(
                hit.dominant_relation,
                Some(RelationKind::CalledBy | RelationKind::UsedByType | RelationKind::Implements)
            );
            if !is_dependent {
                continue;
            }
            callers += 1;
            max_hop = max_hop.max(usize::from(hit.min_hop));
            base_max_heat = base_max_heat.max(u32::from(hit.temperature_bp));
            if let Some(node) = self.arena.nodes.get(hit.node_id) {
                if node.is_public && !node.is_test {
                    touches_public_src = true;
                }
            }
        }

        // Aggregate via max (not sum) with bounded fan-out boost so hubs with many callers
        // escalate cleanly while isolated 0-caller helpers stay at 0bp.
        let max_heat_bp = if callers == 0 {
            0
        } else {
            let fanout_bonus = (callers.saturating_sub(1) as u32)
                .saturating_mul(400)
                .min(3_250);
            base_max_heat
                .saturating_add(fanout_bonus)
                .min(u32::from(MAX_TEMPERATURE_BP))
        };

        Some(RangeImpactSummary {
            max_heat_bp,
            callers,
            hops: max_hop,
            touches_public_src,
            epicenters,
        })
    }

    /// Captures a deterministic snapshot of `qualified_name -> signature_hash` for all indexed symbols.
    pub fn signature_snapshot(&self) -> BTreeMap<String, u64> {
        self.arena
            .nodes
            .iter()
            .map(|n| (n.qualified_name.clone(), n.signature_hash))
            .collect()
    }

    /// Returns all symbols whose `signature_hash` changed relative to `previous` (sorted deterministically).
    pub fn diff_changed_signatures(&self, previous: &BTreeMap<String, u64>) -> Vec<String> {
        let mut changed = Vec::new();
        for node in &self.arena.nodes {
            if let Some(&old_hash) = previous.get(&node.qualified_name) {
                if old_hash != node.signature_hash {
                    changed.push(node.qualified_name.clone());
                }
            }
        }
        changed.sort();
        changed.dedup();
        changed
    }

    /// Walks reverse dependency edges (`CalledBy`, `UsedByType`, `Implements`) from `changed_symbols`
    /// up to `max_hops` (`1..=6`), returning sorted unique caller/dependent qualified names.
    pub fn stale_callers_for_changed_signatures(
        &self,
        changed_symbols: &[String],
        max_hops: usize,
    ) -> Vec<String> {
        if changed_symbols.is_empty() || self.arena.nodes.is_empty() {
            return Vec::new();
        }
        let max_h = max_hops.clamp(1, 6);
        let mut seeds: BTreeSet<usize> = BTreeSet::new();
        for sym in changed_symbols {
            for id in self.resolve_targets(sym) {
                seeds.insert(id);
            }
        }
        if seeds.is_empty() {
            return Vec::new();
        }

        let mut visited: BTreeSet<usize> = seeds.clone();
        let mut queue: VecDeque<(usize, usize)> = seeds.iter().map(|&id| (id, 0)).collect();
        let mut stale: BTreeSet<String> = BTreeSet::new();

        while let Some((cur, depth)) = queue.pop_front() {
            if depth >= max_h {
                continue;
            }
            for edge in &self.arena.edges {
                if edge.source_id != cur {
                    continue;
                }
                if !matches!(
                    edge.kind,
                    RelationKind::CalledBy | RelationKind::UsedByType | RelationKind::Implements
                ) {
                    continue;
                }
                if visited.insert(edge.target_id) {
                    if let Some(node) = self.arena.nodes.get(edge.target_id) {
                        stale.insert(node.qualified_name.clone());
                    }
                    queue.push_back((edge.target_id, depth + 1));
                }
            }
        }

        stale.into_iter().collect()
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
