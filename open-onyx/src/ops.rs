//! Operaciones offline de diagnóstico e inspección (`inspect`, `rewind`, `doctor`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSnapshotStore, GraphView, HeadlessDriver, SnapshotStore,
    Transcript,
};
use onyx_workspace::{effects_after_tick, restore_git_tree_checkpoint};

use crate::session::build_host;

pub fn run_inspect(
    dir: &Path,
    tick: Option<u64>,
    config: AgentConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = AgentSnapshotStore::new(dir);
    let target_tick = match tick {
        Some(t) => t,
        None => store
            .latest_tick()
            .ok_or("no snapshots found in directory")?,
    };
    let mut host = AgentHost::builder(1)
        .config(config)
        .store(Box::new(store))
        .build()?;
    host.request_restore(&target_tick.to_string());
    host.step(&mut HeadlessDriver::new());

    println!("=== openOnyx Graph Snapshot @ tick {target_tick} ===");
    let concepts = host.graph().query(&[]);
    println!("Total concepts: {}", concepts.len());
    for id in host.agents() {
        let view = host.view(id);
        println!(
            "\nAgent {} (`{}`) state={:?} req={}",
            id.0,
            view.name,
            host.agent_state(id),
            view.request
        );
        println!("  goal: {}", view.goal);
        println!("  tools: {:?}", view.tools);
        println!("  memory ({} entries):", view.memory.len());
        for (k, body) in &view.memory {
            let preview: String = body.chars().take(120).collect();
            println!("    - [{k}] {preview}");
        }
        if let Some(p) = &view.pending_approval {
            println!("  pending_approval: {} {}", p.tool, p.args);
        }
        if let Some(res) = host.result(id) {
            println!("  result: {res}");
        }
    }
    Ok(())
}

pub fn reconcile_rewind_effects(
    workspace: &Path,
    dir: &Path,
    tick: u64,
    config: &AgentConfig,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let dir_buf = dir.to_path_buf();
    let store = AgentSnapshotStore::new(dir);
    let Some(latest) = store.latest_tick() else {
        return Ok(());
    };
    if latest <= tick {
        return Ok(());
    }

    let (mut probe_host, probe_ws_vocab) = build_host(1, config.clone(), Some(&dir_buf), None)?;
    probe_host.request_restore(&latest.to_string());
    probe_host.step(&mut HeadlessDriver::new());
    let orphaned = effects_after_tick(probe_host.graph(), &probe_ws_vocab, tick);
    if orphaned.is_empty() {
        return Ok(());
    }

    println!(
        "=== [openOnyx rewind] Side-Effect Ledger ({} effects after tick {tick}) ===",
        orphaned.len()
    );
    for eff in &orphaned {
        let ckpt_info = eff
            .tree_sha
            .as_deref()
            .map(|sha| format!(" [tree:{sha}]"))
            .unwrap_or_default();
        println!(
            "  - tick={} agent={} [{}] {} -> {} ({}){}",
            eff.tick, eff.agent, eff.reversibility, eff.tool, eff.target, eff.summary, ckpt_info
        );
    }
    if let Some(tree_sha) = orphaned.iter().find_map(|e| e.tree_sha.as_deref()) {
        if dry_run {
            println!(
                "[openOnyx rewind --dry-run] would restore workspace working tree to checkpoint {tree_sha}"
            );
        } else {
            restore_git_tree_checkpoint(workspace, tree_sha)?;
            println!("[openOnyx rewind] restored workspace working tree to checkpoint {tree_sha}");
        }
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
    } else {
        report.warnings.push(format!(
            "workspace `{}` is not a git worktree",
            workspace.display()
        ));
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
                "temporary artifacts in workspace root: {}",
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
        let store = AgentSnapshotStore::new(dir);
        match fs::read_dir(dir) {
            Err(e) => report
                .errors
                .push(format!("cannot read snapshot dir `{}`: {e}", dir.display())),
            Ok(entries) => {
                let mut ticks = Vec::new();
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if let Some(stem) = name
                        .strip_prefix("tick-")
                        .and_then(|s| s.strip_suffix(".bin"))
                    {
                        if let Ok(t) = stem.parse::<u64>() {
                            ticks.push(t);
                        }
                    }
                }
                ticks.sort();
                if ticks.is_empty() {
                    report.warnings.push(format!(
                        "no tick-*.bin snapshots found in `{}`",
                        dir.display()
                    ));
                } else {
                    let mut bad_snaps = 0usize;
                    for &t in &ticks {
                        let key = t.to_string();
                        match store.load(&key) {
                            Ok(bytes) if bytes.starts_with(b"ONYXAG") => {}
                            Ok(_) => {
                                bad_snaps += 1;
                                report
                                    .errors
                                    .push(format!("snapshot tick-{t}.bin missing ONYXAG header"));
                            }
                            Err(e) => {
                                bad_snaps += 1;
                                report
                                    .errors
                                    .push(format!("snapshot tick-{t}.bin load error: {e}"));
                            }
                        }
                    }
                    if bad_snaps == 0 {
                        report.checks.push(format!(
                            "all {} snapshots in `{}` carry ONYXAG headers and load cleanly",
                            ticks.len(),
                            dir.display()
                        ));
                    }
                    if let Some(&latest) = ticks.last() {
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
    }

    report
}
