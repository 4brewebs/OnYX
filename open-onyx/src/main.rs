mod command;
mod context;
mod gemini;
mod openai;
mod ops;
mod pack;
mod session;
mod tui;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use command::CommandSpawnerSystem;
use context::{SessionTelemetryHandle, TieredContextConfig};
use gemini::{
    AsyncGeminiDriver, DEFAULT_GEMINI_MODEL, GeminiResponder, StreamPreviewHandle,
    load_project_instructions,
};
use onyx_soul_agents::{
    AgentConfig, AgentView, HeadlessDriver, RecordingBridge, ReplayBridge, Responder, Transcript,
};
use onyx_workspace::{
    PendingCheckpoints, WorkspaceMode, WorkspaceModeHandle, WorkspacePolicy, WorkspaceToolBridge,
    WorkspaceTools,
};
use openai::{AsyncOpenAiDriver, DEFAULT_OPENAI_MODEL, OpenAiResponder};
use ops::{reconcile_rewind_effects, run_doctor, run_inspect};
use pack::PackSnapshotStore;
use session::{
    CliApprovalDriver, SessionOptions, StopPolicy, build_host_with_checkpoints,
    build_host_with_options, make_spec, run_session,
};
use tui::TuiDriver;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LlmProvider {
    Gemini,
    OpenAi,
}

impl LlmProvider {
    fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gemini" => Ok(Self::Gemini),
            "openai" => Ok(Self::OpenAi),
            other => Err(format!(
                "unsupported provider `{other}` (expected `gemini` or `openai`)"
            )),
        }
    }
}

struct LlmConfig {
    provider: LlmProvider,
    api_key: String,
    model: String,
    api_base: Option<String>,
    project_instructions: String,
}

struct CliArgs {
    mode: Mode,
    workspace: PathBuf,
    provider: Option<LlmProvider>,
    model: Option<String>,
    api_base: Option<String>,
    auto_approve: bool,
    ignore_deny: bool,
    dry_run: bool,
    max_steps: Option<i64>,
    shell_timeout_secs: Option<u64>,
    save_transcript: Option<PathBuf>,
    snapshot_dir: Option<PathBuf>,
    resume_dir: Option<PathBuf>,
    snapshot_every: Option<u64>,
    max_snapshots: Option<usize>,
}

impl CliArgs {
    fn simple(mode: Mode) -> Self {
        Self {
            mode,
            workspace: PathBuf::new(),
            provider: None,
            model: None,
            api_base: None,
            auto_approve: false,
            ignore_deny: false,
            dry_run: false,
            max_steps: None,
            shell_timeout_secs: None,
            save_transcript: None,
            snapshot_dir: None,
            resume_dir: None,
            snapshot_every: None,
            max_snapshots: None,
        }
    }
}

enum Mode {
    Help,
    Version,
    Run(String),
    Chat,
    Replay(PathBuf),
    Inspect {
        dir: PathBuf,
        tick: Option<u64>,
        list: bool,
        json: bool,
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
    match cmd.as_str() {
        "--help" | "-h" | "help" => return Ok(CliArgs::simple(Mode::Help)),
        "--version" | "-V" | "version" => return Ok(CliArgs::simple(Mode::Version)),
        _ => {}
    }
    let mut workspace = env::current_dir().map_err(|e| e.to_string())?;
    let mut provider: Option<LlmProvider> = None;
    let mut model: Option<String> = None;
    let mut api_base: Option<String> = None;
    let mut auto_approve = false;
    let mut ignore_deny = false;
    let mut dry_run = false;
    let mut max_steps: Option<i64> = None;
    let mut shell_timeout_secs: Option<u64> = None;
    let mut save_transcript = None;
    let mut snapshot_dir = None;
    let mut resume_dir = None;
    let mut snapshot_every: Option<u64> = None;
    let mut max_snapshots: Option<usize> = None;
    let mut tick_opt: Option<u64> = None;
    let mut transcript_opt: Option<PathBuf> = None;
    let mut list = false;
    let mut json = false;
    let mut positional = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" | "-w" => {
                let p = args.next().ok_or("--workspace requires a path")?;
                workspace = PathBuf::from(p);
            }
            "--provider" => {
                let p = args
                    .next()
                    .ok_or("--provider requires `gemini` or `openai`")?;
                provider = Some(LlmProvider::parse(&p)?);
            }
            "--model" => {
                let m = args.next().ok_or("--model requires a model name")?;
                if m.trim().is_empty() {
                    return Err("--model must not be empty".to_string());
                }
                model = Some(m);
            }
            "--api-base" => {
                let u = args.next().ok_or("--api-base requires a URL")?;
                if u.trim().is_empty() {
                    return Err("--api-base must not be empty".to_string());
                }
                api_base = Some(u);
            }
            "--auto-approve" => auto_approve = true,
            "--ignore-deny" => ignore_deny = true,
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
            "--save-transcript" | "--record" => {
                let p = args.next().ok_or("--save-transcript requires a path")?;
                save_transcript = Some(PathBuf::from(p));
            }
            "--snapshot-dir" => {
                let p = args.next().ok_or("--snapshot-dir requires a path")?;
                snapshot_dir = Some(PathBuf::from(p));
            }
            "--snapshot-every" => {
                let s = args
                    .next()
                    .ok_or("--snapshot-every requires a positive integer")?;
                let parsed = s
                    .parse::<u64>()
                    .map_err(|_| "--snapshot-every must be a positive integer")?;
                if parsed == 0 {
                    return Err("--snapshot-every must be > 0".to_string());
                }
                snapshot_every = Some(parsed);
            }
            "--max-snapshots" => {
                let s = args
                    .next()
                    .ok_or("--max-snapshots requires a positive integer")?;
                let parsed = s
                    .parse::<usize>()
                    .map_err(|_| "--max-snapshots must be a positive integer")?;
                if parsed == 0 {
                    return Err("--max-snapshots must be > 0".to_string());
                }
                max_snapshots = Some(parsed);
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
            "--list" | "-l" => list = true,
            "--json" => json = true,
            "--help" | "-h" => return Ok(CliArgs::simple(Mode::Help)),
            "--version" | "-V" => return Ok(CliArgs::simple(Mode::Version)),
            _ if arg.starts_with('-') => {
                return Err(format!("unknown flag `{arg}`\n\n{}", usage()));
            }
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
                list,
                json,
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
        provider,
        model,
        api_base,
        auto_approve,
        ignore_deny,
        dry_run,
        max_steps,
        shell_timeout_secs,
        save_transcript,
        snapshot_dir,
        resume_dir,
        snapshot_every,
        max_snapshots,
    })
}

