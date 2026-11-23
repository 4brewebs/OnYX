//! Operaciones offline de diagnóstico e inspección (`inspect`, `rewind`, `doctor`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use onyx_soul_agents::{
    AgentConfig, AgentGraph, Graph, GraphView, HeadlessDriver, Transcript, Value,
};
use onyx_workspace::{
    RecordedEffect, SandboxDriverKind, SandboxMode, WorkspacePolicy, WorktreeManager,
    all_recorded_effects, audit_sprout_workspace, check_git_version, detect_sandbox_capabilities,
    effects_after_tick, graph_workspace_mode, graph_workspace_plan, is_effect_undone,
    restore_git_tree_checkpoint_for_targets,
};
use serde::Serialize;
use serde_json::json;

use crate::pack::{PackSnapshotStore, SnapshotSource};
use crate::session::build_host;
use crate::vault::audit_vault;

#[derive(Serialize)]
struct InspectGitCheckpointJson {
    tree_sha: String,
    head_sha: Option<String>,
    tool: String,
    target: String,
}

#[derive(Serialize)]
struct InspectMilestoneJson {
    tick: u64,
    tag: String,
    stored_bytes: u64,
    raw_bytes: u64,
    checksum_fnv1a: String,
    source: &'static str,
    git_checkpoint: Option<InspectGitCheckpointJson>,
}

#[derive(Serialize)]
struct InspectMemoryJson {
    kind: String,
    text: String,
}

#[derive(Serialize)]
struct InspectPendingApprovalJson {
    tool: String,
    args: String,
}

#[derive(Serialize)]
struct InspectAgentJson {
    id: u64,
    name: String,
    state: Option<&'static str>,
    request: u64,
    goal: String,
    tools: Vec<String>,
    pending_approval: Option<InspectPendingApprovalJson>,
    result: Option<String>,
    memory: Vec<InspectMemoryJson>,
}

#[derive(Serialize)]
struct InspectRecordedCheckpointJson {
    concept_id: u64,
    tick: u64,
    agent: u64,
    tool: String,
    target: String,
    reversibility: String,
    summary: String,
    head_sha: Option<String>,
    tree_sha: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    symbols: Vec<String>,
    undone: bool,
}

#[derive(Serialize)]
struct InspectLinkJson {
    rel: String,
    target: u64,
}

#[derive(Serialize)]
struct InspectConceptJson {
    id: u64,
    props: BTreeMap<String, serde_json::Value>,
    links: Vec<InspectLinkJson>,
}

#[derive(Serialize)]
struct InspectSnapshotJson {
    tick: u64,
    tag: String,
    raw_bytes: u64,
    stored_bytes: u64,
    checksum_fnv1a: String,
    total_concepts: usize,
    workspace_mode: Option<&'static str>,
    live_plan: Option<String>,
    agents: Vec<InspectAgentJson>,
    checkpoints: Vec<InspectRecordedCheckpointJson>,
    concepts: Vec<InspectConceptJson>,
}

