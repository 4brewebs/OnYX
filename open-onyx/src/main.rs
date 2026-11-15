mod gemini;
mod tui;

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gemini::{AsyncGeminiResponder, DEFAULT_GEMINI_MODEL, GeminiResponder};
use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSnapshotStore, AgentSpec, AgentState, AttrRegistry, Ctx, EventBus,
    GraphView, HeadlessDriver, IngressEvent, OnyxError, OnyxEvent, OnyxSystem, PlatformDriver,
    RecordingBridge, ReplayBridge, Reply, SurfaceBinding, Transcript, Value, Vocab,
    pending_approvals, spawn_event,
};
use onyx_workspace::{
    PendingCheckpoints, WorkspaceLedgerSystem, WorkspaceMode, WorkspacePolicy, WorkspaceToolBridge,
    WorkspaceTools, WorkspaceVocab, effects_after_tick, restore_git_tree_checkpoint,
};
use tui::TuiDriver;

struct CliArgs {
    mode: Mode,
    workspace: PathBuf,
    auto_approve: bool,
    dry_run: bool,
    save_transcript: Option<PathBuf>,
    snapshot_dir: Option<PathBuf>,
}

enum Mode {
    Run(String),
    Chat,
    Replay(PathBuf),
    Inspect {
        dir: PathBuf,
        tick: Option<u64>,
    },
    Rewind {
        dir: PathBuf,
        tick: u64,
        transcript: Option<PathBuf>,
    },
}

/// Sistema determinista que convierte `IngressEvent::Command(goal)` de la TUI en
/// un `spawn_event` dentro del bucle `tick()`.
struct CommandSpawnerSystem {
    vocab: Option<Vocab>,
    auto_approve: bool,
    counter: usize,
}

impl CommandSpawnerSystem {
    fn new(auto_approve: bool) -> Self {
        Self {
            vocab: None,
            auto_approve,
            counter: 0,
        }
    }
}

impl OnyxSystem for CommandSpawnerSystem {
    fn name(&self) -> &str {
        "openonyx_command_spawner"
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.vocab = Some(Vocab::resolve(attrs));
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let Some(v) = self.vocab else { return };
        for event in ctx.inbox {
            if let OnyxEvent::Ingress(IngressEvent::Command(goal)) = event {
                self.counter += 1;
                let mut spec = AgentSpec::new(format!("openonyx-{}", self.counter), goal.clone())
                    .with_tools(WorkspaceTools::all_tool_names().iter().copied());
                if !self.auto_approve {
                    spec = spec.with_approval_tools(
                        WorkspaceTools::approval_required_names().iter().copied(),
                    );
                }
                ctx.outbox.push(spawn_event(&v, &spec, None));
            }
        }
    }
}

/// Driver CLI para aprobaciones en modo `run`: se envuelve dentro de
/// `RecordingBridge` para que `Approve`/`Deny` queden grabados en `Transcript`.
struct CliApprovalDriver {
    vocab: Vocab,
    inner: Box<dyn PlatformDriver>,
    seen: BTreeSet<(u64, u64)>,
    queued: Vec<Reply>,
}

impl CliApprovalDriver {
    fn new(vocab: Vocab, inner: impl PlatformDriver + 'static) -> Self {
        Self {
            vocab,
            inner: Box::new(inner),
            seen: BTreeSet::new(),
            queued: Vec::new(),
        }
    }
}

impl PlatformDriver for CliApprovalDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        self.inner.poll(bus);
        for reply in self.queued.drain(..) {
            bus.push(reply.to_event());
        }
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        self.inner.present(graph, surfaces);
        for view in pending_approvals(graph, &self.vocab) {
            if !self.seen.insert((view.agent.0, view.request)) {
                continue;
            }
            if let Some(pending) = &view.pending_approval {
                let reply = if prompt_approval(&pending.tool, &pending.args) {
                    Reply::approve(view.agent, view.request)
                } else {
                    Reply::deny(view.agent, view.request, "denied by user")
                };
                self.queued.push(reply);
            }
        }
    }
}