fn usage() -> String {
    "Usage:\n  openonyx chat [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--api-base <url>] [--auto-approve] [--ignore-deny] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>] [--resume <dir>]\n  openonyx run <goal> [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--api-base <url>] [--auto-approve] [--ignore-deny] [--max-steps <n>] [--shell-timeout <secs>] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]\n  openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]\n  openonyx inspect <snapshot_dir> [--tick <n>] [--list] [--json]\n  openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--api-base <url>] [--auto-approve] [--ignore-deny] [--dry-run]\n  openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]\n  openonyx [--help | -h | --version | -V]".to_string()
}

fn help_text() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let usage_str = usage();
    let body = r#"SUBCOMMANDS:
  chat                     Launch interactive TUI with SSE streaming, diff approvals, Plan/Build toggle, and Live Plan
  run <goal>               Run a single headless turn toward <goal> and print the final result
  replay <transcript>      Replay a recorded ONYXTR2 transcript offline (0 network calls, 0 disk mutations)
  inspect <snapshot_dir>   Inspect milestone snapshots (snapshots.onyxpack or legacy tick-*.bin) and graph state
  rewind <dir> <tick>      Restore git checkpoints to <tick> (or prior milestone) and replay/continue
  doctor                   Verify workspace git repository, snapshot packs, and transcript integrity

OPTIONS:
  -w, --workspace <dir>    Target workspace directory (default: current working directory)
      --provider <name>    LLM backend: `gemini` (default) or `openai` (OpenAI-compatible / Ollama / vLLM / LM Studio)
      --model <name>       Model name (defaults: Gemini `__GEMINI_MODEL__`, OpenAI `__OPENAI_MODEL__`)
      --api-base <url>     Custom API base URL (e.g. `http://localhost:11434/v1`; no OPENAI_API_KEY needed when set)
      --auto-approve       Skip interactive approval prompts for mutating tools (`fs_write`, `fs_edit`, `shell_exec`; `deny` rules in `.onyx/config.toml` still apply)
      --ignore-deny        Explicitly bypass `[permissions].deny` rules loaded from `.onyx/config.toml`
      --max-steps <n>      Max agent steps per turn (default: 128 in `chat`, 64 in `run`)
      --shell-timeout <s>  Timeout in seconds per `shell_exec` command
      --save-transcript <f>, --record <f>
                           Record deterministic `ONYXTR2` transcript to `<file>`
      --snapshot-dir <dir> Save compressed milestone snapshots to `<dir>/snapshots.onyxpack`
      --snapshot-every <n> Also capture periodic snapshots every <n> ticks
      --max-snapshots <n>  Opt-in sliding window retention (keeps `init` + latest `n - 1` milestones)
  -r, --resume <dir>       Resume `chat` from the latest snapshot stored in `<dir>`
      --tick <n>           Specific tick to inspect (use with `inspect`)
  -l, --list               List all stored snapshot milestones with tags and git checkpoints (use with `inspect`)
      --json               Emit machine-readable JSON output (use with `inspect`)
      --transcript <file>  Replay transcript from rewind point (`rewind`) or validate transcript (`doctor`)
      --dry-run            Preview orphaned effects and git rollback plan in `rewind` without modifying files
  -h, --help               Print this help guide and exit 0
  -V, --version            Print openOnyx version and exit 0

