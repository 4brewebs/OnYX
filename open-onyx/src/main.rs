mod command;
mod gemini;
mod ops;
mod session;
mod tui;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use command::CommandSpawnerSystem;
use gemini::{AsyncGeminiDriver, DEFAULT_GEMINI_MODEL, GeminiResponder, load_project_instructions};
use onyx_soul_agents::{
    AgentConfig, AgentSnapshotStore, AgentView, HeadlessDriver, RecordingBridge, ReplayBridge,
    Transcript,
};
use onyx_workspace::{
    PendingCheckpoints, WorkspaceMode, WorkspaceModeHandle, WorkspacePolicy, WorkspaceToolBridge,
    WorkspaceTools,
};
use ops::{reconcile_rewind_effects, run_doctor, run_inspect};
use session::{
    CliApprovalDriver, SessionOptions, StopPolicy, build_host, build_host_with_checkpoints,
    make_spec, run_session,
};
use tui::TuiDriver;

struct CliArgs {
    mode: Mode,
    workspace: PathBuf,
    auto_approve: bool,
    dry_run: bool,
    max_steps: Option<i64>,
    shell_timeout_secs: Option<u64>,
    save_transcript: Option<PathBuf>,
    snapshot_dir: Option<PathBuf>,
    resume_dir: Option<PathBuf>,
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
    Doctor {
        snapshot_dir: Option<PathBuf>,
        transcript: Option<PathBuf>,
    },
}

fn parse_args() -> Result<CliArgs, String> {
    parse_args_from(env::args().skip(1))
}

fn parse_args_from<I: Iterator<Item = String>>(mut args: I) -> Result<CliArgs, String> {
    let Some(cmd) = args.next() else {
        return Err(usage());
    };
    let mut workspace = env::current_dir().map_err(|e| e.to_string())?;
    let mut auto_approve = false;
    let mut dry_run = false;
    let mut max_steps: Option<i64> = None;
    let mut shell_timeout_secs: Option<u64> = None;
    let mut save_transcript = None;
    let mut snapshot_dir = None;
    let mut resume_dir = None;
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
            "--max-steps" => {
                let s = args
                    .next()
                    .ok_or("--max-steps requires a positive integer")?;
                let parsed = s
                    .parse::<i64>()
                    .map_err(|_| "--max-steps must be a positive integer")?;
                if parsed <= 0 {
                    return Err("--max-steps must be > 0".to_string());
                }
                max_steps = Some(parsed);
            }
            "--shell-timeout" => {
                let s = args
                    .next()
                    .ok_or("--shell-timeout requires seconds as a positive integer")?;
                let parsed = s
                    .parse::<u64>()
                    .map_err(|_| "--shell-timeout must be a positive integer")?;
                if parsed == 0 {
                    return Err("--shell-timeout must be > 0".to_string());
                }
                shell_timeout_secs = Some(parsed);
            }
            "--save-transcript" => {
                let p = args.next().ok_or("--save-transcript requires a path")?;
                save_transcript = Some(PathBuf::from(p));
            }
            "--snapshot-dir" => {
                let p = args.next().ok_or("--snapshot-dir requires a path")?;
                snapshot_dir = Some(PathBuf::from(p));
            }
            "--resume" | "-r" => {
                let p = args
                    .next()
                    .ok_or("--resume requires a snapshot directory path")?;
                resume_dir = Some(PathBuf::from(p));
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
        "doctor" => Mode::Doctor {
            snapshot_dir: snapshot_dir
                .clone()
                .or_else(|| positional.into_iter().next().map(PathBuf::from)),
            transcript: transcript_opt,
        },
        _ => return Err(usage()),
    };

    if resume_dir.is_some() && snapshot_dir.is_none() {
        snapshot_dir = resume_dir.clone();
    }

    Ok(CliArgs {
        mode,
        workspace,
        auto_approve,
        dry_run,
        max_steps,
        shell_timeout_secs,
        save_transcript,
        snapshot_dir,
        resume_dir,
    })
}

fn usage() -> String {
    "Usage:\n  openonyx chat [--workspace <dir>] [--auto-approve] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file>] [--snapshot-dir <dir>] [--resume <dir>]\n  openonyx run <goal> [--workspace <dir>] [--auto-approve] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file>] [--snapshot-dir <dir>]\n  openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>]\n  openonyx inspect <snapshot_dir> [--tick <n>]\n  openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--workspace <dir>] [--auto-approve] [--dry-run]\n  openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]".to_string()
}