pub fn render_inspect(
    dir: &Path,
    tick: Option<u64>,
    list: bool,
    json_output: bool,
    config: AgentConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let store = PackSnapshotStore::new(dir);

    if list {
        return render_inspect_list(dir, &store, json_output, config);
    }

    let target_tick = match tick {
        Some(t) => {
            if !store.has_tick(t) {
                return Err(format!(
                    "snapshot for tick {t} not found in `{}` (available milestone ticks: {:?}; use --list)",
                    dir.display(),
                    store.ticks()
                )
                .into());
            }
            t
        }
        None => store
            .latest_tick()
            .ok_or("no snapshots found in directory")?,
    };

    let dir_buf = dir.to_path_buf();
    let (mut host, ws_vocab) = build_host(1, config, Some(&dir_buf), None)?;
    host.request_restore(&target_tick.to_string());
    host.step(&mut HeadlessDriver::new());

    let meta = store.get_meta(target_tick);
    let tag = meta.map(|m| m.tag.as_str()).unwrap_or("unknown");

    if json_output {
        let concepts_ids = host.graph().query(&[]);
        let agents = host
            .agents()
            .into_iter()
            .map(|id| {
                let view = host.view(id);
                InspectAgentJson {
                    id: id.0,
                    name: view.name,
                    state: host.agent_state(id).map(|s| s.as_str()),
                    request: view.request,
                    goal: view.goal,
                    tools: view.tools,
                    pending_approval: view.pending_approval.map(|p| InspectPendingApprovalJson {
                        tool: p.tool,
                        args: p.args,
                    }),
                    result: host.result(id),
                    memory: view
                        .memory
                        .into_iter()
                        .map(|(kind, text)| InspectMemoryJson { kind, text })
                        .collect(),
                }
            })
            .collect();

        let checkpoints = all_recorded_effects(host.graph(), &ws_vocab)
            .into_iter()
            .map(|eff| {
                let undone = is_effect_undone(host.graph(), &ws_vocab, eff.id);
                InspectRecordedCheckpointJson {
                    concept_id: eff.id.0,
                    tick: eff.tick,
                    agent: eff.agent,
                    tool: eff.tool,
                    target: eff.target,
                    reversibility: eff.reversibility,
                    summary: eff.summary,
                    head_sha: eff.head_sha,
                    tree_sha: eff.tree_sha,
                    symbols: eff.symbols,
                    undone,
                }
            })
            .collect();

        let concepts = concepts_ids
            .iter()
            .map(|id| {
                let mut props = BTreeMap::new();
                for (name, val) in host.graph().props(*id) {
                    let jv = match val {
                        Value::Bool(b) => json!(b),
                        Value::Int(i) => json!(i),
                        Value::Float(f) => json!(f),
                        Value::Text(s) => json!(s),
                        Value::Concept(c) => json!({ "concept": c.0 }),
                    };
                    props.insert(name, jv);
                }
                let links = host
                    .graph()
                    .links(*id)
                    .into_iter()
                    .map(|(rel, to)| InspectLinkJson { rel, target: to.0 })
                    .collect();
                InspectConceptJson {
                    id: id.0,
                    props,
                    links,
                }
            })
            .collect();

        let doc = InspectSnapshotJson {
            tick: target_tick,
            tag: tag.to_string(),
            raw_bytes: meta.map(|m| m.raw_len).unwrap_or(0),
            stored_bytes: meta.map(|m| m.stored_len).unwrap_or(0),
            checksum_fnv1a: format!("{:016x}", meta.map(|m| m.checksum).unwrap_or(0)),
            total_concepts: concepts_ids.len(),
            workspace_mode: graph_workspace_mode(host.graph(), &ws_vocab).map(|m| m.as_label()),
            live_plan: graph_workspace_plan(host.graph(), &ws_vocab),
            agents,
            checkpoints,
            concepts,
        };
        return Ok(serde_json::to_string_pretty(&doc)?);
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "=== openOnyx Graph Snapshot @ tick {target_tick} [{tag}] ==="
    );
    let concepts = host.graph().query(&[]);
    let _ = writeln!(out, "Total concepts: {}", concepts.len());
    for id in host.agents() {
        let view = host.view(id);
        let _ = writeln!(
            out,
            "\nAgent {} (`{}`) state={:?} req={}",
            id.0,
            view.name,
            host.agent_state(id),
            view.request
        );
        let _ = writeln!(out, "  goal: {}", view.goal);
        let _ = writeln!(out, "  tools: {:?}", view.tools);
        let _ = writeln!(out, "  memory ({} entries):", view.memory.len());
        for (k, body) in &view.memory {
            let preview: String = body.chars().take(120).collect();
            let _ = writeln!(out, "    - [{k}] {preview}");
        }
        if let Some(p) = &view.pending_approval {
            let _ = writeln!(out, "  pending_approval: {} {}", p.tool, p.args);
        }
        if let Some(res) = host.result(id) {
            let _ = writeln!(out, "  result: {res}");
        }
    }
    Ok(out)
}