TUI CONTROLS & SLASH COMMANDS (chat):
  Enter                    Submit goal, follow-up prompt, or slash command
  Tab                      Toggle [PLAN] (read-only) and [BUILD] (edits enabled)
  y / n                    Approve or deny a pending mutating tool diff
  Up / Down / PgUp / PgDn / Home / End / Ctrl+U / Ctrl+D
                           Scroll the `Memory & Trace` history panel
  /undo                    Revert the latest reversible git checkpoint created during the session
  /compact [focus]         Compact older turns into a verified session summary (creating a `cmd:/compact` milestone)
  /mode [plan|build|explore]
                           Switch or toggle workspace mode
  /new [goal]              Start a fresh agent conversation (optionally passing an immediate <goal>)
  Esc / Ctrl+C             Quit cleanly (flushing exit snapshot & transcript if configured)

PROJECT INSTRUCTIONS, SKILLS & LIVE PLAN:
  ONYX.md / AGENTS.md      Loaded from the workspace root into system instructions
  .onyx/skills/<name>/SKILL.md (also skills/, ~/.onyx/skills/)
                           Discovered as a compact catalog and loaded on demand via `load_skill` (includes `scent` & `rust`)
  Live Plan panel          Updated via `plan_update` and shown on the right when terminal width >= 100 columns

ENVIRONMENT VARIABLES:
  GEMINI_API_KEY, OPENAI_API_KEY, ONYX_API_KEY
  ONYX_PROVIDER            Default provider (`gemini` | `openai`)
  ONYX_MODEL               Default model override (`ONYX_GEMINI_MODEL`, `OPENAI_MODEL` as fallbacks)
  GEMINI_API_BASE          Custom Gemini API endpoint
  OPENAI_API_BASE / OPENAI_BASE_URL
                           Custom OpenAI-compatible API endpoint"#
        .replace("__GEMINI_MODEL__", DEFAULT_GEMINI_MODEL)
        .replace("__OPENAI_MODEL__", DEFAULT_OPENAI_MODEL);

    format!("openOnyx {version} — Deterministic Coding Agent Harness\n\n{usage_str}\n\n{body}")
}