fn gemini_env_and_instructions(
    workspace: &Path,
) -> Result<(String, String, String), Box<dyn std::error::Error>> {
    let api_key =
        env::var("GEMINI_API_KEY").map_err(|_| "GEMINI_API_KEY environment variable is not set")?;
    let model = env::var("ONYX_GEMINI_MODEL").unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.to_string());
    let project_instructions = load_project_instructions(workspace);
    Ok((api_key, model, project_instructions))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    let mut policy = WorkspacePolicy::new(&cli.workspace)?;
    if let Some(secs) = cli.shell_timeout_secs {
        policy = policy.with_command_timeout(Duration::from_secs(secs));
    }
    let tools = WorkspaceTools::new(policy);

    let config = AgentConfig {
        default_max_steps: cli.max_steps.unwrap_or(64),
        default_timeout_ticks: 128,
        ..AgentConfig::default()
    };

    let session_opts = SessionOptions {
        snapshot_dir: cli.snapshot_dir.clone(),
        save_transcript: cli.save_transcript.clone(),
        save_step_snapshots: cli.snapshot_dir.is_some(),
    };

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
            let (api_key, model, project_instructions) =
                gemini_env_and_instructions(&cli.workspace)?;

            host.spawn(&make_spec(&goal, cli.auto_approve));

            let cli_driver = CliApprovalDriver::new(host.vocab(), HeadlessDriver::new());
            let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
                .with_mode(WorkspaceMode::Build)
                .with_workspace_vocab(ws_vocab)
                .with_checkpoints(checkpoints)
                .require_read_before_edit(true)
                .wrapping(cli_driver);
            let mut driver = RecordingBridge::new(
                host.vocab(),
                GeminiResponder::new(api_key, model)
                    .with_project_instructions(project_instructions),
            )
            .wrapping(ws_bridge);

            run_session(
                &mut host,
                &mut driver,
                &session_opts,
                StopPolicy::Settled { max_ticks: 256 },
            )?;

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
            let chat_config = AgentConfig {
                default_max_steps: cli.max_steps.unwrap_or(128),
                default_timeout_ticks: 512,
                ..config
            };
            let mode_handle = WorkspaceModeHandle::new(WorkspaceMode::Build);
            let spawner = CommandSpawnerSystem::new(cli.auto_approve)
                .with_max_steps(chat_config.default_max_steps)
                .with_mode_handle(mode_handle.clone());
            let (mut host, ws_vocab) = build_host_with_checkpoints(
                1,
                chat_config,
                cli.snapshot_dir.as_ref(),
                Some(Box::new(spawner)),
                Some(checkpoints.clone()),
            )?;
            if let Some(resume_path) = &cli.resume_dir {
                let store = AgentSnapshotStore::new(resume_path);
                let latest = store
                    .latest_tick()
                    .ok_or_else(|| format!("no snapshots found in `{}`", resume_path.display()))?;
                host.request_restore(&latest.to_string());
                host.step(&mut HeadlessDriver::new());
                host.set_tick(latest.saturating_add(1));
            }
            let (api_key, model, project_instructions) =
                gemini_env_and_instructions(&cli.workspace)?;

            let tui = TuiDriver::new(host.vocab())?
                .with_workspace(cli.workspace.clone(), mode_handle.clone());
            let tui_handle = tui.handle();
            let quit = tui.quit_flag();
            let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
                .with_mode_handle(mode_handle)
                .with_workspace_vocab(ws_vocab)
                .sync_existing_undos(host.graph())
                .with_checkpoints(checkpoints)
                .require_read_before_edit(true)
                .wrapping(tui);
            let llm_in_flight = Arc::new(AtomicUsize::new(0));
            let gemini_ready = Arc::new(AtomicBool::new(false));
            let async_gemini = AsyncGeminiDriver::new(
                host.vocab(),
                api_key,
                model,
                Arc::clone(&llm_in_flight),
                Arc::clone(&gemini_ready),
            )
            .with_project_instructions(project_instructions)
            .wrapping(ws_bridge);
            let mut driver =
                RecordingBridge::new(host.vocab(), |_: &AgentView| None).wrapping(async_gemini);

            run_session(
                &mut host,
                &mut driver,
                &session_opts,
                StopPolicy::Quit {
                    quit,
                    tui: tui_handle,
                    llm_in_flight,
                    llm_ready: gemini_ready,
                },
            )?;
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
            run_session(
                &mut host,
                &mut driver,
                &session_opts,
                StopPolicy::ReplayExhausted { max_ticks: 4096 },
            )?;

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
            run_inspect(&dir, tick, config)?;
        }
        Mode::Rewind {
            dir,
            tick,
            transcript,
        } => {
            reconcile_rewind_effects(&cli.workspace, &dir, tick, &config, cli.dry_run)?;
            if cli.dry_run {
                return Ok(());
            }

            let checkpoints = PendingCheckpoints::default();
            let spawner = CommandSpawnerSystem::new(cli.auto_approve).with_max_steps(128);
            let (mut host, ws_vocab) = build_host_with_checkpoints(
                1,
                config,
                Some(&dir),
                Some(Box::new(spawner)),
                Some(checkpoints.clone()),
            )?;
            host.request_restore(&tick.to_string());
            host.step(&mut HeadlessDriver::new());

            let next_tick = tick.saturating_add(1);
            host.set_tick(next_tick);

            let rewind_opts = SessionOptions::default();
            if let Some(tr_path) = transcript {
                let bytes = fs::read(tr_path)?;
                let tr = Transcript::from_bytes(&bytes).map_err(|e| e.to_string())?;
                let mut driver = ReplayBridge::new(tr)
                    .starting_at(next_tick)
                    .wrapping(HeadlessDriver::new());
                run_session(
                    &mut host,
                    &mut driver,
                    &rewind_opts,
                    StopPolicy::ReplayExhausted { max_ticks: 4096 },
                )?;
            } else {
                let (api_key, model, project_instructions) =
                    gemini_env_and_instructions(&cli.workspace)?;
                let cli_driver = CliApprovalDriver::new(host.vocab(), HeadlessDriver::new());
                let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
                    .with_mode(WorkspaceMode::Build)
                    .with_workspace_vocab(ws_vocab)
                    .sync_existing_undos(host.graph())
                    .with_checkpoints(checkpoints)
                    .require_read_before_edit(true)
                    .wrapping(cli_driver);
                let mut driver = RecordingBridge::new(
                    host.vocab(),
                    GeminiResponder::new(api_key, model)
                        .with_project_instructions(project_instructions),
                )
                .starting_at(next_tick)
                .wrapping(ws_bridge);

                run_session(
                    &mut host,
                    &mut driver,
                    &rewind_opts,
                    StopPolicy::Settled { max_ticks: 256 },
                )?;
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
        Mode::Doctor {
            snapshot_dir,
            transcript,
        } => {
            let report = run_doctor(&cli.workspace, snapshot_dir.as_ref(), transcript.as_ref());
            report.print();
            if !report.errors.is_empty() {
                return Err(format!("doctor detected {} error(s)", report.errors.len()).into());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use onyx_soul_agents::{AgentHost, AgentState, IngressEvent, OnyxEvent, Reply};
    use onyx_workspace::{graph_workspace_mode, latest_undoable_effect};
    use session::{SURFACE_APPROVAL, active_surfaces, all_live_waiting_approval};

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

        let surfaces = active_surfaces(host.graph(), &host.vocab());
        let approval_surface = surfaces
            .iter()
            .find(|s| s.surface == SURFACE_APPROVAL)
            .expect("approval surface must be declared");
        assert!(approval_surface.focused);
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
        let spec = agent_spec_for_test("write a file");
        host.spawn(&spec);

        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode(WorkspaceMode::Build)
            .with_workspace_vocab(ws_vocab)
            .require_read_before_edit(true)
            .wrapping(HeadlessDriver::new());
        let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
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
        })
        .wrapping(ws_bridge);

        assert!(host.run_until_settled(&mut driver, 16));
        let orphaned = onyx_workspace::effects_after_tick(host.graph(), &ws_vocab, 0);
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0].tool, "fs_write");
        assert_eq!(orphaned[0].target, "out.txt");
        assert_eq!(orphaned[0].reversibility, "auto_reversible");

        let _ = fs::remove_dir_all(&dir);
    }

    fn agent_spec_for_test(goal: &str) -> onyx_soul_agents::AgentSpec {
        onyx_soul_agents::AgentSpec::new("tester", goal)
            .with_tools(WorkspaceTools::all_tool_names().iter().copied())
    }

    #[test]
    fn parse_args_supports_max_steps_and_rejects_non_positive() {
        let parsed = parse_args_from(
            ["chat", "--max-steps", "96"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert_eq!(parsed.max_steps, Some(96));

        let err = parse_args_from(["chat", "--max-steps", "0"].into_iter().map(str::to_string))
            .err()
            .expect("0 max-steps must be rejected");
        assert!(err.contains("--max-steps"), "{err}");

        let parsed_timeout = parse_args_from(
            ["chat", "--shell-timeout", "45"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert_eq!(parsed_timeout.shell_timeout_secs, Some(45));

        let parsed_resume = parse_args_from(
            ["chat", "--resume", "snaps_prev"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert_eq!(parsed_resume.resume_dir, Some(PathBuf::from("snaps_prev")));
        assert_eq!(
            parsed_resume.snapshot_dir,
            Some(PathBuf::from("snaps_prev"))
        );
    }

    #[test]
    fn command_spawner_supports_multi_turn_mode_new_and_undo() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-chat-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let run_git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git must run");
            assert!(out.status.success(), "git {:?} failed", args);
        };
        run_git(&["init"]);
        run_git(&["config", "user.name", "Test"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "core.autocrlf", "false"]);
        fs::write(dir.join("note.txt"), "v1\n").unwrap();
        run_git(&["add", "note.txt"]);
        run_git(&["commit", "-m", "init"]);

        let checkpoints = PendingCheckpoints::default();
        let mode_handle = WorkspaceModeHandle::new(WorkspaceMode::Build);
        let spawner = CommandSpawnerSystem::new(true)
            .with_max_steps(16)
            .with_mode_handle(mode_handle.clone());
        let (mut host, ws_vocab) = build_host_with_checkpoints(
            1,
            AgentConfig {
                default_max_steps: 16,
                default_timeout_ticks: 64,
                ..AgentConfig::default()
            },
            None,
            Some(Box::new(spawner)),
            Some(checkpoints.clone()),
        )
        .unwrap();

        let tools = WorkspaceTools::new(WorkspacePolicy::new(&dir).unwrap());
        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode_handle(mode_handle.clone())
            .with_workspace_vocab(ws_vocab)
            .with_checkpoints(checkpoints)
            .wrapping(HeadlessDriver::new());
        let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| match req.request {
            1 => Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_write",
                r#"{"path":"note.txt","content":"v2\n"}"#,
            )),
            2 => Some(Reply::done(req.agent, req.request, "updated note")),
            3 => Some(Reply::done(req.agent, req.request, "follow-up answer")),
            _ => None,
        })
        .wrapping(ws_bridge);

        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "edit note".into(),
        )));
        host.step(&mut driver);
        host.step(&mut driver);
        assert!(host.run_until_settled(&mut driver, 32));
        assert_eq!(host.agents().len(), 1);
        assert_eq!(fs::read_to_string(dir.join("note.txt")).unwrap(), "v2\n");

        // `/mode plan` persiste el modo en el grafo y actualiza el handle; `/mode build` vuelve a Build.
        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "/mode plan".into(),
        )));
        host.step(&mut driver);
        assert_eq!(mode_handle.get(), WorkspaceMode::Plan);
        assert_eq!(
            graph_workspace_mode(host.graph(), &ws_vocab),
            Some(WorkspaceMode::Plan)
        );
        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "/mode build".into(),
        )));
        host.step(&mut driver);
        assert_eq!(mode_handle.get(), WorkspaceMode::Build);
        assert_eq!(
            graph_workspace_mode(host.graph(), &ws_vocab),
            Some(WorkspaceMode::Build)
        );

        // `/undo` marca la intención en el grafo (sistema puro) y `WorkspaceToolBridge::present`
        // restaura `note.txt` a `v1\n`.
        host.push(OnyxEvent::Ingress(IngressEvent::Command("/undo".into())));
        host.step(&mut driver);
        assert_eq!(fs::read_to_string(dir.join("note.txt")).unwrap(), "v1\n");
        assert!(latest_undoable_effect(host.graph(), &ws_vocab).is_none());

        // Turno 2 sobre el mismo agente raíz (`request = 3`).
        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "continue work".into(),
        )));
        assert!(host.run_until_settled(&mut driver, 32));
        assert_eq!(host.agents().len(), 1);
        let a1 = host.agents()[0];
        assert_eq!(host.result(a1).as_deref(), Some("follow-up answer"));
        let mem_kinds: Vec<String> = host.view(a1).memory.into_iter().map(|(k, _)| k).collect();
        assert!(mem_kinds.contains(&"system".to_string()), "{mem_kinds:?}");
        assert!(mem_kinds.contains(&"say".to_string()), "{mem_kinds:?}");
        assert!(mem_kinds.contains(&"user".to_string()), "{mem_kinds:?}");

        // `/new fresh goal` crea un segundo agente distinto.
        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "/new fresh goal".into(),
        )));
        host.step(&mut driver);
        host.step(&mut driver);
        assert_eq!(host.agents().len(), 2);

        // Replay determinista: `ReplayBridge` con comandos `/mode plan` y `/undo`
        // ejecuta cambios sobre el grafo sin tocar `note.txt` en disco.
        let mut replay_tr = driver.transcript().clone();
        replay_tr.commands = vec![
            onyx_soul_agents::LoggedCommand {
                poll: 0,
                command: "edit note".to_string(),
            },
            onyx_soul_agents::LoggedCommand {
                poll: 5,
                command: "/mode plan".to_string(),
            },
            onyx_soul_agents::LoggedCommand {
                poll: 6,
                command: "/mode build".to_string(),
            },
            onyx_soul_agents::LoggedCommand {
                poll: 7,
                command: "/undo".to_string(),
            },
            onyx_soul_agents::LoggedCommand {
                poll: 8,
                command: "continue work".to_string(),
            },
            onyx_soul_agents::LoggedCommand {
                poll: 9,
                command: "/mode plan".to_string(),
            },
        ];
        fs::write(dir.join("note.txt"), "untouched-during-replay\n").unwrap();
        let replay_mode = WorkspaceModeHandle::new(WorkspaceMode::Build);
        let (mut replay_host, replay_ws_vocab) = build_host(
            1,
            AgentConfig {
                default_max_steps: 16,
                default_timeout_ticks: 64,
                ..AgentConfig::default()
            },
            None,
            Some(Box::new(
                CommandSpawnerSystem::new(true)
                    .with_max_steps(16)
                    .with_mode_handle(replay_mode.clone()),
            )),
        )
        .unwrap();
        let mut replay_driver = ReplayBridge::new(replay_tr).wrapping(HeadlessDriver::new());
        run_session(
            &mut replay_host,
            &mut replay_driver,
            &SessionOptions::default(),
            StopPolicy::ReplayExhausted { max_ticks: 64 },
        )
        .unwrap();
        assert_eq!(replay_mode.get(), WorkspaceMode::Plan);
        assert_eq!(
            graph_workspace_mode(replay_host.graph(), &replay_ws_vocab),
            Some(WorkspaceMode::Plan)
        );
        assert_eq!(replay_host.agents().len(), 1);
        let replayed_agent = replay_host.agents()[0];
        assert_eq!(
            replay_host.result(replayed_agent).as_deref(),
            Some("follow-up answer")
        );
        let replayed_sys: Vec<String> = replay_host
            .view(replayed_agent)
            .memory
            .into_iter()
            .filter(|(k, _)| k == "system")
            .map(|(_, v)| v)
            .collect();
        assert!(
            replayed_sys
                .iter()
                .any(|m| m.starts_with("[/undo] Restored git checkpoint ")),
            "expected [/undo] Restored git checkpoint in replay memory, got: {replayed_sys:?}"
        );
        assert!(
            !replayed_sys
                .iter()
                .any(|m| m.contains("No reversible git checkpoint available")),
            "unexpected missing checkpoint note in replay memory: {replayed_sys:?}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("note.txt")).unwrap(),
            "untouched-during-replay\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn doctor_validates_snapshots_transcript_and_flags_bad_headers() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-doctor-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let snap_dir = dir.join("snaps_test");
        fs::create_dir_all(&snap_dir).unwrap();

        let (mut host, _) = build_host(1, AgentConfig::default(), Some(&snap_dir), None).unwrap();
        host.spawn(&make_spec("check doctor", true));
        host.request_snapshot("0");
        host.step(&mut HeadlessDriver::new());

        let tr_path = dir.join("ok.tr");
        let rec = RecordingBridge::new(host.vocab(), |req: &AgentView| {
            Some(Reply::done(req.agent, req.request, "done"))
        })
        .wrapping(HeadlessDriver::new());
        fs::write(&tr_path, rec.transcript().to_bytes()).unwrap();

        let report = run_doctor(&dir, Some(&snap_dir), Some(&tr_path));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("ok.tr") && w.contains("snaps_test")),
            "{:?}",
            report.warnings
        );

        let bad_tr = dir.join("bad.tr");
        fs::write(&bad_tr, b"NOT_A_TRANSCRIPT").unwrap();
        let bad_report = run_doctor(&dir, Some(&snap_dir), Some(&bad_tr));
        assert!(!bad_report.errors.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