fn render_inspect_list(
    dir: &Path,
    store: &PackSnapshotStore,
    json_output: bool,
    config: AgentConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let entries = store.entries();
    if entries.is_empty() {
        return Err("no snapshots found in directory".into());
    }

    let dir_buf = dir.to_path_buf();
    let latest = entries.last().expect("non-empty entries").tick;
    let mut effects_by_tick: BTreeMap<u64, RecordedEffect> = BTreeMap::new();
    if let Ok((mut probe_host, probe_ws_vocab)) = build_host(1, config, Some(&dir_buf), None) {
        probe_host.request_restore(&latest.to_string());
        probe_host.step(&mut HeadlessDriver::new());
        for eff in all_recorded_effects(probe_host.graph(), &probe_ws_vocab) {
            if eff.tree_sha.is_some() {
                effects_by_tick.insert(eff.tick, eff);
            }
        }
    }

    if json_output {
        let items: Vec<InspectMilestoneJson> = entries
            .iter()
            .map(|meta| {
                let git_checkpoint = effects_by_tick.get(&meta.tick).and_then(|eff| {
                    eff.tree_sha
                        .as_ref()
                        .map(|tree_sha| InspectGitCheckpointJson {
                            tree_sha: tree_sha.clone(),
                            head_sha: eff.head_sha.clone(),
                            tool: eff.tool.clone(),
                            target: eff.target.clone(),
                        })
                });
                InspectMilestoneJson {
                    tick: meta.tick,
                    tag: meta.tag.clone(),
                    stored_bytes: meta.stored_len,
                    raw_bytes: meta.raw_len,
                    checksum_fnv1a: format!("{:016x}", meta.checksum),
                    source: match meta.source {
                        SnapshotSource::PackFrame { .. } => "pack",
                        SnapshotSource::LegacyBin(_) => "legacy_bin",
                    },
                    git_checkpoint,
                }
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&items)?);
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "=== openOnyx Milestones in `{}` ({} snapshots) ===",
        dir.display(),
        entries.len()
    );
    let _ = writeln!(
        out,
        "  {:>4}  {:<22} {:>9}  {:>9}  {:<16}  GIT CHECKPOINT",
        "TICK", "TAG", "STORED", "RAW", "CHECKSUM"
    );
    for meta in &entries {
        let ckpt = effects_by_tick
            .get(&meta.tick)
            .and_then(|eff| {
                eff.tree_sha.as_deref().map(|sha| {
                    let short: String = sha.chars().take(12).collect();
                    format!("{}:{} ({short})", eff.tool, eff.target)
                })
            })
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(
            out,
            "  {:>4}  {:<22} {:>7} B  {:>7} B  {:016x}  {}",
            meta.tick, meta.tag, meta.stored_len, meta.raw_len, meta.checksum, ckpt
        );
    }
    Ok(out)
}

