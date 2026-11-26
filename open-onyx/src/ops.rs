//! Operaciones offline de diagnóstico e inspección (`inspect`, `rewind`, `doctor`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use onyx_soul_agents::{
    AgentConfig, AgentGraph, Graph, GraphView, HeadlessDriver, Transcript, Value,
};
use onyx_workspace::{
    ExternalDriverConfig, McpRegistry, McpServerStatus, NetworkPolicy, ProcessJob, ProcessLimits,
    PromptTransport, RecordedEffect, SandboxConfig, SandboxDriverKind, SandboxMode,
    SubagentNetworkPolicy, WorkspacePolicy, WorktreeManager, all_recorded_effects,
    apply_sanitized_env, audit_sprout_workspace, builtin_external_driver_presets,
    check_git_version, clean_process_path, configure_process_isolation,
    detect_sandbox_capabilities, effects_after_tick, graph_workspace_mode, graph_workspace_plan,
    is_effect_undone, load_mcp_config, resolve_process_binary,
    restore_git_tree_checkpoint_for_targets,
};
use serde::Serialize;
use serde_json::json;

use crate::external_driver::kill_process_tree;
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

#[derive(Debug, Default)]
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
    // `SDD-04` TEST-04.4: auditoría de los servidores MCP declarados
    // (`[mcp.servers.*]`) respetando Workspace Trust, con timeout acotado a 5 s.
    let mcp_home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    audit_mcp_servers(
        &mut report,
        mcp_home.as_deref(),
        workspace,
        loaded_policy
            .as_ref()
            .is_some_and(|p| p.trust.config_trusted),
        32 * 1024,
    );

    if let Some(ref pol) = loaded_policy {
        for warn in &pol.trust_warnings {
            report.warnings.push(warn.clone());
        }
        // `SDD-10` Fase 4: auditoría de los harnesses CLI externos declarados
        // (`[[external_drivers]]` + presets): bin resuelto en PATH/PATHEXT (preset
        // faltante → `[WARN]`, driver declarado faltante → `[ERR]`), probe `--version`
        // aislada con deadline, transporte de prompt y política de red (`[CHECK]`
        // enforzada solo con `strict-container` activo).
        let presets = builtin_external_driver_presets();
        for d in pol.external_drivers() {
            audit_external_driver(&mut report, d, &active_sandbox, &presets);
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

/// Resultado de la probe `--version` de un binario de harness externo (`SDD-10` Fase 4).
enum DriverVersionProbe {
    /// El binario respondió a `--version` (primerá línea, truncada).
    Responded(String),
    /// No respondió dentro del deadline (proceso colgado o sin soporte `--version`).
    TimedOut,
    /// El spawn o la salida fallaron (motivo descriptivo).
    Failed(String),
}

/// Probe `--version` acotada en el tiempo del binario resuelto (`SDD-10` Fase 4):
/// diagnóstico local **aislado** — entorno saneado con `NetworkPolicy::Offline`
/// (`--version` no necesita salir a red) vía [`apply_sanitized_env`], ruta limpia vía
/// [`clean_process_path`], `configure_process_isolation` (grupo de proceso / ventana
/// oculta) y `ProcessJob::assign` **inmediato** tras `spawn` — y espera con deadline de
/// 3 s vía `try_wait`; al expirar mata el **árbol** completo con [`kill_process_tree`]
/// (Job Object / `kill -9 -pgid`, fallback `taskkill /F /T /PID`) y join de los hilos
/// lectores, acotados a 64 KiB cada uno. **0 `tokio`**, solo `std` + `onyx-workspace`.
fn probe_driver_version(path: &Path) -> DriverVersionProbe {
    use std::io::Read;
    use std::time::{Duration, Instant};

    const DEADLINE: Duration = Duration::from_secs(3);
    const READ_CAP: u64 = 64 * 1024;

    let started = Instant::now();
    let clean = clean_process_path(path);
    let workspace_root = clean
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut cmd = Command::new(&clean);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_isolation(&mut cmd, true);
    apply_sanitized_env(
        &mut cmd,
        &SandboxConfig {
            network: NetworkPolicy::Offline,
            ..SandboxConfig::default()
        },
        &workspace_root,
        &std::env::temp_dir(),
    );

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return DriverVersionProbe::Failed(format!("spawn failed: {e}")),
    };

    // Asignación inmediata al `ProcessJob`: sin intervalo sin cobertura entre `spawn`
    // y el bucle de espera (los nietos heredan el job / grupo de procesos).
    let mut job = ProcessJob::assign(&child, ProcessLimits::default());

    // Hilos lectores acotados: ni una salida enorme ni una pipe llena pueden bloquear
    // el bucle de espera ni agotar memoria.
    let out_thread = child.stdout.take().map(|out| {
        thread::spawn(move || {
            let mut buf = String::new();
            let _ = out.take(READ_CAP).read_to_string(&mut buf);
            buf
        })
    });
    let err_thread = child.stderr.take().map(|err| {
        thread::spawn(move || {
            let mut buf = String::new();
            let _ = err.take(READ_CAP).read_to_string(&mut buf);
            buf
        })
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= DEADLINE {
                    // El proceso se colgó (o ignoró `--version`): mata el árbol completo,
                    // cosecha y join de los lectores (las pipes mueron con el proceso).
                    kill_process_tree(&mut job, &mut child);
                    let _ = child.wait();
                    let _ = out_thread.and_then(|h| h.join().ok());
                    let _ = err_thread.and_then(|h| h.join().ok());
                    return DriverVersionProbe::TimedOut;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                kill_process_tree(&mut job, &mut child);
                let _ = out_thread.and_then(|h| h.join().ok());
                let _ = err_thread.and_then(|h| h.join().ok());
                return DriverVersionProbe::Failed(format!("wait failed: {e}"));
            }
        }
    };

    let out = out_thread.and_then(|h| h.join().ok()).unwrap_or_default();
    let err = err_thread.and_then(|h| h.join().ok()).unwrap_or_default();

    if !status.success() {
        let code = status
            .code()
            .map(|c| format!("code {c}"))
            .unwrap_or_else(|| "signal".to_string());
        let detail: String = err
            .trim()
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        return DriverVersionProbe::Failed(if detail.is_empty() {
            format!("exited with {code}")
        } else {
            format!("exited with {code} ({detail})")
        });
    }
    let first: String = out
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(80)
        .collect();
    DriverVersionProbe::Responded(if first.is_empty() {
        "ok".to_string()
    } else {
        first
    })
}

/// Auditoría de un driver de harness CLI externo declarado en `[[external_drivers]]`
/// (`SDD-10` Fase 4): bin resuelto en `PATH`/`PATHEXT`, probe `--version` aislada y
/// acotada, `prompt_transport = "arg"` en Windows y política de red.
///
/// Severidad según el origen del driver: un **preset integrado** (`presets.contains(d)`)
/// cuyo bin falte da `[WARN]` — las máquinas/CI sin ese CLI instalado no deben romper
/// `doctor` —; un **driver declarado** en `config.toml` cuyo bin falte da `[ERR]`
/// accionable. La `ExternalHarnessAllowlist` solo es `[CHECK]` (enforzada) con el modo de
/// sandbox activo `StrictContainer`; en el resto de modos es `[WARN]` advisory.
/// `SDD-04` TEST-04.4: auditoría de los servidores MCP declarados
/// (`[mcp.servers.*]`) respetando Workspace Trust.
///
/// El doctor hace un handshake **acotado a 5 s** por servidor (`min(cfg.timeout_secs, 5)`):
/// un servidor caído no puede colgar `doctor`. Mapeo de severidad:
/// - `Connected{n}` → `[CHECK]` informativo.
/// - `Disabled` → `[CHECK]` informativo.
/// - `Failed(msg)` → `[ERR]` accionable (el mensaje ya fue scrubbeado en
///   `discover_from_config`).
/// - workspace no confiado → las advertencias de `load_mcp_config` (que ya
///   mencionan `openonyx trust`) → `[WARN]`, sin descubrimiento de workspace.
fn audit_mcp_servers(
    report: &mut DoctorReport,
    global_root: Option<&Path>,
    workspace: &Path,
    config_trusted: bool,
    max_output_bytes: usize,
) {
    let (configs, warnings) = match load_mcp_config(global_root, Some(workspace), config_trusted) {
        Ok(v) => v,
        Err(e) => {
            report.errors.push(format!(
                "mcp config: failed to parse/merge `[mcp.servers.*]`: {e}"
            ));
            return;
        }
    };
    for w in warnings {
        report.warnings.push(w);
    }
    if configs.is_empty() {
        return;
    }
    // Timeout de doctor: nunca espera más de 5 s por servidor.
    let audit_configs: BTreeMap<String, _> = configs
        .into_iter()
        .map(|(name, mut cfg)| {
            cfg.timeout_secs = cfg.timeout_secs.min(5);
            (name, cfg)
        })
        .collect();
    let registry =
        McpRegistry::discover_from_config(&audit_configs, max_output_bytes, Some(workspace));
    for (name, status) in registry.server_statuses() {
        match status {
            McpServerStatus::Connected { tool_count } => {
                report.checks.push(format!(
                    "mcp server `{name}` connected ({tool_count} tool(s))"
                ));
            }
            McpServerStatus::Disabled => {
                report.checks.push(format!(
                    "mcp server `{name}` disabled in config (not loaded)"
                ));
            }
            McpServerStatus::Failed(msg) => {
                report
                    .errors
                    .push(format!("mcp server `{name}` failed handshake: {msg}"));
            }
            McpServerStatus::HttpPending => {
                report
                    .warnings
                    .push(format!("mcp server `{name}`: http transport pending"));
            }
        }
    }
}

fn audit_external_driver(
    report: &mut DoctorReport,
    d: &ExternalDriverConfig,
    active_sandbox: &SandboxConfig,
    presets: &[ExternalDriverConfig],
) {
    let label = format!("external driver `{}`", d.id);
    let Some(resolved) = resolve_process_binary(&d.bin) else {
        if presets.contains(d) {
            report.warnings.push(format!(
                "{label}: built-in preset binary `{}` not found in PATH/PATHEXT; install it to use `/spawn --driver {}` (doctor continues)",
                d.bin, d.id
            ));
        } else {
            report.errors.push(format!(
                "{label}: binary `{}` not found in PATH/PATHEXT; `/spawn --driver {}` will fail at runtime",
                d.bin, d.id
            ));
        }
        return;
    };
    match probe_driver_version(&resolved.path) {
        DriverVersionProbe::Responded(v) => report.checks.push(format!(
            "{label}: binary `{}` resolves to `{}` and responds to `--version` ({v})",
            d.bin,
            resolved.path.display()
        )),
        DriverVersionProbe::TimedOut => report.errors.push(format!(
            "{label}: binary `{}` (resolved `{}`) did not respond to `--version` within 3 s (hangs or lacks `--version` support)",
            d.bin,
            resolved.path.display()
        )),
        DriverVersionProbe::Failed(msg) => report.warnings.push(format!(
            "{label}: `--version` probe on `{}` failed: {msg}",
            resolved.path.display()
        )),
    }
    if d.prompt_transport == PromptTransport::Arg && cfg!(windows) {
        let why = if resolved.is_shell_wrapper {
            "it resolves to a `.cmd`/`.bat` shell wrapper"
        } else {
            "single-argv prompts are fragile on Windows"
        };
        report.warnings.push(format!(
            "{label}: `prompt_transport = \"arg\"` on Windows ({why}); prefer `stdin`"
        ));
    }
    match &d.network_policy {
        SubagentNetworkPolicy::NativeOffline => report.warnings.push(format!(
            "{label}: network policy is `NativeOffline`; the external CLI cannot reach its LLM API (declare `network_allowlist`)"
        )),
        SubagentNetworkPolicy::ExternalHarnessAllowlist(hosts) => {
            let hosts = if hosts.is_empty() {
                "(empty → LocalhostOnly)".to_string()
            } else {
                hosts.join(", ")
            };
            if active_sandbox.mode == SandboxMode::StrictContainer {
                report.checks.push(format!(
                    "{label}: network allowlist [{hosts}] is enforced (active sandbox mode is `strict-container`)"
                ));
            } else {
                report.warnings.push(format!(
                    "{label}: network allowlist [{hosts}] is advisory; only enforced with `--sandbox strict-container`"
                ));
            }
        }
    }
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
        // Sin warnings de *worktrees* residuales tras el prune (`SDD-05` TEST-05.4 (2)).
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("leftover onyx subagent worktree")),
            "no leftover worktree warning expected: {:?}",
            report.warnings
        );

        let _ = fs::remove_dir_all(&wt_base);
        let _ = fs::remove_dir_all(&dir);
    }

    /// La probe tiene un deadline fijo de 3 s (contrato de `doctor`, no ampliable aquí).
    /// Bajo carga extrema de CI (runner de 2 vCPU corriendo la suite completa en paralelo),
    /// el `spawn`+`exec` de un `sh` trivial puede excederlo por planificación del SO — no
    /// por el binario: los fakes de este test responden al instante. Se reintentan solo
    /// los artefactos de carga (`TimedOut` / `spawn failed`); un `Failed` real (código de
    /// salida + stderr) se devuelve sin reintentar. Sin carga, el primer intento acierta
    /// siempre (coste 0).
    fn probe_driver_version_resilient(path: &Path, attempts: u32) -> DriverVersionProbe {
        let mut probe = probe_driver_version(path);
        for _ in 1..attempts {
            let load_artifact = matches!(&probe, DriverVersionProbe::TimedOut)
                || matches!(&probe, DriverVersionProbe::Failed(m) if m.starts_with("spawn failed"));
            if !load_artifact {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            probe = probe_driver_version(path);
        }
        probe
    }

    /// `SDD-10` Fase 4: la probe `--version` de un binario de harness externo —
    /// respuesta → `Responded` (primera línea, truncada), fallo → `Failed` (code + stderr),
    /// colgado → `TimedOut` tras el deadline de 3 s.
    #[test]
    fn probe_driver_version_reports_responded_failed_and_timeout() {
        let tmp = std::env::temp_dir().join(format!(
            "openonyx_probe_drv_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        fn write_bin(tmp: &Path, name: &str, body_windows: &str, body_posix: &str) -> PathBuf {
            #[cfg(windows)]
            {
                let _ = body_posix;
                let p = tmp.join(format!("{name}.cmd"));
                fs::write(&p, body_windows).unwrap();
                p
            }
            #[cfg(not(windows))]
            {
                let _ = body_windows;
                let p = tmp.join(format!("{name}.sh"));
                fs::write(&p, body_posix).unwrap();
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
                p
            }
        }

        // Responde a `--version` (ignora el arg) → `Responded` con la primera línea.
        let ok_bin = write_bin(
            &tmp,
            "ok",
            "@echo off\r\necho opencode v9.9.9-fake\r\n",
            "#!/bin/sh\necho \"opencode v9.9.9-fake\"\n",
        );
        assert!(matches!(
            probe_driver_version_resilient(&ok_bin, 3),
            DriverVersionProbe::Responded(ref v) if v == "opencode v9.9.9-fake"
        ));

        // Sale con código ≠ 0 → `Failed` con code y primera línea de stderr (80 chars).
        let fail_bin = write_bin(
            &tmp,
            "fail",
            "@echo off\r\necho boom 1>&2\r\nexit /b 1\r\n",
            "#!/bin/sh\necho boom >&2\nexit 1\n",
        );
        assert!(matches!(
            probe_driver_version_resilient(&fail_bin, 3),
            DriverVersionProbe::Failed(ref m) if m.contains("code 1") && m.contains("boom")
        ));

        // Se cuelga (ignora `--version` > 3 s) → `TimedOut` y el proceso es matado.
        let hang_bin = write_bin(
            &tmp,
            "hang",
            "@echo off\r\nping -n 6 127.0.0.1 >nul\r\n",
            "#!/bin/sh\nsleep 5 &\nwait\n",
        );
        assert!(matches!(
            probe_driver_version(&hang_bin),
            DriverVersionProbe::TimedOut
        ));

        let _ = fs::remove_dir_all(&tmp);
    }

    /// `SDD-10` Fase 4: `openonyx doctor` audita cada driver de `[[external_drivers]]`:
    /// bin de **driver declarado** inexistente → `[ERR]` accionable; bin de **preset
    /// integrado** inexistente → `[WARN]` (no rompe doctor/CI); `--version` OK → `[CHECK]`;
    /// `prompt_transport = "arg"` en Windows → `[WARN]` (advisory); y la política de red →
    /// `[WARN]` advisory en modos distintos de `strict-container` y `[CHECK]` enforzada
    /// con el modo activo `StrictContainer`.
    #[test]
    fn doctor_audits_external_driver_bins() {
        let tmp = std::env::temp_dir().join(format!(
            "openonyx_doctor_drv_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        fn drv(id: &str, bin: &str, transport: PromptTransport) -> ExternalDriverConfig {
            ExternalDriverConfig {
                id: id.to_string(),
                bin: bin.to_string(),
                args: Vec::new(),
                prompt_transport: transport,
                prompt_flag: None,
                timeout: std::time::Duration::from_secs(600),
                allowed_env: Vec::new(),
                inherit_home: false,
                network_policy: SubagentNetworkPolicy::ExternalHarnessAllowlist(vec![
                    "api.example.com".to_string(),
                ]),
            }
        }

        // 1) Bin inexistente en **driver declarado** (no es preset) → `[ERR]` accionable
        //    que menciona `/spawn --driver <id>`.
        let mut report = DoctorReport::default();
        audit_external_driver(
            &mut report,
            &drv(
                "ghost",
                "definitely-missing-onyx-bin",
                PromptTransport::Stdin,
            ),
            &SandboxConfig::default(),
            &[],
        );
        assert!(
            report.errors.iter().any(|e| {
                e.contains("not found in PATH/PATHEXT") && e.contains("/spawn --driver ghost")
            }),
            "missing bin must be [ERR]: {:?}",
            report.errors
        );
        assert!(
            report.checks.is_empty() && report.warnings.is_empty(),
            "missing bin must not add checks/warnings: {report:?}"
        );

        // Bin fake que responde a `--version` (la probe lo usa; los args se ignoran).
        #[cfg(windows)]
        let ok_bin = tmp.join("ok.cmd");
        #[cfg(not(windows))]
        let ok_bin = tmp.join("ok.sh");
        #[cfg(windows)]
        fs::write(&ok_bin, "@echo off\r\necho opencode v9.9.9-fake\r\n").unwrap();
        #[cfg(not(windows))]
        {
            fs::write(&ok_bin, "#!/bin/sh\necho \"opencode v9.9.9-fake\"\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&ok_bin, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ok_bin = ok_bin.to_string_lossy().into_owned();

        // 2) Bin resuelto y con `--version` → `[CHECK]` con la versión; la red → `[WARN]`
        //    advisory que distingue `--sandbox strict-container`.
        //    La probe interna (deadline fijo de 3 s) puede expirar bajo carga extrema de CI
        //    por planificación del SO y el audit emite `[ERR]` — artefacto de carga, no del
        //    bin fake, que responde al instante → reintento acotado del audit antes de asertar.
        let mut report = DoctorReport::default();
        for _ in 0..3 {
            report = DoctorReport::default();
            audit_external_driver(
                &mut report,
                &drv("ok", &ok_bin, PromptTransport::Stdin),
                &SandboxConfig::default(),
                &[],
            );
            if report.errors.is_empty()
                && report.checks.iter().any(|c| {
                    c.contains("responds to `--version`") && c.contains("opencode v9.9.9-fake")
                })
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert!(
            report.checks.iter().any(
                |c| c.contains("responds to `--version`") && c.contains("opencode v9.9.9-fake")
            ),
            "responsive bin must be [CHECK] with version: {:?}",
            report.checks
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("advisory") && w.contains("--sandbox strict-container")),
            "network allowlist must be [WARN] advisory: {:?}",
            report.warnings
        );

        // 3) `prompt_transport = "arg"` en Windows → `[WARN]` (fragilidad argv); en el
        //    resto de plataformas no se emite (el transporte es seguro ahí).
        let mut report = DoctorReport::default();
        audit_external_driver(
            &mut report,
            &drv("arg", &ok_bin, PromptTransport::Arg),
            &SandboxConfig::default(),
            &[],
        );
        if cfg!(windows) {
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|w| w.contains("prompt_transport = \"arg\"")
                        && w.contains("prefer `stdin`")),
                "arg transport on Windows must be [WARN]: {:?}",
                report.warnings
            );
        } else {
            assert!(
                !report
                    .warnings
                    .iter()
                    .any(|w| w.contains("prompt_transport")),
                "arg transport is safe off Windows: {:?}",
                report.warnings
            );
        }

        // 4) `NativeOffline` → `[WARN]` (el CLI no podría llegar a su API de LLM).
        let offline = ExternalDriverConfig {
            network_policy: SubagentNetworkPolicy::NativeOffline,
            ..drv("off", &ok_bin, PromptTransport::Stdin)
        };
        let mut report = DoctorReport::default();
        audit_external_driver(&mut report, &offline, &SandboxConfig::default(), &[]);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("NativeOffline") && w.contains("cannot reach its LLM API")),
            "NativeOffline must be [WARN]: {:?}",
            report.warnings
        );

        // 5) Bin inexistente en **preset integrado** (`presets.contains(d)`) → `[WARN]`,
        //    nunca `[ERR]`: una máquina/CI sin el CLI del preset no rompe `doctor`.
        let ghost_preset = drv(
            "ghost-preset",
            "definitely-missing-onyx-bin",
            PromptTransport::Stdin,
        );
        let mut report = DoctorReport::default();
        audit_external_driver(
            &mut report,
            &ghost_preset,
            &SandboxConfig::default(),
            std::slice::from_ref(&ghost_preset),
        );
        assert!(
            !report
                .errors
                .iter()
                .any(|e| e.contains("/spawn --driver ghost-preset")),
            "missing built-in preset must not be [ERR]: {:?}",
            report.errors
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("built-in preset") && w.contains("not found in PATH/PATHEXT")),
            "missing built-in preset must be [WARN]: {:?}",
            report.warnings
        );

        // 6) Allowlist + modo activo `StrictContainer` → `[CHECK]` enforzada (no `[WARN]`
        //    advisory); el resto de modos ya queda cubierto en el caso 2.
        let strict = SandboxConfig {
            mode: SandboxMode::StrictContainer,
            ..SandboxConfig::default()
        };
        let mut report = DoctorReport::default();
        audit_external_driver(
            &mut report,
            &drv("strict", &ok_bin, PromptTransport::Stdin),
            &strict,
            &[],
        );
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.contains("network allowlist") && c.contains("enforced")),
            "allowlist under strict-container must be [CHECK] enforced: {:?}",
            report.checks
        );
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("network allowlist")),
            "enforced allowlist must not also be [WARN] advisory: {:?}",
            report.warnings
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// Mock HTTP MCP mínimo (JSON directo) para el doctor: `initialize` (con
    /// `Mcp-Session-Id`), `notifications/initialized` (202) y `tools/list`
    /// (1 tool). Mismo patrón hermetic de `tests/mcp_http.rs`.
    fn spawn_mcp_doctor_mock(listener: std::net::TcpListener) {
        thread::spawn(move || {
            for stream in listener.incoming().filter_map(Result::ok) {
                let mut stream = stream;
                let _ = doctor_mock_conn(&mut stream);
            }
        });
    }

    fn doctor_mock_conn(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        use std::io::{BufRead, BufReader, Read, Write};
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut head = String::new();
        let mut line = String::new();
        while !head.contains("\r\n\r\n") {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            head.push_str(&line);
        }
        let content_length = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("Content-Length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = req.get("id").cloned();
        let (status, payload, with_session) = match method {
            "initialize" => (
                200,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": { "name": "onyx-doctor-mock", "version": "0.1" }
                    }
                })
                .to_string(),
                true,
            ),
            "notifications/initialized" => (202, String::new(), false),
            "tools/list" => (
                200,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "tools": [
                        { "name": "echo", "description": "echo de texto",
                          "inputSchema": { "type": "object", "properties": {} } }
                    ] }
                })
                .to_string(),
                true,
            ),
            _ => (404, String::new(), true),
        };
        let reason = if status == 202 { "Accepted" } else { "OK" };
        let mut out = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n");
        if with_session {
            out.push_str("Mcp-Session-Id: onyx-doctor-session\r\n");
        }
        out.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        ));
        out.push_str(&payload);
        stream.write_all(out.as_bytes())?;
        stream.flush()
    }

    /// `SDD-04` TEST-04.4: `openonyx doctor` audita `[mcp.servers.*]` respetando
    /// Workspace Trust y con timeout acotado a 5 s: HTTP loopback sano → `[CHECK]`
    /// conectado; puerto muerto → `[ERR]` accionable; `enabled = false` → `[CHECK]`
    /// deshabilitado; workspace no confiado → `[WARN]` con `openonyx trust` y sin
    /// descubrimiento de servidores.
    #[test]
    fn doctor_audits_mcp_servers() {
        let tmp = std::env::temp_dir().join(format!(
            "openonyx_doctor_mcp_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        let workspace = tmp.join("ws");
        let global = tmp.join("global");
        fs::create_dir_all(workspace.join(".onyx")).unwrap();
        fs::create_dir_all(&global).unwrap();

        // Mock HTTP MCP sano en loopback (1 tool).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("local_addr").port();
        spawn_mcp_doctor_mock(listener);

        // Puerto loopback muerto (fallo rápido y hermetic).
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
            let p = l.local_addr().expect("local_addr").port();
            drop(l);
            p
        };

        fs::write(
            workspace.join(".onyx").join("config.toml"),
            format!(
                r#"
[mcp.servers.ok]
transport = "http"
url = "http://127.0.0.1:{port}/mcp"
timeout_secs = 99

[mcp.servers.dead]
transport = "http"
url = "http://127.0.0.1:{dead}/mcp"
timeout_secs = 99

[mcp.servers.off]
transport = "http"
url = "http://127.0.0.1:1/mcp"
enabled = false
"#
            ),
        )
        .unwrap();

        // 1) Confiable: los tres estados se reportan y el timeout 99 s se acota a 5 s
        //    (el test no puede colgar por eso).
        let mut report = DoctorReport::default();
        audit_mcp_servers(&mut report, Some(&global), &workspace, true, 32 * 1024);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.contains("mcp server `ok`") && c.contains("connected (1 tool(s))")),
            "ok server must be [CHECK] connected: {:?}",
            report.checks
        );
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.contains("mcp server `off`") && c.contains("disabled")),
            "off server must be [CHECK] disabled: {:?}",
            report.checks
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("mcp server `dead`") && e.contains("failed handshake")),
            "dead server must be [ERR] accionable: {:?}",
            report.errors
        );
        assert!(report.warnings.is_empty(), "{report:?}");

        // 2) No confiado: la config de workspace se ignora con `[WARN]` accionable
        //    (sin descubrimiento de servidores del workspace).
        let mut report = DoctorReport::default();
        audit_mcp_servers(&mut report, Some(&global), &workspace, false, 32 * 1024);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("not trusted") && w.contains("openonyx trust")),
            "untrusted must warn with `openonyx trust`: {report:?}"
        );
        assert!(
            report.checks.iter().all(|c| !c.contains("mcp server")),
            "untrusted must not discover workspace servers: {report:?}"
        );
        assert!(report.errors.is_empty(), "{report:?}");

        let _ = fs::remove_dir_all(&tmp);
    }
}