fn parse_args() -> Result<CliArgs, String> {
    let mut args = env::args().skip(1);
    let Some(cmd) = args.next() else {
        return Err(usage());
    };
    let mut workspace = env::current_dir().map_err(|e| e.to_string())?;
    let mut auto_approve = false;
    let mut dry_run = false;
    let mut save_transcript = None;
    let mut snapshot_dir = None;
    let mut tick_opt: Option<u64> = None;
    let mut transcript_opt: Option<PathBuf> = None;
    let mut positional = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" | "-w" => {
                let p = args.next().ok_or("--workspace requires a path")?;
                workspace = PathBuf::from(p);
            }
            "--auto-approve" => auto_approve = true,
            "--dry-run" => dry_run = true,
            "--save-transcript" => {
                let p = args.next().ok_or("--save-transcript requires a path")?;
                save_transcript = Some(PathBuf::from(p));
            }
            "--snapshot-dir" => {
                let p = args.next().ok_or("--snapshot-dir requires a path")?;
                snapshot_dir = Some(PathBuf::from(p));
            }
            "--tick" => {
                let t = args.next().ok_or("--tick requires a number")?;
                tick_opt = Some(t.parse::<u64>().map_err(|_| "--tick must be a u64")?);
            }
            "--transcript" => {
                let p = args.next().ok_or("--transcript requires a path")?;
                transcript_opt = Some(PathBuf::from(p));
            }
            "--help" | "-h" => return Err(usage()),
            _ => positional.push(arg),
        }
    }

    let mode = match cmd.as_str() {
        "run" => {
            if positional.is_empty() {
                return Err("run requires a <goal>".to_string());
            }
            Mode::Run(positional.join(" "))
        }
        "chat" => Mode::Chat,
        "replay" => {
            let p = positional
                .into_iter()
                .next()
                .ok_or("replay requires <transcript>")?;
            Mode::Replay(PathBuf::from(p))
        }
        "inspect" => {
            let dir = positional
                .into_iter()
                .next()
                .ok_or("inspect requires <snapshot_dir>")?;
            Mode::Inspect {
                dir: PathBuf::from(dir),
                tick: tick_opt,
            }
        }
        "rewind" => {
            let mut it = positional.into_iter();
            let dir = it.next().ok_or("rewind requires <snapshot_dir> <tick>")?;
            let tick_str = it.next().ok_or("rewind requires <tick>")?;
            let tick = tick_str
                .parse::<u64>()
                .map_err(|_| "rewind <tick> must be a u64")?;
            Mode::Rewind {
                dir: PathBuf::from(dir),
                tick,
                transcript: transcript_opt,
            }
        }
        _ => return Err(usage()),
    };

    Ok(CliArgs {
        mode,
        workspace,
        auto_approve,
        dry_run,
        save_transcript,
        snapshot_dir,
    })
}

fn usage() -> String {
    "Usage:\n  openonyx chat [--workspace <dir>] [--auto-approve] [--save-transcript <file>] [--snapshot-dir <dir>]\n  openonyx run <goal> [--workspace <dir>] [--auto-approve] [--save-transcript <file>] [--snapshot-dir <dir>]\n  openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>]\n  openonyx inspect <snapshot_dir> [--tick <n>]\n  openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--workspace <dir>] [--auto-approve] [--dry-run]".to_string()
}