fn resolve_llm_config(
    cli: &CliArgs,
    workspace: &Path,
) -> Result<LlmConfig, Box<dyn std::error::Error>> {
    let provider = match cli.provider {
        Some(p) => p,
        None => match env::var("ONYX_PROVIDER") {
            Ok(s) if !s.trim().is_empty() => LlmProvider::parse(&s)?,
            _ => LlmProvider::Gemini,
        },
    };
    let project_instructions = load_project_instructions(workspace);

    match provider {
        LlmProvider::Gemini => {
            let api_key = env::var("GEMINI_API_KEY")
                .or_else(|_| env::var("ONYX_API_KEY"))
                .map_err(|_| "GEMINI_API_KEY environment variable is not set")?;
            let model = cli
                .model
                .clone()
                .or_else(|| env::var("ONYX_MODEL").ok().filter(|s| !s.trim().is_empty()))
                .or_else(|| {
                    env::var("ONYX_GEMINI_MODEL")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| DEFAULT_GEMINI_MODEL.to_string());
            let api_base = cli
                .api_base
                .clone()
                .or_else(|| env::var("GEMINI_API_BASE").ok())
                .filter(|s| !s.trim().is_empty());
            Ok(LlmConfig {
                provider,
                api_key,
                model,
                api_base,
                project_instructions,
            })
        }
        LlmProvider::OpenAi => {
            let api_base = cli
                .api_base
                .clone()
                .or_else(|| env::var("OPENAI_API_BASE").ok())
                .or_else(|| env::var("OPENAI_BASE_URL").ok())
                .filter(|s| !s.trim().is_empty());
            let api_key = match env::var("OPENAI_API_KEY").or_else(|_| env::var("ONYX_API_KEY")) {
                Ok(k) => k,
                Err(_) if api_base.is_some() => String::new(),
                Err(_) => return Err("OPENAI_API_KEY environment variable is not set".into()),
            };
            let model = cli
                .model
                .clone()
                .or_else(|| env::var("ONYX_MODEL").ok().filter(|s| !s.trim().is_empty()))
                .or_else(|| {
                    env::var("OPENAI_MODEL")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string());
            Ok(LlmConfig {
                provider,
                api_key,
                model,
                api_base,
                project_instructions,
            })
        }
    }
}

fn build_sync_responder(
    llm: &LlmConfig,
    telemetry: Option<SessionTelemetryHandle>,
) -> Box<dyn Responder> {
    let ctx_cfg = TieredContextConfig::for_model(&llm.model);
    match llm.provider {
        LlmProvider::Gemini => {
            let mut r = GeminiResponder::new(llm.api_key.clone(), llm.model.clone())
                .with_api_base(llm.api_base.clone())
                .with_project_instructions(llm.project_instructions.clone())
                .with_context_config(ctx_cfg);
            if let Some(tel) = telemetry {
                r = r.with_telemetry(tel);
            }
            Box::new(r)
        }
        LlmProvider::OpenAi => {
            let mut r = OpenAiResponder::new(llm.api_key.clone(), llm.model.clone())
                .with_api_base(llm.api_base.clone())
                .with_project_instructions(llm.project_instructions.clone())
                .with_context_config(ctx_cfg);
            if let Some(tel) = telemetry {
                r = r.with_telemetry(tel);
            }
            Box::new(r)
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    match &cli.mode {
        Mode::Help => {
            println!("{}", help_text());
            return Ok(());
        }
        Mode::Version => {
            println!("openOnyx {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

    let mut policy = WorkspacePolicy::new(&cli.workspace)?.with_loaded_config();
    if let Some(secs) = cli.shell_timeout_secs {
        policy = policy.with_command_timeout(Duration::from_secs(secs));
    }
    if cli.ignore_deny {
        policy = policy.with_ignore_deny(true);
    }
    let tools = WorkspaceTools::new(policy.clone());

    let config = AgentConfig {
        default_max_steps: cli.max_steps.unwrap_or(64),
        default_timeout_ticks: 128,
        ..AgentConfig::default()
    };

    let session_opts = SessionOptions {
        snapshot_dir: cli.snapshot_dir.clone(),
        save_transcript: cli.save_transcript.clone(),
        save_step_snapshots: cli.snapshot_dir.is_some(),
        snapshot_every: cli.snapshot_every,
        max_snapshots: cli.max_snapshots,
    };

    match &cli.mode {
        Mode::Run(goal) => {
            let checkpoints = PendingCheckpoints::default();
            let (mut host, ws_vocab) =
                build_host_with_options(1, config, &session_opts, None, Some(checkpoints.clone()))?;
            let llm = resolve_llm_config(&cli, &cli.workspace)?;

            host.spawn(&make_spec(goal, cli.auto_approve));

            let cli_driver = CliApprovalDriver::new(host.vocab(), HeadlessDriver::new())
                .with_policy(policy.clone());
            let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
                .with_mode(WorkspaceMode::Build)
                .with_workspace_vocab(ws_vocab)
                .with_checkpoints(checkpoints)
                .require_read_before_edit(true)
                .wrapping(cli_driver);
            let telemetry = SessionTelemetryHandle::new(&llm.model, llm.api_base.is_some());
            let mut driver = RecordingBridge::new(
                host.vocab(),
                build_sync_responder(&llm, Some(telemetry.clone())),
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
            let snap = telemetry.snapshot();
            if snap.cumulative.input_tokens > 0 || snap.cumulative.output_tokens > 0 {
                println!("[openOnyx telemetry] {}", snap.format_status_bar());
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
            let (mut host, ws_vocab) = build_host_with_options(
                1,
                chat_config,
                &session_opts,
                Some(Box::new(spawner)),
                Some(checkpoints.clone()),
            )?;
            if let Some(resume_path) = &cli.resume_dir {
                let store = PackSnapshotStore::new(resume_path);
                let latest = store
                    .latest_tick()
                    .ok_or_else(|| format!("no snapshots found in `{}`", resume_path.display()))?;
                host.request_restore(&latest.to_string());
                host.step(&mut HeadlessDriver::new());
                host.set_tick(latest.saturating_add(1));
            }
            let llm = resolve_llm_config(&cli, &cli.workspace)?;
            let stream_preview = StreamPreviewHandle::new();
            let ctx_cfg = TieredContextConfig::for_model(&llm.model);
            let telemetry = SessionTelemetryHandle::new(&llm.model, llm.api_base.is_some());

            let tui = TuiDriver::new(host.vocab())?
                .with_workspace(cli.workspace.clone(), mode_handle.clone())
                .with_policy(policy.clone())
                .with_workspace_vocab(ws_vocab)
                .with_stream_preview(stream_preview.clone())
                .with_telemetry(telemetry.clone())
                .with_context_config(ctx_cfg);
            let tui_handle = tui.handle();
            let quit = tui.quit_flag();
            let bridge_ready = Arc::new(AtomicBool::new(false));
            let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
                .with_mode_handle(mode_handle)
                .with_workspace_vocab(ws_vocab)
                .sync_existing_undos(host.graph())
                .with_checkpoints(checkpoints)
                .with_ready_flag(Arc::clone(&bridge_ready))
                .require_read_before_edit(true)
                .wrapping(tui);
            let llm_in_flight = Arc::new(AtomicUsize::new(0));
            let llm_ready = Arc::new(AtomicBool::new(false));
            let rec_bridge = RecordingBridge::new(host.vocab(), |_: &AgentView| None);
            let mut driver = match llm.provider {
                LlmProvider::Gemini => rec_bridge.wrapping(
                    AsyncGeminiDriver::new(
                        host.vocab(),
                        llm.api_key,
                        llm.model,
                        Arc::clone(&llm_in_flight),
                        Arc::clone(&llm_ready),
                    )
                    .with_api_base(llm.api_base)
                    .with_project_instructions(llm.project_instructions)
                    .with_stream_preview(stream_preview)
                    .with_telemetry(telemetry)
                    .with_context_config(ctx_cfg)
                    .wrapping(ws_bridge),
                ),
                LlmProvider::OpenAi => rec_bridge.wrapping(
                    AsyncOpenAiDriver::new(
                        host.vocab(),
                        llm.api_key,
                        llm.model,
                        Arc::clone(&llm_in_flight),
                        Arc::clone(&llm_ready),
                    )
                    .with_api_base(llm.api_base)
                    .with_project_instructions(llm.project_instructions)
                    .with_stream_preview(stream_preview)
                    .with_telemetry(telemetry)
                    .with_context_config(ctx_cfg)
                    .wrapping(ws_bridge),
                ),
            };

            run_session(
                &mut host,
                &mut driver,
                &session_opts,
                StopPolicy::Quit {
                    quit,
                    tui: tui_handle,
                    llm_in_flight,
                    llm_ready,
                    bridge_ready,
                },
            )?;
        }
        Mode::Replay(path) => {
            let bytes = fs::read(path)?;
            let transcript = Transcript::from_bytes(&bytes).map_err(|e| e.to_string())?;
            let has_commands = !transcript.commands.is_empty();

            let (mut host, _ws_vocab) = build_host_with_options(
                1,
                config,
                &session_opts,
                Some(Box::new(CommandSpawnerSystem::new(cli.auto_approve))),
                Some(PendingCheckpoints::default()),
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
        Mode::Inspect {
            dir,
            tick,
            list,
            json,
        } => {
            run_inspect(dir, *tick, *list, *json, config)?;
        }
        Mode::Rewind {
            dir,
            tick,
            transcript,
        } => {
            let store = PackSnapshotStore::new(dir);
            let target_tick = store.resolve_tick_or_floor(*tick).ok_or_else(|| {
                format!(
                    "no snapshots found in `{}` at or before tick {tick}",
                    dir.display()
                )
            })?;
            if target_tick != *tick {
                println!(
                    "[openOnyx rewind] requested tick {tick} is not a milestone; resolving to prior milestone tick {target_tick}"
                );
            }
            reconcile_rewind_effects(&cli.workspace, dir, target_tick, &config, cli.dry_run)?;
            if cli.dry_run {
                return Ok(());
            }

            let checkpoints = PendingCheckpoints::default();
            let spawner = CommandSpawnerSystem::new(cli.auto_approve).with_max_steps(128);
            let (mut host, ws_vocab) = build_host_with_checkpoints(
                1,
                config,
                Some(dir),
                Some(Box::new(spawner)),
                Some(checkpoints.clone()),
            )?;
            host.request_restore(&target_tick.to_string());
            host.step(&mut HeadlessDriver::new());

            let next_tick = target_tick.saturating_add(1);
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
                let llm = resolve_llm_config(&cli, &cli.workspace)?;
                let cli_driver = CliApprovalDriver::new(host.vocab(), HeadlessDriver::new())
                    .with_policy(policy.clone());
                let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
                    .with_mode(WorkspaceMode::Build)
                    .with_workspace_vocab(ws_vocab)
                    .sync_existing_undos(host.graph())
                    .with_checkpoints(checkpoints)
                    .require_read_before_edit(true)
                    .wrapping(cli_driver);
                let mut driver =
                    RecordingBridge::new(host.vocab(), build_sync_responder(&llm, None))
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
                    "\n[openOnyx rewind@{target_tick}] agent={} state={:?} result:\n{}",
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
        Mode::Help | Mode::Version => unreachable!(),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use onyx_soul_agents::{AgentHost, AgentState, IngressEvent, OnyxEvent, Reply};
    use onyx_workspace::{graph_workspace_mode, latest_undoable_effect};
    use ops::render_inspect;
    use session::{
        SURFACE_APPROVAL, SURFACE_PLAN, active_surfaces, all_live_waiting_approval, build_host,
    };

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
        assert!(
            surfaces.iter().any(|s| s.surface == SURFACE_PLAN),
            "plan surface must be declared"
        );
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

    #[test]
    fn parse_args_supports_record_snapshot_controls_inspect_flags_and_rejects_unknown_flags() {
        let parsed = parse_args_from(
            [
                "chat",
                "--record",
                "session.tr",
                "--snapshot-dir",
                "snaps_out",
                "--snapshot-every",
                "10",
                "--max-snapshots",
                "5",
                "--ignore-deny",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap();
        assert_eq!(parsed.save_transcript, Some(PathBuf::from("session.tr")));
        assert_eq!(parsed.snapshot_dir, Some(PathBuf::from("snaps_out")));
        assert_eq!(parsed.snapshot_every, Some(10));
        assert_eq!(parsed.max_snapshots, Some(5));
        assert!(parsed.ignore_deny);

        let inspect_parsed = parse_args_from(
            ["inspect", "snaps_out", "--list", "--json", "--tick", "4"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        match inspect_parsed.mode {
            Mode::Inspect {
                dir,
                tick,
                list,
                json,
            } => {
                assert_eq!(dir, PathBuf::from("snaps_out"));
                assert_eq!(tick, Some(4));
                assert!(list);
                assert!(json);
            }
            _ => panic!("expected Mode::Inspect"),
        }

        let err_zero = parse_args_from(
            ["chat", "--max-snapshots", "0"]
                .into_iter()
                .map(str::to_string),
        )
        .err()
        .expect("0 max-snapshots must be rejected");
        assert!(err_zero.contains("--max-snapshots"), "{err_zero}");

        let err_unknown =
            parse_args_from(["chat", "--unknown-flag"].into_iter().map(str::to_string))
                .err()
                .expect("unknown flag must be rejected");
        assert!(err_unknown.contains("--unknown-flag"), "{err_unknown}");
    }

    #[test]
    fn milestone_pack_snapshots_support_non_contiguous_inspect_rewind_and_doctor() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-milestone-pack-{}", std::process::id()));
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

        let snap_dir = dir.join("snaps_pack");
        let opts = SessionOptions {
            snapshot_dir: Some(snap_dir.clone()),
            save_transcript: None,
            save_step_snapshots: true,
            snapshot_every: None,
            max_snapshots: None,
        };
        let checkpoints = PendingCheckpoints::default();
        let mode_handle = WorkspaceModeHandle::new(WorkspaceMode::Build);
        let spawner = CommandSpawnerSystem::new(true)
            .with_max_steps(16)
            .with_mode_handle(mode_handle.clone());
        let (mut host, ws_vocab) = build_host_with_options(
            1,
            AgentConfig {
                default_max_steps: 16,
                default_timeout_ticks: 64,
                ..AgentConfig::default()
            },
            &opts,
            Some(Box::new(spawner)),
            Some(checkpoints.clone()),
        )
        .unwrap();

        let tools = WorkspaceTools::new(WorkspacePolicy::new(&dir).unwrap());
        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode_handle(mode_handle)
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
            _ => None,
        })
        .wrapping(ws_bridge);

        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "edit note".into(),
        )));
        // Tick 0 processes IngressEvent::Command -> emits ev_spawn; Tick 1 spawns agent.
        host.step(&mut driver);
        host.step(&mut driver);
        run_session(
            &mut host,
            &mut driver,
            &opts,
            StopPolicy::Settled { max_ticks: 32 },
        )
        .unwrap();

        // Commands on subsequent ticks trigger cmd milestones too.
        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "/mode plan".into(),
        )));
        host.step(&mut driver);
        host.push(OnyxEvent::Ingress(IngressEvent::Command("/undo".into())));
        host.step(&mut driver);

        let pack_file = snap_dir.join(pack::PACK_FILE_NAME);
        assert!(pack_file.exists(), "snapshots.onyxpack must be written");
        let legacy_bins: Vec<_> = fs::read_dir(&snap_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bin"))
            .collect();
        assert!(
            legacy_bins.is_empty(),
            "must not emit tick-*.bin files: {legacy_bins:?}"
        );

        let store = PackSnapshotStore::new(&snap_dir);
        let ticks = store.ticks();
        assert!(
            ticks.len() < host.ticks() as usize,
            "milestone snapshots ({:?}) should be strictly fewer than total ticks ({})",
            ticks,
            host.ticks()
        );
        assert!(
            !ticks.contains(&2),
            "non-milestone Acting tick 2 must not be stored: {ticks:?}"
        );
        let tags: Vec<String> = store.entries().into_iter().map(|m| m.tag).collect();
        assert!(tags.iter().any(|t| t == "init"), "tags={tags:?}");
        assert!(tags.iter().any(|t| t == "spawn"), "tags={tags:?}");
        assert!(tags.iter().any(|t| t == "tool:fs_write"), "tags={tags:?}");
        assert!(tags.iter().any(|t| t == "settled"), "tags={tags:?}");
        assert!(tags.iter().any(|t| t == "cmd:/mode plan"), "tags={tags:?}");
        assert!(tags.iter().any(|t| t == "cmd:/undo"), "tags={tags:?}");

        // Verify non-contiguous rewind floor resolution.
        let tool_meta = store
            .entries()
            .into_iter()
            .find(|m| m.tag == "tool:fs_write")
            .unwrap();
        let floor = store.resolve_tick_or_floor(tool_meta.tick).unwrap();
        assert_eq!(floor, tool_meta.tick);
        assert_eq!(store.resolve_tick_or_floor(2), Some(1));
        let missing_err = render_inspect(
            &snap_dir,
            Some(999_999),
            false,
            false,
            AgentConfig::default(),
        )
        .expect_err("missing tick must fail with helpful error");
        assert!(missing_err.to_string().contains("--list"), "{missing_err}");

        // Verify `inspect --list --json`.
        let list_json_str =
            render_inspect(&snap_dir, None, true, true, AgentConfig::default()).unwrap();
        let list_json: serde_json::Value = serde_json::from_str(&list_json_str).unwrap();
        let list_arr = list_json.as_array().unwrap();
        assert_eq!(list_arr.len(), ticks.len());
        assert!(
            list_arr.iter().any(|item| item["tag"] == "tool:fs_write"
                && item["git_checkpoint"]["tree_sha"].is_string()),
            "{list_json_str}"
        );

        // Verify `inspect --tick <tool_tick> --json`.
        let snap_json_str = render_inspect(
            &snap_dir,
            Some(tool_meta.tick),
            false,
            true,
            AgentConfig::default(),
        )
        .unwrap();
        let snap_json: serde_json::Value = serde_json::from_str(&snap_json_str).unwrap();
        assert_eq!(snap_json["tick"], tool_meta.tick);
        assert_eq!(snap_json["tag"], "tool:fs_write");
        assert!(!snap_json["agents"].as_array().unwrap().is_empty());
        assert!(!snap_json["checkpoints"].as_array().unwrap().is_empty());
        assert!(!snap_json["concepts"].as_array().unwrap().is_empty());

        // Verify `doctor` validates the pack file cleanly with 0 errors.
        let report = run_doctor(&dir, Some(&snap_dir), None);
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_args_supports_provider_model_and_api_base() {
        let parsed = parse_args_from(
            [
                "chat",
                "--provider",
                "openai",
                "--model",
                "qwen2.5-coder:14b",
                "--api-base",
                "http://localhost:11434/v1",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap();
        assert_eq!(parsed.provider, Some(LlmProvider::OpenAi));
        assert_eq!(parsed.model.as_deref(), Some("qwen2.5-coder:14b"));
        assert_eq!(
            parsed.api_base.as_deref(),
            Some("http://localhost:11434/v1")
        );

        let err = parse_args_from(
            ["run", "hello", "--provider", "unknown"]
                .into_iter()
                .map(str::to_string),
        )
        .err()
        .expect("unknown provider must fail");
        assert!(err.contains("unsupported provider"), "{err}");
    }

    #[test]
    fn parse_args_supports_help_and_version_cleanly() {
        for args in [
            vec!["--help"],
            vec!["-h"],
            vec!["help"],
            vec!["chat", "--help"],
            vec!["inspect", "--help"],
        ] {
            let parsed = parse_args_from(args.into_iter().map(str::to_string)).unwrap();
            assert!(matches!(parsed.mode, Mode::Help));
        }

        for args in [
            vec!["--version"],
            vec!["-V"],
            vec!["version"],
            vec!["chat", "-V"],
        ] {
            let parsed = parse_args_from(args.into_iter().map(str::to_string)).unwrap();
            assert!(matches!(parsed.mode, Mode::Version));
        }

        let help = help_text();
        for needle in [
            "SUBCOMMANDS:",
            "chat",
            "run",
            "replay",
            "inspect",
            "rewind",
            "doctor",
            "OPTIONS:",
            "--provider",
            "--model",
            "--api-base",
            "--snapshot-dir",
            "--record",
            "--resume",
            "TUI CONTROLS & SLASH COMMANDS",
            "/undo",
            "/compact",
            "/mode",
            "/new",
            "Tab",
            "PgUp",
            "PROJECT INSTRUCTIONS, SKILLS & LIVE PLAN:",
            "SKILL.md",
            "load_skill",
            "Live Plan",
            "ENVIRONMENT VARIABLES:",
            "GEMINI_API_KEY",
            "OPENAI_API_KEY",
            "ONYX_PROVIDER",
            "OPENAI_API_BASE",
        ] {
            assert!(help.contains(needle), "help text missing `{needle}`");
        }
    }

    #[test]
    fn compact_command_creates_milestone_reduces_context_fill_and_replays_cleanly() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-compact-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let big_body = "let line = 1234567890;\n".repeat(1200);
        fs::write(dir.join("big.txt"), &big_body).unwrap();

        let snap_dir = dir.join("snaps_compact");
        let opts = SessionOptions {
            snapshot_dir: Some(snap_dir.clone()),
            save_transcript: None,
            save_step_snapshots: true,
            snapshot_every: None,
            max_snapshots: None,
        };
        let checkpoints = PendingCheckpoints::default();
        let mode_handle = WorkspaceModeHandle::new(WorkspaceMode::Build);
        let spawner = CommandSpawnerSystem::new(true)
            .with_max_steps(16)
            .with_mode_handle(mode_handle.clone());
        let (mut host, ws_vocab) = build_host_with_options(
            1,
            AgentConfig {
                default_max_steps: 16,
                default_timeout_ticks: 64,
                ..AgentConfig::default()
            },
            &opts,
            Some(Box::new(spawner)),
            Some(checkpoints.clone()),
        )
        .unwrap();

        let tools = WorkspaceTools::new(WorkspacePolicy::new(&dir).unwrap());
        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
            .with_mode_handle(mode_handle)
            .with_workspace_vocab(ws_vocab)
            .with_checkpoints(checkpoints)
            .wrapping(HeadlessDriver::new());
        let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| match req.request {
            1 => Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_read",
                r#"{"path":"big.txt"}"#,
            )),
            2 => Some(Reply::done(req.agent, req.request, "analyzed big.txt")),
            3 => Some(Reply::done(
                req.agent,
                req.request,
                "continued after compact",
            )),
            _ => None,
        })
        .wrapping(ws_bridge);

        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "analyze big.txt".into(),
        )));
        host.step(&mut driver);
        host.step(&mut driver);
        run_session(
            &mut host,
            &mut driver,
            &opts,
            StopPolicy::Settled { max_ticks: 32 },
        )
        .unwrap();

        let a1 = host.agents()[0];
        let view_before = host.view(a1);
        let ctx_cfg = TieredContextConfig {
            model_context_tokens: 10_000,
            ..TieredContextConfig::default()
        };
        let proj_before = context::project_memory(&view_before.goal, &view_before.memory, &ctx_cfg);
        let telemetry = SessionTelemetryHandle::new("gpt-4o", false);
        let mut tel_before = context::SessionTelemetry::new("gpt-4o", false);
        tel_before.model_context_tokens = ctx_cfg.model_context_tokens;
        tel_before.update_estimated_context_bytes(proj_before.total_bytes);
        telemetry.sync_estimated_context_bytes(proj_before.total_bytes);

        // Execute `/compact keep auth details`
        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "/compact keep auth details".into(),
        )));
        host.step(&mut driver);

        let view_after = host.view(a1);
        let proj_after = context::project_memory(&view_after.goal, &view_after.memory, &ctx_cfg);
        assert_eq!(
            proj_after.highest_tier,
            context::ToolOutputTier::Tier2Compacted
        );
        assert!(
            proj_after.total_bytes < proj_before.total_bytes / 4,
            "expected sharp drop: before={} after={}",
            proj_before.total_bytes,
            proj_after.total_bytes
        );

        let mut tel_after = tel_before.clone();
        tel_after.update_estimated_context_bytes(proj_after.total_bytes);
        assert!(
            tel_after.context_fill_pct < tel_before.context_fill_pct,
            "context_fill_pct must drop after /compact: before={} after={}",
            tel_before.context_fill_pct,
            tel_after.context_fill_pct
        );

        let first_turn = match &proj_after.turns[0] {
            context::ProjectedTurn::User(s) => s.as_str(),
            other => panic!("expected User session_summary turn, got {other:?}"),
        };
        assert!(first_turn.contains("[session_summary]"), "{first_turn}");
        assert!(
            first_turn.contains("focus: keep auth details"),
            "{first_turn}"
        );
        assert!(first_turn.contains("big.txt"), "{first_turn}");

        // Milestone `cmd:/compact` exists in `snapshots.onyxpack`, with pre-compact ticks preserved.
        let store = PackSnapshotStore::new(&snap_dir);
        let tags: Vec<String> = store.entries().into_iter().map(|m| m.tag).collect();
        assert!(tags.iter().any(|t| t == "cmd:/compact"), "tags={tags:?}");
        assert!(tags.iter().any(|t| t == "tool:fs_read"), "tags={tags:?}");

        // Continue with a follow-up turn after compaction and verify ONYXTR2 replay compatibility (TEST-01.4).
        host.push(OnyxEvent::Ingress(IngressEvent::Command(
            "next step".into(),
        )));
        run_session(
            &mut host,
            &mut driver,
            &opts,
            StopPolicy::Settled { max_ticks: 32 },
        )
        .unwrap();
        assert_eq!(host.result(a1).as_deref(), Some("continued after compact"));

        let mut replay_tr = driver.transcript().clone();
        replay_tr.commands = vec![
            onyx_soul_agents::LoggedCommand {
                poll: 0,
                command: "analyze big.txt".to_string(),
            },
            onyx_soul_agents::LoggedCommand {
                poll: 5,
                command: "/compact keep auth details".to_string(),
            },
            onyx_soul_agents::LoggedCommand {
                poll: 6,
                command: "next step".to_string(),
            },
        ];
        let tr_bytes = replay_tr.to_bytes();
        let decoded = Transcript::from_bytes(&tr_bytes).unwrap();
        let (mut replay_host, _) = build_host(
            1,
            AgentConfig {
                default_max_steps: 16,
                default_timeout_ticks: 64,
                ..AgentConfig::default()
            },
            None,
            Some(Box::new(CommandSpawnerSystem::new(true).with_max_steps(16))),
        )
        .unwrap();
        let mut replay_driver = ReplayBridge::new(decoded).wrapping(HeadlessDriver::new());
        run_session(
            &mut replay_host,
            &mut replay_driver,
            &SessionOptions::default(),
            StopPolicy::ReplayExhausted { max_ticks: 64 },
        )
        .unwrap();
        let ra1 = replay_host.agents()[0];
        assert_eq!(
            replay_host.result(ra1).as_deref(),
            Some("continued after compact")
        );
        assert!(
            replay_host
                .view(ra1)
                .memory
                .iter()
                .any(|(k, t)| k == "system" && t.starts_with(context::COMPACT_MARKER))
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