pub fn run_inspect(
    dir: &Path,
    tick: Option<u64>,
    list: bool,
    json: bool,
    config: AgentConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let rendered = render_inspect(dir, tick, list, json, config)?;
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RewindReport {
    pub orphaned_count: usize,
    pub restored_tree_sha: Option<String>,
    pub lines: Vec<String>,
}

pub fn reconcile_rewind_effects_report(
    workspace: &Path,
    dir: &Path,
    tick: u64,
    config: &AgentConfig,
    dry_run: bool,
) -> Result<RewindReport, Box<dyn std::error::Error>> {
    let dir_buf = dir.to_path_buf();
    let store = PackSnapshotStore::new(dir);
    let Some(latest) = store.latest_tick() else {
        return Ok(RewindReport::default());
    };
    if latest <= tick {
        return Ok(RewindReport::default());
    }

    let (mut probe_host, probe_ws_vocab) = build_host(1, config.clone(), Some(&dir_buf), None)?;
    probe_host.request_restore(&latest.to_string());
    probe_host.step(&mut HeadlessDriver::new());
    let orphaned = effects_after_tick(probe_host.graph(), &probe_ws_vocab, tick);
    if orphaned.is_empty() {
        return Ok(RewindReport::default());
    }

    let mut report = RewindReport {
        orphaned_count: orphaned.len(),
        restored_tree_sha: None,
        lines: Vec::with_capacity(orphaned.len() + 2),
    };
    report.lines.push(format!(
        "=== [openOnyx rewind] Side-Effect Ledger ({} effects after tick {tick}) ===",
        orphaned.len()
    ));
    for eff in &orphaned {
        let ckpt_info = eff
            .tree_sha
            .as_deref()
            .map(|sha| format!(" [tree:{sha}]"))
            .unwrap_or_default();
        let sym_info = if eff.symbols.is_empty() {
            String::new()
        } else {
            format!(" [symbols:{}]", eff.symbols.join(","))
        };
        report.lines.push(format!(
            "  - tick={} agent={} [{}] {} -> {} ({}){}{}",
            eff.tick,
            eff.agent,
            eff.reversibility,
            eff.tool,
            eff.target,
            eff.summary,
            ckpt_info,
            sym_info
        ));
    }
    if let Some(tree_sha) = orphaned.iter().find_map(|e| e.tree_sha.as_deref()) {
        if dry_run {
            report.lines.push(format!(
                "[openOnyx rewind --dry-run] would restore workspace working tree to checkpoint {tree_sha}"
            ));
        } else {
            let created_targets: Vec<&str> = orphaned
                .iter()
                .filter(|e| e.tool == "fs_write")
                .map(|e| e.target.as_str())
                .collect();
            restore_git_tree_checkpoint_for_targets(workspace, tree_sha, &created_targets)?;
            report.restored_tree_sha = Some(tree_sha.to_string());
            report.lines.push(format!(
                "[openOnyx rewind] restored workspace working tree to checkpoint {tree_sha}"
            ));
        }
    }
    Ok(report)
}

pub fn reconcile_rewind_effects(
    workspace: &Path,
    dir: &Path,
    tick: u64,
    config: &AgentConfig,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let report = reconcile_rewind_effects_report(workspace, dir, tick, config, dry_run)?;
    for line in report.lines {
        println!("{line}");
    }
    Ok(())
}

#[derive(Default)]
pub struct DoctorReport {
    pub checks: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl DoctorReport {
    pub fn print(&self) {
        println!("=== [openOnyx doctor] ===");
        for item in &self.checks {
            println!("  [OK]   {item}");
        }
        for warn in &self.warnings {
            println!("  [WARN] {warn}");
        }
        for err in &self.errors {
            println!("  [ERR]  {err}");
        }
    }
}

pub fn run_doctor(
    workspace: &Path,
    snapshot_dir: Option<&PathBuf>,
    transcript: Option<&PathBuf>,
) -> DoctorReport {
    let mut report = DoctorReport::default();

    let git_version_ok = match check_git_version() {
        Ok(ver) => {
            report.checks.push(format!(
                "git version {ver} (>= 2.38.0; 3-way worktree merge-tree ready)"
            ));
            true
        }
        Err(err) => {
            report.warnings.push(format!(
                "git >= 2.38.0 check failed (async subagent worktree 3-way merge disabled): {err}"
            ));
            false
        }
    };

    let git_ok = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(workspace)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if git_ok {
        report
            .checks
            .push(format!("git worktree valid at {}", workspace.display()));
        if git_version_ok {
            match WorktreeManager::new(workspace) {
                Ok(mgr) => match mgr.prune_worktrees() {
                    Ok(()) => {
                        let active_sub = mgr
                            .list_worktrees()
                            .map(|list| {
                                list.into_iter()
                                    .filter(|w| w.is_onyx_subagent)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        report.checks.push(format!(
                            "git worktrees pruned (active onyx subagent worktrees: {})",
                            active_sub.len()
                        ));
                        if !active_sub.is_empty() {
                            let names: Vec<String> = active_sub
                                .iter()
                                .map(|w| {
                                    w.branch
                                        .clone()
                                        .unwrap_or_else(|| w.path.display().to_string())
                                })
                                .collect();
                            report.warnings.push(format!(
                                "leftover onyx subagent worktree(s) detected: {}",
                                names.join(", ")
                            ));
                        }
                    }
                    Err(e) => report
                        .warnings
                        .push(format!("failed to prune git worktrees: {e}")),
                },
                Err(e) => report
                    .warnings
                    .push(format!("worktree manager initialization skipped: {e}")),
            }
        }
    } else {
        report.warnings.push(format!(
            "workspace `{}` is not a git worktree",
            workspace.display()
        ));
    }

    // Auditoría de sandbox OS y capacidades del host (REQ-06.6, TEST-06.5).
    let sandbox_caps = detect_sandbox_capabilities();
    let loaded_policy = WorkspacePolicy::new(workspace)
        .map(|p| p.with_loaded_config())
        .ok();
    let active_sandbox = loaded_policy
        .as_ref()
        .map(|p| p.sandbox.clone())
        .unwrap_or_default();
    let cli_label = sandbox_caps.container_cli.unwrap_or("none");
    let effective_badge = active_sandbox
        .mode
        .effective_badge(sandbox_caps.native_driver);
    report.checks.push(format!(
        "sandbox driver: {} (mode={}, badge={}, net={}, container_cli={cli_label})",
        sandbox_caps.native_driver.as_str(),
        active_sandbox.mode.as_str(),
        effective_badge,
        active_sandbox.network.as_str()
    ));
    if active_sandbox.mode == SandboxMode::Off {
        report.warnings.push(
            "sandbox mode is `off`; `shell_exec` runs without OS sandbox or env_clear isolation"
                .to_string(),
        );
    } else if active_sandbox.mode == SandboxMode::WorkspaceWrite
        && sandbox_caps.native_driver == SandboxDriverKind::Win32Job
    {
        report.warnings.push(
            "Windows `workspace-write` runs as `env+job` (env_clear + Win32 Job Objects + best-effort network filter, no kernel FS/network namespace isolation); use `--sandbox strict-container` for untrusted workloads"
                .to_string(),
        );
    } else if sandbox_caps.native_driver == SandboxDriverKind::None {
        report.warnings.push(
            "no native OS sandbox driver detected on host (only env_clear and timeout apply)"
                .to_string(),
        );
    }

    // Auditoría de Workspace Trust (`~/.onyx/trusted_workspaces.toml`, hashes `blake3`).
    let trust = onyx_workspace::evaluate_workspace_trust(workspace, None);
    let cfg_short = trust
        .config_blake3
        .as_deref()
        .map(|h| &h[..h.len().min(12)])
        .unwrap_or("none");
    let sprout_short = trust
        .sprout_blake3
        .as_deref()
        .map(|h| &h[..h.len().min(12)])
        .unwrap_or("none");
    if trust.is_fully_trusted() {
        report.checks.push(format!(
            "workspace trust: {} (config={cfg_short}, sprout={sprout_short})",
            trust.status_label()
        ));
    } else {
        report.warnings.push(format!(
            "workspace trust: {} (config={cfg_short}, sprout={sprout_short}) — project allow/sandbox relaxations ignored and sprout.toml blocked; inspect files and run `openonyx trust` to approve",
            trust.status_label()
        ));
    }
    if let Some(ref pol) = loaded_policy {
        for warn in &pol.trust_warnings {
            report.warnings.push(warn.clone());
        }
    }

    // Auditoría de `sprout.toml` (sintaxis, DAG acíclico, runtime OCI y puertos ocupados — REQ-08.6).
    if let Some(sprout_audit) = audit_sprout_workspace(workspace) {
        report.checks.extend(sprout_audit.checks);
        report.warnings.extend(sprout_audit.warnings);
        report.errors.extend(sprout_audit.errors);
    }

    // Auditoría del vault de sesiones `.onyx/vault/`.
    let vault_audit = audit_vault(workspace);
    if vault_audit.exists {
        report.checks.push(format!(
            "session vault `.onyx/vault` ready ({} session dir(s), {} unassigned transcript(s))",
            vault_audit.session_dirs, vault_audit.unassigned_transcripts
        ));
        if !vault_audit.gitignore_present {
            report.warnings.push(
                "`.onyx/vault/.gitignore` is missing; add `*` so git checkpoints never index session snapshots"
                    .to_string(),
            );
        }
    }

    if let Ok(entries) = fs::read_dir(workspace) {
        let mut temp_artifacts = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tr") || (name.starts_with("snaps") && entry.path().is_dir()) {
                temp_artifacts.push(name);
            }
        }
        temp_artifacts.sort();
        if temp_artifacts.is_empty() {
            report
                .checks
                .push("no stray *.tr or snaps* artifacts in workspace root".to_string());
        } else {
            report.warnings.push(format!(
                "temporary artifacts in workspace root: {} (consider using `--auto-save` or `.onyx/vault/`)",
                temp_artifacts.join(", ")
            ));
        }
    }

    if let Some(tr_path) = transcript {
        match fs::read(tr_path) {
            Err(e) => report.errors.push(format!(
                "failed to read transcript `{}`: {e}",
                tr_path.display()
            )),
            Ok(bytes) => {
                if !bytes.starts_with(b"ONYXTR2") && !bytes.starts_with(b"ONYXTR1") {
                    report.errors.push(format!(
                        "transcript `{}` missing ONYXTR1/ONYXTR2 header",
                        tr_path.display()
                    ));
                }
                match Transcript::from_bytes(&bytes) {
                    Ok(tr) => report.checks.push(format!(
                        "transcript `{}` valid ({} entries, {} commands)",
                        tr_path.display(),
                        tr.entries.len(),
                        tr.commands.len()
                    )),
                    Err(e) => report.errors.push(format!(
                        "transcript `{}` parse error: {e}",
                        tr_path.display()
                    )),
                }
            }
        }
    }

    if let Some(dir) = snapshot_dir {
        if let Err(e) = fs::read_dir(dir) {
            report
                .errors
                .push(format!("cannot read snapshot dir `{}`: {e}", dir.display()));
        } else {
            let store = PackSnapshotStore::new(dir);
            if let Some(err) = store.corruption_error() {
                report.errors.push(err.to_string());
            }
            if store.truncated_tail_bytes() > 0 {
                report.warnings.push(format!(
                    "snapshots.onyxpack has {} truncated tail byte(s) from an interrupted write (earlier milestones intact)",
                    store.truncated_tail_bytes()
                ));
            }
            let entries = store.entries();
            if entries.is_empty() {
                report
                    .warnings
                    .push(format!("no snapshots found in `{}`", dir.display()));
            } else {
                let mut bad_snaps = 0usize;
                for meta in &entries {
                    match store.load_tick(meta.tick) {
                        Ok(bytes) => {
                            if !bytes.starts_with(b"ONYXAG") {
                                bad_snaps += 1;
                                report.errors.push(format!(
                                    "snapshot tick {} [{}] missing ONYXAG header",
                                    meta.tick, meta.tag
                                ));
                            } else {
                                let mut probe = AgentGraph::new();
                                if let Err(e) = probe.import(&bytes) {
                                    bad_snaps += 1;
                                    report.errors.push(format!(
                                        "snapshot tick {} [{}] corrupt AgentGraph payload: {e}",
                                        meta.tick, meta.tag
                                    ));
                                }
                            }
                        }
                        Err(e) => {
                            bad_snaps += 1;
                            report.errors.push(format!(
                                "snapshot tick {} [{}] load error: {e}",
                                meta.tick, meta.tag
                            ));
                        }
                    }
                }
                if bad_snaps == 0 {
                    report.checks.push(format!(
                        "all {} snapshots in `{}` carry ONYXAG headers and load cleanly",
                        entries.len(),
                        dir.display()
                    ));
                }
                if let Some(latest_meta) = entries.last() {
                    let latest = latest_meta.tick;
                    match build_host(1, AgentConfig::default(), Some(dir), None) {
                        Ok((mut host, ws_vocab)) => {
                            host.request_restore(&latest.to_string());
                            host.step(&mut HeadlessDriver::new());
                            let effects = effects_after_tick(host.graph(), &ws_vocab, 0);
                            let mut checked_shas = 0usize;
                            for eff in effects {
                                if let Some(sha) = eff.tree_sha {
                                    checked_shas += 1;
                                    if git_ok {
                                        let is_tree = Command::new("git")
                                            .args(["cat-file", "-t", &sha])
                                            .current_dir(workspace)
                                            .output()
                                            .map(|o| {
                                                o.status.success()
                                                    && String::from_utf8_lossy(&o.stdout).trim()
                                                        == "tree"
                                            })
                                            .unwrap_or(false);
                                        if !is_tree {
                                            report.errors.push(format!(
                                                "ledger tree_sha `{sha}` (tick {}) missing in git object database",
                                                eff.tick
                                            ));
                                        }
                                    }
                                }
                            }
                            report.checks.push(format!(
                                "verified {checked_shas} ledger tree checkpoint(s) at tick {latest}"
                            ));
                        }
                        Err(e) => report
                            .errors
                            .push(format!("failed to inspect snapshot ledger: {e}")),
                    }
                }
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_verifies_git_version_and_prunes_stale_worktrees() {
        let dir = std::env::temp_dir().join(format!(
            "openonyx_doctor_wt_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let wt_base = dir
            .parent()
            .unwrap()
            .join(format!("openonyx_doctor_wt_base_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&wt_base);
        fs::create_dir_all(&dir).unwrap();

        let run_git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@local")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@local")
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["init", "-q"]);
        fs::write(dir.join("README.md"), "hello\n").unwrap();
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "init"]);

        // Create a worktree and simulate an abrupt crash by deleting its folder directly.
        let mgr = WorktreeManager::with_base_dir_override(&dir, Some(&wt_base)).unwrap();
        let handle = mgr.create_worktree("stale-1", 1).unwrap();
        fs::remove_dir_all(&handle.worktree_path).unwrap();

        let report = run_doctor(&dir, None, None);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.contains(">= 2.38.0") && c.contains("merge-tree")),
            "expected git >= 2.38 check: {:?}",
            report.checks
        );
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.contains("git worktrees pruned (active onyx subagent worktrees: 0)")),
            "expected pruned worktree check: {:?}",
            report.checks
        );

        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&dir);
    }
}