fn prompt_approval(tool: &str, args: &str) -> bool {
    println!("\n[openOnyx] Approval required for `{tool}`:");
    println!("  args: {args}");
    print!("Approve? [y/N]: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn all_live_waiting_approval(graph: &dyn GraphView, v: &Vocab) -> bool {
    let mut any_live = false;
    for id in graph.query(&[v.state]) {
        if let Some(Value::Text(s)) = graph.get(id, v.state) {
            if let Some(state) = AgentState::parse(&s) {
                if !state.is_terminal() {
                    any_live = true;
                    if state != AgentState::WaitingApproval {
                        return false;
                    }
                }
            }
        }
    }
    any_live
}

fn build_host(
    seed: u64,
    config: AgentConfig,
    snapshot_dir: Option<&PathBuf>,
    extra_system: Option<Box<dyn OnyxSystem>>,
) -> Result<(AgentHost, WorkspaceVocab), Box<dyn std::error::Error>> {
    build_host_with_checkpoints(seed, config, snapshot_dir, extra_system, None)
}

fn build_host_with_checkpoints(
    seed: u64,
    config: AgentConfig,
    snapshot_dir: Option<&PathBuf>,
    extra_system: Option<Box<dyn OnyxSystem>>,
    checkpoints: Option<PendingCheckpoints>,
) -> Result<(AgentHost, WorkspaceVocab), Box<dyn std::error::Error>> {
    let mut ledger = WorkspaceLedgerSystem::new();
    if let Some(cp) = checkpoints {
        ledger = ledger.with_checkpoints(cp);
    }
    let mut builder = AgentHost::builder(seed)
        .config(config)
        .system(Box::new(ledger));
    if let Some(sys) = extra_system {
        builder = builder.system(sys);
    }
    if let Some(dir) = snapshot_dir {
        builder = builder.store(Box::new(AgentSnapshotStore::new(dir)));
    }
    let mut host = builder.build()?;
    let ws_vocab = WorkspaceVocab::resolve_with(|name| host.attr(name));
    Ok((host, ws_vocab))
}

fn make_spec(goal: &str, auto_approve: bool) -> AgentSpec {
    let mut spec = AgentSpec::new("openonyx", goal)
        .with_tools(WorkspaceTools::all_tool_names().iter().copied());
    if !auto_approve {
        spec = spec.with_approval_tools(WorkspaceTools::approval_required_names().iter().copied());
    }
    spec
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    let policy = WorkspacePolicy::new(&cli.workspace)?;
    let tools = WorkspaceTools::new(policy);

    let config = AgentConfig {
        default_max_steps: 32,
        default_timeout_ticks: 64,
        ..AgentConfig::default()
    };

    let save_snapshots = cli.snapshot_dir.is_some();

    match cli.mode {
        Mode::Run(goal) => {
            let checkpoints = PendingCheckpoints::default();
            let (mut host, ws_vocab) = build_host_with_checkpoints(
                1,
                config,
                cli.snapshot_dir.as_ref(),
                None,
                Some(checkpoints.clone()),
            )?;
            let api_key = env::var("GEMINI_API_KEY")
                .map_err(|_| "GEMINI_API_KEY environment variable is not set")?;
            let model =
                env::var("ONYX_GEMINI_MODEL").unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.to_string());

            host.spawn(&make_spec(&goal, cli.auto_approve));

            let cli_driver = CliApprovalDriver::new(host.vocab(), HeadlessDriver::new());
            let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
                .with_mode(WorkspaceMode::Build)
                .with_workspace_vocab(ws_vocab)
                .with_checkpoints(checkpoints)
                .require_read_before_edit(true)
                .wrapping(cli_driver);
            let mut driver =
                RecordingBridge::new(host.vocab(), GeminiResponder::new(api_key, model))
                    .wrapping(ws_bridge);

            for _ in 0..256 {
                if save_snapshots {
                    host.request_snapshot(&host.ticks().to_string());
                }
                host.step(&mut driver);
                if host.is_settled() {
                    break;
                }
            }

            if let Some(path) = cli.save_transcript {
                fs::write(path, driver.transcript().to_bytes())?;
            }
            if let Some(id) = host.agents().first().copied() {
                println!(
                    "\n[openOnyx] state={:?} result:\n{}",
                    host.agent_state(id),
                    host.result(id).unwrap_or_default()
                );
            }
        }
        Mode::Chat => {
            let checkpoints = PendingCheckpoints::default();
            let (mut host, ws_vocab) = build_host_with_checkpoints(
                1,
                config,
                cli.snapshot_dir.as_ref(),
                Some(Box::new(CommandSpawnerSystem::new(cli.auto_approve))),
                Some(checkpoints.clone()),
            )?;
            let api_key = env::var("GEMINI_API_KEY")
                .map_err(|_| "GEMINI_API_KEY environment variable is not set")?;
            let model =
                env::var("ONYX_GEMINI_MODEL").unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.to_string());

            let tui = TuiDriver::new(host.vocab())?;
            let tui_handle = tui.handle();
            let quit = tui.quit_flag();
            let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
                .with_mode(WorkspaceMode::Build)
                .with_workspace_vocab(ws_vocab)
                .with_checkpoints(checkpoints)
                .require_read_before_edit(true)
                .wrapping(tui);
            let llm_in_flight = Arc::new(AtomicUsize::new(0));
            let mut driver = RecordingBridge::new(
                host.vocab(),
                AsyncGeminiResponder::new(api_key, model, Arc::clone(&llm_in_flight)),
            )
            .wrapping(ws_bridge);

            tui_handle.render(host.graph());
            let mut needs_follow_up_tick = false;
            while !quit.load(Ordering::Relaxed) {
                let queued = tui_handle.has_queued_events();
                let waiting_human = all_live_waiting_approval(host.graph(), &host.vocab());
                let waiting_llm = llm_in_flight.load(Ordering::Acquire) > 0;
                let can_advance = !host.is_settled() && !waiting_human && !waiting_llm;
                if can_advance || queued || needs_follow_up_tick {
                    let was_settled = host.is_settled();
                    if save_snapshots {
                        host.request_snapshot(&host.ticks().to_string());
                    }
                    host.step(&mut driver);
                    // Un `IngressEvent::Command` tarda 1 tick en convertirse en
                    // `spawn_event` (por `CommandSpawnerSystem`) y otro en crear
                    // el agente en el grafo (por `AgentSpawner`).
                    needs_follow_up_tick = queued;
                    if !was_settled && host.is_settled() {
                        if let Some(path) = &cli.save_transcript {
                            let _ = fs::write(path, driver.transcript().to_bytes());
                        }
                    }
                } else {
                    tui_handle.pump_idle(host.graph());
                }
            }
            if let Some(path) = cli.save_transcript {
                fs::write(path, driver.transcript().to_bytes())?;
            }
        }
        Mode::Replay(path) => {
            let bytes = fs::read(path)?;
            let transcript = Transcript::from_bytes(&bytes).map_err(|e| e.to_string())?;
            let has_commands = !transcript.commands.is_empty();

            let (mut host, _ws_vocab) = build_host(
                1,
                config,
                cli.snapshot_dir.as_ref(),
                Some(Box::new(CommandSpawnerSystem::new(cli.auto_approve))),
            )?;
            if !has_commands {
                host.spawn(&make_spec("replay", cli.auto_approve));
            }

            // Replay determinista puro: NO toca disco ni llama a Gemini.
            let mut driver = ReplayBridge::new(transcript).wrapping(HeadlessDriver::new());
            for _ in 0..4096 {
                if save_snapshots {
                    host.request_snapshot(&host.ticks().to_string());
                }
                host.step(&mut driver);
                if driver.remaining() == 0
                    && (host.is_settled() || all_live_waiting_approval(host.graph(), &host.vocab()))
                {
                    break;
                }
            }

            for id in host.agents() {
                println!(
                    "\n[openOnyx replay] agent={} state={:?} result:\n{}",
                    id.0,
                    host.agent_state(id),
                    host.result(id).unwrap_or_default()
                );
            }
        }
        Mode::Inspect { dir, tick } => {
            let store = AgentSnapshotStore::new(&dir);
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
        }
        Mode::Rewind {
            dir,
            tick,
            transcript,
        } => {
            let store = AgentSnapshotStore::new(&dir);
            if let Some(latest) = store.latest_tick() {
                if latest > tick {
                    let (mut probe_host, probe_ws_vocab) =
                        build_host(1, config.clone(), Some(&dir), None)?;
                    probe_host.request_restore(&latest.to_string());
                    probe_host.step(&mut HeadlessDriver::new());
                    let orphaned = effects_after_tick(probe_host.graph(), &probe_ws_vocab, tick);
                    if !orphaned.is_empty() {
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
                                eff.tick,
                                eff.agent,
                                eff.reversibility,
                                eff.tool,
                                eff.target,
                                eff.summary,
                                ckpt_info
                            );
                        }
                        if let Some(tree_sha) = orphaned.iter().find_map(|e| e.tree_sha.as_deref())
                        {
                            if cli.dry_run {
                                println!(
                                    "[openOnyx rewind --dry-run] would restore workspace working tree to checkpoint {tree_sha}"
                                );
                            } else {
                                restore_git_tree_checkpoint(&cli.workspace, tree_sha)?;
                                println!(
                                    "[openOnyx rewind] restored workspace working tree to checkpoint {tree_sha}"
                                );
                            }
                        }
                    }
                }
            }
            if cli.dry_run {
                return Ok(());
            }

            let checkpoints = PendingCheckpoints::default();
            let (mut host, ws_vocab) = build_host_with_checkpoints(
                1,
                config,
                Some(&dir),
                Some(Box::new(CommandSpawnerSystem::new(cli.auto_approve))),
                Some(checkpoints.clone()),
            )?;
            host.request_restore(&tick.to_string());
            host.step(&mut HeadlessDriver::new());

            let next_tick = tick.saturating_add(1);
            host.set_tick(next_tick);

            if let Some(tr_path) = transcript {
                let bytes = fs::read(tr_path)?;
                let tr = Transcript::from_bytes(&bytes).map_err(|e| e.to_string())?;
                let mut driver = ReplayBridge::new(tr)
                    .starting_at(next_tick)
                    .wrapping(HeadlessDriver::new());
                for _ in 0..4096 {
                    host.step(&mut driver);
                    if driver.remaining() == 0
                        && (host.is_settled()
                            || all_live_waiting_approval(host.graph(), &host.vocab()))
                    {
                        break;
                    }
                }
            } else {
                let api_key = env::var("GEMINI_API_KEY")
                    .map_err(|_| "GEMINI_API_KEY environment variable is not set")?;
                let model = env::var("ONYX_GEMINI_MODEL")
                    .unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.to_string());
                let cli_driver = CliApprovalDriver::new(host.vocab(), HeadlessDriver::new());
                let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
                    .with_mode(WorkspaceMode::Build)
                    .with_workspace_vocab(ws_vocab)
                    .with_checkpoints(checkpoints)
                    .require_read_before_edit(true)
                    .wrapping(cli_driver);
                let mut driver =
                    RecordingBridge::new(host.vocab(), GeminiResponder::new(api_key, model))
                        .starting_at(next_tick)
                        .wrapping(ws_bridge);

                for _ in 0..256 {
                    host.step(&mut driver);
                    if host.is_settled() {
                        break;
                    }
                }
            }

            for id in host.agents() {
                println!(
                    "\n[openOnyx rewind@{tick}] agent={} state={:?} result:\n{}",
                    id.0,
                    host.agent_state(id),
                    host.result(id).unwrap_or_default()
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_spawner_spawns_agent_on_follow_up_tick() {
        let mut host = AgentHost::builder(1)
            .config(AgentConfig {
                default_max_steps: 32,
                default_timeout_ticks: 64,
                ..AgentConfig::default()
            })
            .system(Box::new(CommandSpawnerSystem::new(false)))
            .build()
            .unwrap();

        host.push(OnyxEvent::Ingress(IngressEvent::Command("hola".into())));
        host.step(&mut HeadlessDriver::new());
        assert!(host.agents().is_empty(), "tick 0 only emits spawn_event");

        host.step(&mut HeadlessDriver::new());
        assert_eq!(host.agents().len(), 1, "tick 1 must spawn the agent");
        let id = host.agents()[0];
        assert_eq!(
            host.agent_state(id),
            Some(onyx_soul_agents::AgentState::Thinking)
        );
        assert!(!all_live_waiting_approval(host.graph(), &host.vocab()));

        let req = onyx_soul_agents::pending_requests(host.graph(), &host.vocab())[0].request;
        host.push(Reply::call(id, req, "shell_exec {\"command\":\"echo hi\"}").to_event());
        host.step(&mut HeadlessDriver::new());
        assert_eq!(host.agent_state(id), Some(AgentState::WaitingApproval));
        assert!(all_live_waiting_approval(host.graph(), &host.vocab()));
    }

    #[test]
    fn rewind_reports_orphaned_effects_after_target_tick() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-rewind-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let policy = WorkspacePolicy::new(&dir).unwrap();
        let tools = WorkspaceTools::new(policy);
        let (mut host, ws_vocab) = build_host(1, AgentConfig::default(), None, None).unwrap();
        let spec = AgentSpec::new("tester", "write a file")
            .with_tools(WorkspaceTools::all_tool_names().iter().copied());
        host.spawn(&spec);

        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode(WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab)
            .require_read_before_edit(true)
            .wrapping(HeadlessDriver::new());
        let mut driver = onyx_soul_agents::RecordingBridge::new(
            host.vocab(),
            |req: &onyx_soul_agents::AgentView| {
                if req.memory.iter().any(|(k, _)| k == "observe") {
                    Some(Reply::done(req.agent, req.request, "done"))
                } else if req.memory.iter().any(|(k, _)| k == "call") {
                    None
                } else {
                    Some(Reply::call_structured(
                        req.agent,
                        req.request,
                        "fs_write",
                        r#"{"path":"out.txt","content":"hello"}"#,
                    ))
                }
            },
        )
        .wrapping(ws_bridge);

        assert!(host.run_until_settled(&mut driver, 16));
        let orphaned = effects_after_tick(host.graph(), &ws_vocab, 0);
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0].tool, "fs_write");
        assert_eq!(orphaned[0].target, "out.txt");
        assert_eq!(orphaned[0].reversibility, "auto_reversible");

        let _ = fs::remove_dir_all(&dir);
    }
}
