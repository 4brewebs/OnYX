mod command;
mod context;
mod gemini;
mod openai;
mod ops;
mod pack;
mod session;
mod tui;
mod tui_input;
mod vault;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::{Duration, SystemTime};

use command::{CommandSpawnerSystem, PendingSessionAction, PendingSystemNotes};
use context::{ReasoningEffort, SessionTelemetryHandle, TieredContextConfig};
use gemini::{
    AsyncGeminiDriver, DEFAULT_GEMINI_MODEL, GeminiResponder, StreamPreviewHandle,
    load_project_instructions,
};
use onyx_soul_agents::{
    AgentConfig, AgentView, HeadlessDriver, RecordingBridge, ReplayBridge, Responder, Transcript,
};
use onyx_workspace::{
    NetworkPolicy, PendingCheckpoints, SandboxMode, WorkspaceMode, WorkspaceModeHandle,
    WorkspacePolicy, WorkspaceToolBridge, WorkspaceTools, graph_workspace_mode,
};
use openai::{AsyncOpenAiDriver, DEFAULT_OPENAI_MODEL, OpenAiResponder};
use ops::{reconcile_rewind_effects, run_doctor, run_inspect};
use pack::{
    PackSnapshotStore, SharedPackStore, default_fork_dir, fork_pack_until_tick,
    truncate_pack_until_tick,
};
use session::{
    CliApprovalDriver, SessionActionContext, SessionOptions, StopPolicy,
    build_host_with_checkpoints, build_host_with_options, build_host_with_shared_store, make_spec,
    run_session,
};
use tui::TuiDriver;
use vault::{
    auto_save_from_env, ensure_vault_gitignore, resolve_fork_dir, resolve_read_snapshot_dir,
    resolve_read_transcript, resolve_write_storage, session_display_label,
};

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
    effort: Option<ReasoningEffort>,
    api_base: Option<String>,
    project_instructions: String,
}

struct CliArgs {
    mode: Mode,
    workspace: PathBuf,
    provider: Option<LlmProvider>,
    model: Option<String>,
    effort: Option<ReasoningEffort>,
    api_base: Option<String>,
    auto_approve: bool,
    ignore_deny: bool,
    sandbox_mode: Option<SandboxMode>,
    sandbox_network: Option<NetworkPolicy>,
    dry_run: bool,
    max_steps: Option<i64>,
    shell_timeout_secs: Option<u64>,
    save_transcript: Option<PathBuf>,
    snapshot_dir: Option<PathBuf>,
    auto_save: bool,
    resume_dir: Option<PathBuf>,
    resume_tick: Option<u64>,
    fork_dir: Option<PathBuf>,
    truncate_future: bool,
    interactive: bool,
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
            effort: None,
            api_base: None,
            auto_approve: false,
            ignore_deny: false,
            sandbox_mode: None,
            sandbox_network: None,
            dry_run: false,
            max_steps: None,
            shell_timeout_secs: None,
            save_transcript: None,
            snapshot_dir: None,
            auto_save: false,
            resume_dir: None,
            resume_tick: None,
            fork_dir: None,
            truncate_future: false,
            interactive: false,
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
    let mut effort: Option<ReasoningEffort> = None;
    let mut api_base: Option<String> = None;
    let mut auto_approve = false;
    let mut ignore_deny = false;
    let mut sandbox_mode: Option<SandboxMode> = None;
    let mut sandbox_network: Option<NetworkPolicy> = None;
    let mut dry_run = false;
    let mut max_steps: Option<i64> = None;
    let mut shell_timeout_secs: Option<u64> = None;
    let mut save_transcript = None;
    let mut snapshot_dir = None;
    let mut auto_save = false;
    let mut resume_dir = None;
    let mut fork_dir = None;
    let mut truncate_future = false;
    let mut interactive = false;
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
            "--effort" => {
                let e = args
                    .next()
                    .ok_or("--effort requires `low`, `medium`, or `high`")?;
                effort = Some(ReasoningEffort::parse(&e)?);
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
            "--sandbox" => {
                let s = args
                    .next()
                    .ok_or("--sandbox requires `off`, `workspace-write`, or `strict-container`")?;
                let parsed = SandboxMode::parse(&s).ok_or_else(|| {
                    format!(
                        "unsupported --sandbox mode `{s}` (expected `off`, `workspace-write`, or `strict-container`)"
                    )
                })?;
                sandbox_mode = Some(parsed);
            }
            "--sandbox-net" => {
                let s = args
                    .next()
                    .ok_or("--sandbox-net requires `offline`, `localhost`, or `unrestricted`")?;
                let parsed = NetworkPolicy::parse(&s).ok_or_else(|| {
                    format!(
                        "unsupported --sandbox-net policy `{s}` (expected `offline`, `localhost`, or `unrestricted`)"
                    )
                })?;
                sandbox_network = Some(parsed);
            }
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
            "--auto-save" => auto_save = true,
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
            "--fork-dir" => {
                let p = args.next().ok_or("--fork-dir requires a directory path")?;
                fork_dir = Some(PathBuf::from(p));
            }
            "--truncate-future" => truncate_future = true,
            "--interactive" | "--tui" => interactive = true,
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
    let resume_tick = if matches!(mode, Mode::Chat) {
        tick_opt
    } else {
        None
    };

    Ok(CliArgs {
        mode,
        workspace,
        provider,
        model,
        effort,
        api_base,
        auto_approve,
        ignore_deny,
        sandbox_mode,
        sandbox_network,
        dry_run,
        max_steps,
        shell_timeout_secs,
        save_transcript,
        snapshot_dir,
        auto_save,
        resume_dir,
        resume_tick,
        fork_dir,
        truncate_future,
        interactive,
        snapshot_every,
        max_snapshots,
    })
}

fn usage() -> String {
    "Usage:\n  openonyx chat [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--max-steps <n>] [--shell-timeout <secs>] [--auto-save] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>] [--resume <dir> [--tick <n>] [--fork-dir <dir>] [--truncate-future]]\n  openonyx run <goal> [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--max-steps <n>] [--shell-timeout <secs>] [--auto-save] [--save-transcript <file> | --record <file>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]\n  openonyx replay <transcript> [--workspace <dir>] [--snapshot-dir <dir>] [--snapshot-every <n>] [--max-snapshots <n>]\n  openonyx inspect <snapshot_dir> [--tick <n>] [--list] [--json]\n  openonyx rewind <snapshot_dir> <tick> [--transcript <file>] [--interactive | --tui] [--fork-dir <dir>] [--truncate-future] [--workspace <dir>] [--provider <gemini|openai>] [--model <name>] [--effort <low|medium|high>] [--api-base <url>] [--sandbox <off|workspace-write|strict-container>] [--sandbox-net <offline|localhost|unrestricted>] [--auto-approve] [--ignore-deny] [--dry-run]\n  openonyx doctor [--workspace <dir>] [--snapshot-dir <dir>] [--transcript <file>]\n  openonyx [--help | -h | --version | -V]".to_string()
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
      --effort <level>     Reasoning/thinking effort: `low`, `medium`, or `high` (or `ONYX_EFFORT`)
      --api-base <url>     Custom API base URL (e.g. `http://localhost:11434/v1`; no OPENAI_API_KEY needed when set)
      --auto-approve       Skip interactive approval prompts for mutating tools (`fs_write`, `fs_edit`, `shell_exec`; `deny` rules in `.onyx/config.toml` still apply and sandbox elevates to `workspace-write` + `offline` unless `--sandbox off`)
      --ignore-deny        Explicitly bypass `[permissions].deny` rules loaded from `.onyx/config.toml`
      --sandbox <mode>     OS sandbox profile for `shell_exec`: `workspace-write` (default), `strict-container`, or `off`
      --sandbox-net <pol>  Network policy inside sandbox: `offline` (default), `localhost`, or `unrestricted`
      --max-steps <n>      Max agent steps per turn (default: 128 in `chat`, 64 in `run`)
      --shell-timeout <s>  Timeout in seconds per `shell_exec` command
      --auto-save          Auto-save snapshots + ONYXTR2 transcript together in `.onyx/vault/<timestamp>_<model>[_<branch|goal>]/` (or `ONYX_AUTO_SAVE=1`)
      --save-transcript <f>, --record <f>
                           Record deterministic `ONYXTR2` transcript (short names go inside `--snapshot-dir`, or `.onyx/vault/unassigned/` if no snapshot dir)
      --snapshot-dir <dir> Save compressed milestone snapshots to `<dir>/snapshots.onyxpack` (short names resolve inside `.onyx/vault/<dir>/`)
      --snapshot-every <n> Also capture periodic snapshots every <n> ticks
      --max-snapshots <n>  Opt-in sliding window retention (keeps `init` + latest `n - 1` milestones)
  -r, --resume <dir>       Resume `chat` from the latest snapshot (or `--tick <n>`) stored in `<dir>`
      --tick <n>           Specific tick to inspect (`inspect`) or resume/fork from (`chat --resume`)
      --fork-dir <dir>     Destination snapshot directory when forking via `chat --resume --tick` or `rewind --interactive`
      --truncate-future    Opt-in: overwrite the source pack in place instead of creating a sibling fork directory
      --interactive, --tui Open interactive TUI after `rewind <dir> <tick>` instead of running headless
  -l, --list               List all stored snapshot milestones with tags and git checkpoints (use with `inspect`)
      --json               Emit machine-readable JSON output (use with `inspect`)
      --transcript <file>  Replay transcript from rewind point (`rewind`) or validate transcript (`doctor`)
      --dry-run            Preview orphaned effects and git rollback plan in `rewind` without modifying files
  -h, --help               Print this help guide and exit 0
  -V, --version            Print openOnyx version and exit 0

TUI CONTROLS & SLASH COMMANDS (chat):
  Enter                    Submit goal, follow-up prompt (queues while busy), or slash command
  /  /  @                  Open live autocomplete popup for slash commands (`/`) or workspace files (`@`)
  Left / Right / Home / End / Delete
                           Edit prompt text with a visible cursor
  Alt+Up / Alt+Down (Ctrl+P / Ctrl+N)
                           Browse prompt input history (preserving unsent draft)
  Ctrl+X                   Clear queued follow-up prompts
  Tab                      Toggle [PLAN] (read-only) and [BUILD] (edits enabled)
  F2                       Open/close interactive Agent Selector popup (focus root or subagents)
  Ctrl+Tab                 Quick-cycle focused agent (optional terminal-dependent alias for F2 / /agent)
  y / n                    Approve or deny a pending mutating tool diff
  Up / Down / PgUp / PgDn / Home / End / Ctrl+U / Ctrl+D
                           Scroll the `Memory & Trace` history panel
  /agent [id|name]         Open agent selector or focus a specific agent by ID or name
  /undo                    Revert the latest reversible git checkpoint (also removes newly created untracked `fs_write` files)
  /rewind [tick]           List milestones or rewind in-place to `<tick>` (forks snapshots safely + restores git tree)
  /fork [name]             Branch snapshots up to the current tick into a sibling directory without mutating the original
  /compact [focus]         Compact older turns into a verified session summary (creating a `cmd:/compact` milestone)
  /services                Show @TOON v1 status of local processes & containers from `sprout.toml`
  /service <start|stop|logs> <id|all> [tail]
                           Control Sprout services interactively from the TUI
  /mode [plan|build|explore]
                           Switch or toggle workspace mode
  /new [goal]              Start a fresh agent conversation (optionally passing an immediate <goal>)
  Esc / Ctrl+C             Close popup or quit cleanly (flushing exit snapshot & transcript if configured)

PROJECT INSTRUCTIONS, SKILLS & SIDE PANELS:
  ONYX.md / AGENTS.md      Loaded from ~/.onyx/ and the workspace root into system instructions
  .onyx/skills/<name>/SKILL.md (also skills/, ~/.onyx/skills/)
                           Discovered as a compact catalog and loaded on demand via `load_skill` (includes `scent` & `rust`)
  Live Plan & Services     Right-hand panels (`plan_update` + `sprout.toml`) shown automatically when width >= 100 columns
  User Manual              Full guide available in `docs/manual/index.md`
  Session Vault            `.onyx/vault/<session>/` stores snapshots + `.tr` together (`.onyx/vault/unassigned/` for standalone `.tr`; auto-ignored by Git)

ENVIRONMENT VARIABLES:
  GEMINI_API_KEY, OPENAI_API_KEY, ONYX_API_KEY
  ONYX_PROVIDER            Default provider (`gemini` | `openai`)
  ONYX_MODEL               Default model override (`ONYX_GEMINI_MODEL`, `OPENAI_MODEL` as fallbacks)
  ONYX_EFFORT              Default reasoning/thinking effort (`low` | `medium` | `high`; fallbacks: `GEMINI_THINKING_LEVEL`, `OPENAI_REASONING_EFFORT`)
  ONYX_AUTO_SAVE           Enable `--auto-save` by default (`1` | `true` | `yes` | `on`)
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
    let effort = match cli.effort {
        Some(e) => Some(e),
        None => {
            let env_val = env::var("ONYX_EFFORT")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| match provider {
                    LlmProvider::Gemini => env::var("GEMINI_THINKING_LEVEL")
                        .ok()
                        .filter(|s| !s.trim().is_empty()),
                    LlmProvider::OpenAi => env::var("OPENAI_REASONING_EFFORT")
                        .ok()
                        .filter(|s| !s.trim().is_empty()),
                });
            match env_val {
                Some(raw) => Some(ReasoningEffort::parse(&raw)?),
                None => None,
            }
        }
    };

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
                effort,
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
                effort,
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
                .with_context_config(ctx_cfg)
                .with_effort(llm.effort);
            if let Some(tel) = telemetry {
                r = r.with_telemetry(tel);
            }
            Box::new(r)
        }
        LlmProvider::OpenAi => {
            let mut r = OpenAiResponder::new(llm.api_key.clone(), llm.model.clone())
                .with_api_base(llm.api_base.clone())
                .with_project_instructions(llm.project_instructions.clone())
                .with_context_config(ctx_cfg)
                .with_effort(llm.effort);
            if let Some(tel) = telemetry {
                r = r.with_telemetry(tel);
            }
            Box::new(r)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatResumePlan {
    target_tick: u64,
    active_snapshot_dir: PathBuf,
    branched_from: Option<PathBuf>,
}

fn prepare_chat_resume(
    resume_path: &Path,
    requested_tick: Option<u64>,
    fork_dir: Option<&Path>,
    truncate_future: bool,
    workspace: &Path,
    config: &AgentConfig,
    explicit_snapshot_dir: Option<&Path>,
) -> Result<ChatResumePlan, Box<dyn std::error::Error>> {
    let store = PackSnapshotStore::new(resume_path);
    let latest = store
        .latest_tick()
        .ok_or_else(|| format!("no snapshots found in `{}`", resume_path.display()))?;
    let target_tick = match requested_tick {
        Some(req) => store.resolve_tick_or_floor(req).ok_or_else(|| {
            format!(
                "no snapshots found in `{}` at or before tick {req}",
                resume_path.display()
            )
        })?,
        None => latest,
    };
    if let Some(req) = requested_tick {
        if target_tick != req {
            println!(
                "[openOnyx resume] requested tick {req} is not a milestone; resolving to prior milestone tick {target_tick}"
            );
        }
    }

    let has_future = latest > target_tick;
    if has_future {
        reconcile_rewind_effects(workspace, resume_path, target_tick, config, false)?;
    }

    let (active_snapshot_dir, branched_from) = if let Some(fd) = fork_dir {
        let report = fork_pack_until_tick(resume_path, fd, target_tick)?;
        println!(
            "[openOnyx fork] copied {} milestone(s) (<= tick {}) into `{}` (original `{}` preserved)",
            report.copied_snapshots,
            report.resolved_tick,
            report.dest_dir.display(),
            report.source_dir.display()
        );
        (report.dest_dir, Some(resume_path.to_path_buf()))
    } else if has_future {
        if truncate_future {
            let report = truncate_pack_until_tick(resume_path, target_tick)?;
            println!(
                "[openOnyx resume --truncate-future] truncated `{}` to {} milestone(s) (<= tick {})",
                report.dest_dir.display(),
                report.copied_snapshots,
                report.resolved_tick
            );
            (resume_path.to_path_buf(), None)
        } else {
            let dest = default_fork_dir(resume_path, target_tick, None);
            let report = fork_pack_until_tick(resume_path, &dest, target_tick)?;
            println!(
                "[openOnyx fork] branched {} milestone(s) (<= tick {}) into `{}` (original `{}` preserved)",
                report.copied_snapshots,
                report.resolved_tick,
                report.dest_dir.display(),
                report.source_dir.display()
            );
            (report.dest_dir, Some(resume_path.to_path_buf()))
        }
    } else {
        (
            explicit_snapshot_dir.unwrap_or(resume_path).to_path_buf(),
            None,
        )
    };

    Ok(ChatResumePlan {
        target_tick,
        active_snapshot_dir,
        branched_from,
    })
}

fn print_vault_summary(
    workspace: &Path,
    opts: &SessionOptions,
    shared_store: Option<&SharedPackStore>,
) {
    let active_snap = shared_store
        .map(SharedPackStore::current_dir)
        .or_else(|| opts.snapshot_dir.clone());
    if active_snap.is_none() && opts.save_transcript.is_none() {
        return;
    }
    let mut parts = Vec::new();
    if let Some(ref dir) = active_snap {
        parts.push(format!("snapshots={}", dir.display()));
    }
    if let Some(ref tr) = opts.save_transcript {
        parts.push(format!("transcript={}", tr.display()));
    }
    println!("[openOnyx vault] {}", parts.join(" · "));
    if let Some(ref dir) = active_snap {
        if let Some(label) = session_display_label(workspace, dir) {
            if dir.starts_with(vault::vault_root(workspace)) {
                println!("[openOnyx vault] resume with: openonyx chat --resume {label}");
            }
        }
    }
}

fn launch_interactive_chat(
    cli: &CliArgs,
    policy: &WorkspacePolicy,
    tools: WorkspaceTools,
    base_config: &AgentConfig,
    base_opts: &SessionOptions,
    resume_override: Option<(&Path, Option<u64>)>,
) -> Result<(), Box<dyn std::error::Error>> {
    let llm = resolve_llm_config(cli, &cli.workspace)?;
    let checkpoints = PendingCheckpoints::default();
    let chat_config = AgentConfig {
        default_max_steps: cli.max_steps.unwrap_or(128),
        default_timeout_ticks: 512,
        ..base_config.clone()
    };

    let resolved_fork_dir = cli
        .fork_dir
        .as_deref()
        .map(|fd| resolve_fork_dir(&cli.workspace, fd));
    let explicit_snap_raw =
        if resume_override.is_some() && cli.resume_dir.as_deref() == cli.snapshot_dir.as_deref() {
            None
        } else {
            cli.snapshot_dir.as_deref()
        };
    let explicit_snap_resolved = explicit_snap_raw.map(|d| {
        if vault::is_explicit_path(d) {
            d.to_path_buf()
        } else {
            vault::vault_root(&cli.workspace).join(d)
        }
    });

    let mut effective_opts = base_opts.clone();
    let resume_plan = match resume_override {
        Some((raw_dir, tick)) => {
            let resolved_dir = resolve_read_snapshot_dir(&cli.workspace, raw_dir);
            if resolved_fork_dir
                .as_deref()
                .is_some_and(|p| p.starts_with(vault::vault_root(&cli.workspace)))
                || resolved_dir.starts_with(vault::vault_root(&cli.workspace))
            {
                ensure_vault_gitignore(&cli.workspace)?;
            }
            let plan = prepare_chat_resume(
                &resolved_dir,
                tick,
                resolved_fork_dir.as_deref(),
                cli.truncate_future,
                &cli.workspace,
                &chat_config,
                explicit_snap_resolved.as_deref(),
            )?;
            effective_opts.snapshot_dir = Some(plan.active_snapshot_dir.clone());
            effective_opts.save_step_snapshots = true;
            Some(plan)
        }
        None => None,
    };

    let auto_save = cli.auto_save || auto_save_from_env();
    let resolved_storage = resolve_write_storage(
        &cli.workspace,
        if resume_plan.is_some() {
            None
        } else {
            cli.snapshot_dir.as_deref()
        },
        cli.save_transcript.as_deref(),
        resume_plan
            .as_ref()
            .map(|p| p.active_snapshot_dir.as_path()),
        auto_save,
        Some(&llm.model),
        None,
        SystemTime::now(),
    );
    if resolved_storage.uses_vault {
        ensure_vault_gitignore(&cli.workspace)?;
    }
    effective_opts.snapshot_dir = resolved_storage.snapshot_dir.clone();
    effective_opts.save_transcript = resolved_storage.save_transcript.clone();
    effective_opts.save_step_snapshots = effective_opts.snapshot_dir.is_some();

    let shared_store = effective_opts
        .snapshot_dir
        .as_ref()
        .map(|d| SharedPackStore::new(d, effective_opts.max_snapshots));
    let pending_action = PendingSessionAction::new();
    let pending_notes = PendingSystemNotes::new();
    let mode_handle = WorkspaceModeHandle::new(WorkspaceMode::Build);
    let mut spawner = CommandSpawnerSystem::new(cli.auto_approve)
        .with_max_steps(chat_config.default_max_steps)
        .with_mode_handle(mode_handle.clone())
        .with_sprout(tools.sprout().clone())
        .with_pending_action(pending_action.clone())
        .with_pending_notes(pending_notes.clone());
    if let Some(store) = shared_store.clone() {
        spawner = spawner.with_snapshot_store(store);
    }

    let (mut host, ws_vocab) = build_host_with_shared_store(
        1,
        chat_config.clone(),
        &effective_opts,
        Some(Box::new(spawner)),
        Some(checkpoints.clone()),
        shared_store.clone(),
    )?;

    if let Some(plan) = &resume_plan {
        host.request_restore(&plan.target_tick.to_string());
        host.step(&mut HeadlessDriver::new());
        host.set_tick(plan.target_tick.saturating_add(1));
        if let Some(restored_mode) = graph_workspace_mode(host.graph(), &ws_vocab) {
            mode_handle.set(restored_mode);
        }
        if let Some(src) = &plan.branched_from {
            pending_notes.push(format!(
                "[resume] Restored milestone tick {} into branch `{}` (source `{}` preserved).",
                plan.target_tick,
                plan.active_snapshot_dir.display(),
                src.display()
            ));
        }
    }

    let stream_preview = StreamPreviewHandle::new();
    let ctx_cfg = TieredContextConfig::for_model(&llm.model);
    let telemetry = SessionTelemetryHandle::new(&llm.model, llm.api_base.is_some());

    let tui = TuiDriver::new(host.vocab())?
        .with_workspace(cli.workspace.clone(), mode_handle.clone())
        .with_policy(policy.clone())
        .with_sprout(tools.sprout().clone())
        .with_workspace_vocab(ws_vocab)
        .with_stream_preview(stream_preview.clone())
        .with_telemetry(telemetry.clone())
        .with_context_config(ctx_cfg)
        .with_model_info(llm.model.clone(), llm.effort)
        .with_session_storage(resolved_storage.session_label.clone(), shared_store.clone());
    let tui_handle = tui.handle();
    let quit = tui.quit_flag();
    let bridge_ready = Arc::new(AtomicBool::new(false));
    let action_ctx = SessionActionContext {
        workspace_root: cli.workspace.clone(),
        config: chat_config,
        ws_vocab,
        mode_handle: mode_handle.clone(),
        tools: tools.clone(),
        snapshot_store: shared_store.clone(),
        pending_action,
        pending_notes,
    };
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
            .with_effort(llm.effort)
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
            .with_effort(llm.effort)
            .wrapping(ws_bridge),
        ),
    };

    run_session(
        &mut host,
        &mut driver,
        &effective_opts,
        StopPolicy::Quit {
            quit,
            tui: tui_handle,
            llm_in_flight,
            llm_ready,
            bridge_ready,
            action_ctx: Some(action_ctx),
        },
    )?;
    drop(driver);
    print_vault_summary(&cli.workspace, &effective_opts, shared_store.as_ref());
    Ok(())
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
    if let Some(sm) = cli.sandbox_mode {
        policy.sandbox.mode = sm;
    }
    if let Some(sn) = cli.sandbox_network {
        policy.sandbox.network = sn;
    }
    if cli.auto_approve {
        policy
            .sandbox
            .elevate_for_auto_approve(cli.sandbox_mode.is_some());
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
            let llm = resolve_llm_config(&cli, &cli.workspace)?;
            let auto_save = cli.auto_save || auto_save_from_env();
            let resolved = resolve_write_storage(
                &cli.workspace,
                cli.snapshot_dir.as_deref(),
                cli.save_transcript.as_deref(),
                None,
                auto_save,
                Some(&llm.model),
                Some(goal),
                SystemTime::now(),
            );
            if resolved.uses_vault {
                ensure_vault_gitignore(&cli.workspace)?;
            }
            let run_opts = SessionOptions {
                snapshot_dir: resolved.snapshot_dir,
                save_transcript: resolved.save_transcript,
                save_step_snapshots: false,
                ..session_opts
            };
            let run_opts = SessionOptions {
                save_step_snapshots: run_opts.snapshot_dir.is_some(),
                ..run_opts
            };

            let checkpoints = PendingCheckpoints::default();
            let (mut host, ws_vocab) =
                build_host_with_options(1, config, &run_opts, None, Some(checkpoints.clone()))?;

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
                &run_opts,
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
            print_vault_summary(&cli.workspace, &run_opts, None);
        }
        Mode::Chat => {
            let resume_arg = cli.resume_dir.as_deref().map(|dir| (dir, cli.resume_tick));
            launch_interactive_chat(&cli, &policy, tools, &config, &session_opts, resume_arg)?;
        }
        Mode::Replay(path) => {
            let resolved_tr = resolve_read_transcript(&cli.workspace, path);
            let resolved_out = resolve_write_storage(
                &cli.workspace,
                cli.snapshot_dir.as_deref(),
                None,
                None,
                false,
                None,
                None,
                SystemTime::now(),
            );
            if resolved_out.uses_vault {
                ensure_vault_gitignore(&cli.workspace)?;
            }
            let replay_opts = SessionOptions {
                save_step_snapshots: resolved_out.snapshot_dir.is_some(),
                snapshot_dir: resolved_out.snapshot_dir,
                save_transcript: None,
                ..session_opts
            };

            let bytes = fs::read(&resolved_tr)?;
            let transcript = Transcript::from_bytes(&bytes).map_err(|e| e.to_string())?;
            let has_commands = !transcript.commands.is_empty();
            let replay_config = AgentConfig {
                default_max_steps: cli.max_steps.unwrap_or(128),
                default_timeout_ticks: 512,
                ..config
            };
            let replay_spawner = CommandSpawnerSystem::new(cli.auto_approve)
                .with_max_steps(replay_config.default_max_steps);

            let (mut host, _ws_vocab) = build_host_with_options(
                1,
                replay_config,
                &replay_opts,
                Some(Box::new(replay_spawner)),
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
                &replay_opts,
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
            let resolved_dir = resolve_read_snapshot_dir(&cli.workspace, dir);
            run_inspect(&resolved_dir, *tick, *list, *json, config)?;
        }
        Mode::Rewind {
            dir,
            tick,
            transcript,
        } => {
            let resolved_dir = resolve_read_snapshot_dir(&cli.workspace, dir);
            if cli.interactive && !cli.dry_run && transcript.is_none() {
                launch_interactive_chat(
                    &cli,
                    &policy,
                    tools,
                    &config,
                    &session_opts,
                    Some((resolved_dir.as_path(), Some(*tick))),
                )?;
                return Ok(());
            }
            let store = PackSnapshotStore::new(&resolved_dir);
            let target_tick = store.resolve_tick_or_floor(*tick).ok_or_else(|| {
                format!(
                    "no snapshots found in `{}` at or before tick {tick}",
                    resolved_dir.display()
                )
            })?;
            if target_tick != *tick {
                println!(
                    "[openOnyx rewind] requested tick {tick} is not a milestone; resolving to prior milestone tick {target_tick}"
                );
            }
            reconcile_rewind_effects(
                &cli.workspace,
                &resolved_dir,
                target_tick,
                &config,
                cli.dry_run,
            )?;
            if cli.dry_run {
                return Ok(());
            }

            let checkpoints = PendingCheckpoints::default();
            let spawner = CommandSpawnerSystem::new(cli.auto_approve).with_max_steps(128);
            let (mut host, ws_vocab) = build_host_with_checkpoints(
                1,
                config,
                Some(&resolved_dir),
                Some(Box::new(spawner)),
                Some(checkpoints.clone()),
            )?;
            host.request_restore(&target_tick.to_string());
            host.step(&mut HeadlessDriver::new());

            let next_tick = target_tick.saturating_add(1);
            host.set_tick(next_tick);

            let rewind_opts = SessionOptions::default();
            if let Some(tr_path) = transcript {
                let resolved_tr = resolve_read_transcript(&cli.workspace, tr_path);
                let bytes = fs::read(&resolved_tr)?;
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
            let resolved_dir = snapshot_dir
                .as_ref()
                .map(|d| resolve_read_snapshot_dir(&cli.workspace, d));
            let resolved_tr = transcript
                .as_ref()
                .map(|t| resolve_read_transcript(&cli.workspace, t));
            let report = run_doctor(&cli.workspace, resolved_dir.as_ref(), resolved_tr.as_ref());
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
            report.checks.iter().any(|c| {
                c.starts_with("sandbox driver: ")
                    && ["bwrap", "landlock", "sandbox-exec", "win32-job", "none"]
                        .iter()
                        .any(|drv| c.contains(drv))
            }),
            "expected sandbox driver report in doctor checks: {:?}",
            report.checks
        );
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
                "--sandbox",
                "workspace-write",
                "--sandbox-net",
                "localhost",
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
        assert_eq!(parsed.sandbox_mode, Some(SandboxMode::WorkspaceWrite));
        assert_eq!(parsed.sandbox_network, Some(NetworkPolicy::LocalhostOnly));

        let err_bad_sandbox = parse_args_from(
            ["chat", "--sandbox", "bogus"]
                .into_iter()
                .map(str::to_string),
        )
        .err()
        .expect("invalid --sandbox must be rejected");
        assert!(err_bad_sandbox.contains("--sandbox"), "{err_bad_sandbox}");

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
            "--effort",
            "--api-base",
            "--snapshot-dir",
            "--auto-save",
            "--record",
            "--resume",
            "--fork-dir",
            "--interactive",
            "TUI CONTROLS & SLASH COMMANDS",
            "F2",
            "Ctrl+Tab",
            "/agent",
            "/undo",
            "/rewind",
            "/fork",
            "/compact",
            "/mode",
            "/new",
            "Tab",
            "PgUp",
            "PROJECT INSTRUCTIONS, SKILLS & SIDE PANELS:",
            "SKILL.md",
            "load_skill",
            "Live Plan",
            "docs/manual/index.md",
            ".onyx/vault/",
            "ENVIRONMENT VARIABLES:",
            "GEMINI_API_KEY",
            "OPENAI_API_KEY",
            "ONYX_PROVIDER",
            "ONYX_EFFORT",
            "ONYX_AUTO_SAVE",
            "OPENAI_API_BASE",
        ] {
            assert!(help.contains(needle), "help text missing `{needle}`");
        }

        let auto_chat =
            parse_args_from(["chat", "--auto-save"].into_iter().map(str::to_string)).unwrap();
        assert!(auto_chat.auto_save);
        let auto_run = parse_args_from(
            ["run", "check crates", "--auto-save"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert!(auto_run.auto_save);
        assert!(matches!(auto_run.mode, Mode::Run(ref g) if g == "check crates"));
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

    #[test]
    fn sprout_tui_panel_slash_commands_and_doctor_validation() {
        // TEST-08.5: slash command parsing for `/services` and `/service ...` (including plural alias).
        assert_eq!(
            command::parse_slash_command("/services"),
            Some(command::SlashCommand::Services)
        );
        assert_eq!(
            command::parse_slash_command("/service"),
            Some(command::SlashCommand::Services)
        );
        assert_eq!(
            command::parse_slash_command("/service start postgres"),
            Some(command::SlashCommand::Service("start postgres".to_string()))
        );
        assert_eq!(
            command::parse_slash_command("/services  start all"),
            Some(command::SlashCommand::Service("start all".to_string()))
        );
        assert_eq!(
            command::parse_slash_command("/service stop all"),
            Some(command::SlashCommand::Service("stop all".to_string()))
        );

        // Styled lines for `Services (Sprout)` TUI panel.
        let panel_text = "[READY] postgres (container :5432)\n[STARTING] orders-api (process :8080)\n[FAILED] worker (process)\n[OFF] redis (container :6379)";
        let styled = tui::render_services_lines(panel_text);
        assert_eq!(styled.len(), 4);

        // Doctor audit of `sprout.toml`: valid config + occupied port warning + cycle error.
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-sprout-doctor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_port = listener.local_addr().unwrap().port();
        fs::write(
            dir.join("sprout.toml"),
            format!(
                r#"
[services.api]
kind = "process"
cmd = "echo api"
ready_port = {occupied_port}
"#
            ),
        )
        .unwrap();

        let report_ok = run_doctor(&dir, None, None);
        assert!(report_ok.errors.is_empty(), "{:?}", report_ok.errors);
        assert!(
            report_ok
                .checks
                .iter()
                .any(|c| c.contains("sprout.toml valid") && c.contains("DAG acyclic")),
            "{:?}",
            report_ok.checks
        );
        assert!(
            report_ok.warnings.iter().any(|w| {
                w.contains(&occupied_port.to_string()) && w.contains("already in use")
            }),
            "expected occupied port warning: {:?}",
            report_ok.warnings
        );
        drop(listener);

        // Cyclic `sprout.toml` must surface as an error in `doctor`.
        fs::write(
            dir.join("sprout.toml"),
            r#"
[services.a]
cmd = "echo a"
depends_on = ["b"]

[services.b]
cmd = "echo b"
depends_on = ["a"]
"#,
        )
        .unwrap();
        let report_bad = run_doctor(&dir, None, None);
        assert!(
            report_bad
                .errors
                .iter()
                .any(|e| e.contains("sprout.toml") && e.contains("cycle")),
            "expected cycle error in doctor report: {:?}",
            report_bad.errors
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn services_command_before_first_spawn_replays_with_identical_concept_ids() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-sprout-replay-id-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("sprout.toml"),
            r#"
[services.bus]
kind = "process"
cmd = "echo READY_BUS"
ready_log = "READY_BUS"
"#,
        )
        .unwrap();

        let policy = WorkspacePolicy::new(&dir).unwrap();
        let tools = WorkspaceTools::new(policy);
        let sprout = tools.sprout().clone();
        let mode_handle = WorkspaceModeHandle::new(WorkspaceMode::Build);

        let spawner = CommandSpawnerSystem::new(true)
            .with_mode_handle(mode_handle.clone())
            .with_sprout(sprout);
        let (mut live_host, live_ws_vocab) =
            build_host(1, AgentConfig::default(), None, Some(Box::new(spawner))).unwrap();

        let mut headless = HeadlessDriver::new();
        headless.push_event(OnyxEvent::Ingress(IngressEvent::Command(
            "/services".to_string(),
        )));
        let handle = headless.handle();
        let ws_bridge = WorkspaceToolBridge::new(live_host.vocab(), tools)
            .with_mode_handle(mode_handle)
            .with_workspace_vocab(live_ws_vocab)
            .wrapping(headless);
        let mut driver = RecordingBridge::new(live_host.vocab(), |req: &AgentView| {
            Some(Reply::done(req.agent, req.request, "all good"))
        })
        .wrapping(ws_bridge);

        // Tick 1: process `/services` BEFORE any agent exists.
        live_host.step(&mut driver);
        assert!(live_host.agents().is_empty());

        // Tick 2+: send a regular prompt so an agent spawns AFTER `/services`.
        handle.push_event(OnyxEvent::Ingress(IngressEvent::Command(
            "check status".to_string(),
        )));
        live_host.step(&mut driver);
        live_host.step(&mut driver);
        assert!(live_host.run_until_settled(&mut driver, 16));
        assert_eq!(live_host.agents().len(), 1);
        let live_agent_id = live_host.agents()[0];
        assert_eq!(live_host.agent_state(live_agent_id), Some(AgentState::Done));

        // Now replay OFFLINE (`sprout = None`) with the recorded `ONYXTR2` transcript.
        let tr_bytes = driver.transcript().to_bytes();
        assert!(tr_bytes.starts_with(b"ONYXTR2\n"));
        let decoded = Transcript::from_bytes(&tr_bytes).unwrap();

        let replay_mode = WorkspaceModeHandle::new(WorkspaceMode::Build);
        let replay_spawner = CommandSpawnerSystem::new(true).with_mode_handle(replay_mode);
        let (mut replay_host, _) = build_host(
            1,
            AgentConfig::default(),
            None,
            Some(Box::new(replay_spawner)),
        )
        .unwrap();
        let mut replay_driver = ReplayBridge::new(decoded).wrapping(HeadlessDriver::new());
        for _ in 0..3 {
            replay_host.step(&mut replay_driver);
        }
        assert!(replay_host.run_until_settled(&mut replay_driver, 24));
        assert_eq!(replay_host.agents(), vec![live_agent_id]);
        assert_eq!(
            replay_host.agent_state(live_agent_id),
            Some(AgentState::Done)
        );
        assert_eq!(
            replay_host.result(live_agent_id).as_deref(),
            Some("all good")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_spawner_blocks_duplicate_root_while_waiting_children_and_caps_subagent_steps() {
        use onyx_soul_agents::{ConceptId, GraphView};

        // Verify `--effort` CLI parsing and `/agent` slash command parsing.
        let cli = parse_args_from(
            ["chat", "--model", "qwen38-27b", "--effort", "high"]
                .into_iter()
                .map(String::from),
        )
        .unwrap();
        assert_eq!(cli.effort, Some(ReasoningEffort::High));
        assert!(
            parse_args_from(["chat", "--effort", "ultra"].into_iter().map(String::from)).is_err()
        );
        assert_eq!(
            command::parse_slash_command("/agent"),
            Some(command::SlashCommand::Agent(None))
        );
        assert_eq!(
            command::parse_slash_command("/agents auditor"),
            Some(command::SlashCommand::Agent(Some("auditor".to_string())))
        );

        // Root agent spawns a child subagent and enters `WaitingChildren`.
        let cfg = AgentConfig {
            default_max_steps: 128,
            default_timeout_ticks: 512,
            ..AgentConfig::default()
        };
        let spawner = CommandSpawnerSystem::new(true).with_max_steps(128);
        let (mut host, _) = build_host(1, cfg, None, Some(Box::new(spawner))).unwrap();

        let mut headless = HeadlessDriver::new();
        headless.push_event(OnyxEvent::Ingress(IngressEvent::Command(
            "audit repo".to_string(),
        )));
        let handle = headless.handle();
        let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
            if req.agent == ConceptId(1) && req.request == 1 {
                Some(Reply::call_structured(
                    req.agent,
                    req.request,
                    "spawn_agent",
                    r#"{"name":"auditor","goal":"inspect open-onyx"}"#,
                ))
            } else {
                // Keep child alive (pending) while testing follow-up input protection.
                None
            }
        })
        .wrapping(headless);

        for _ in 0..5 {
            host.step(&mut driver);
        }
        assert_eq!(host.agents().len(), 2, "expected root + 1 subagent");
        let root_id = host.agents()[0];
        let child_id = host.agents()[1];
        assert_eq!(host.agent_state(root_id), Some(AgentState::WaitingChildren));

        // Subagent max_steps must be capped at DEFAULT_SUBAGENT_MAX_STEPS (32), not 128.
        let v = host.vocab();
        assert_eq!(
            host.graph().get(child_id, v.max_steps),
            Some(onyx_soul_agents::Value::Int(32))
        );

        // Sending a follow-up user prompt while root is WaitingChildren must NOT spawn a 2nd root.
        handle.push_event(OnyxEvent::Ingress(IngressEvent::Command(
            "are you still working?".to_string(),
        )));
        host.step(&mut driver);
        host.step(&mut driver);
        assert_eq!(
            host.agents().len(),
            2,
            "follow-up while WaitingChildren must not spawn duplicate root agent"
        );
    }

    #[test]
    fn slash_commands_and_cli_flags_for_rewind_and_fork() {
        // TEST-03.3: slash command parsing for `/rewind`, `/rewind 15`, `/fork rama-2`, `/compact`, `/agent`.
        assert_eq!(
            command::parse_slash_command("/rewind"),
            Some(command::SlashCommand::Rewind(None))
        );
        assert_eq!(
            command::parse_slash_command("/rewind 15"),
            Some(command::SlashCommand::Rewind(Some(15)))
        );
        assert_eq!(
            command::parse_slash_command("/rewind nope"),
            Some(command::SlashCommand::RewindInvalid("nope".to_string()))
        );
        assert_eq!(
            command::parse_slash_command("/fork"),
            Some(command::SlashCommand::Fork(None))
        );
        assert_eq!(
            command::parse_slash_command("/fork rama-2"),
            Some(command::SlashCommand::Fork(Some("rama-2".to_string())))
        );
        assert_eq!(
            command::parse_slash_command("/compact enfoque"),
            Some(command::SlashCommand::Compact(Some("enfoque".to_string())))
        );
        assert_eq!(
            command::parse_slash_command("/agent 2"),
            Some(command::SlashCommand::Agent(Some("2".to_string())))
        );

        let chat_cli = parse_args_from(
            [
                "chat",
                "--resume",
                ".snaps",
                "--tick",
                "12",
                "--fork-dir",
                ".snaps_alt",
            ]
            .into_iter()
            .map(String::from),
        )
        .unwrap();
        assert!(matches!(chat_cli.mode, Mode::Chat));
        assert_eq!(chat_cli.resume_dir, Some(PathBuf::from(".snaps")));
        assert_eq!(chat_cli.resume_tick, Some(12));
        assert_eq!(chat_cli.fork_dir, Some(PathBuf::from(".snaps_alt")));
        assert!(!chat_cli.truncate_future);

        let rewind_cli = parse_args_from(
            [
                "rewind",
                ".snaps",
                "12",
                "--interactive",
                "--truncate-future",
            ]
            .into_iter()
            .map(String::from),
        )
        .unwrap();
        assert!(matches!(rewind_cli.mode, Mode::Rewind { tick: 12, .. }));
        assert!(rewind_cli.interactive);
        assert!(rewind_cli.truncate_future);
    }

    #[test]
    fn chat_resume_at_tick_and_live_fork_rewind_restore_git_and_preserve_origin() {
        use onyx_soul_agents::Graph;

        // TEST-03.2 + interactive `/fork` & `/rewind <tick>` verification.
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-sdd03-b1-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
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

        let snap_dir = dir.join("snaps_main");
        let fork_dir = dir.join("snaps_fork_a");
        let opts = SessionOptions {
            snapshot_dir: Some(snap_dir.clone()),
            save_transcript: None,
            save_step_snapshots: true,
            snapshot_every: None,
            max_snapshots: None,
        };
        let cfg = AgentConfig {
            default_max_steps: 16,
            default_timeout_ticks: 64,
            ..AgentConfig::default()
        };

        let policy = WorkspacePolicy::new(&dir).unwrap();
        let tools = WorkspaceTools::new(policy);
        let checkpoints = PendingCheckpoints::default();
        let mode_handle = WorkspaceModeHandle::new(WorkspaceMode::Build);
        let shared_store = SharedPackStore::new(&snap_dir, None);
        let pending_action = PendingSessionAction::new();
        let pending_notes = PendingSystemNotes::new();

        let spawner = CommandSpawnerSystem::new(true)
            .with_max_steps(16)
            .with_mode_handle(mode_handle.clone())
            .with_snapshot_store(shared_store.clone())
            .with_pending_action(pending_action.clone())
            .with_pending_notes(pending_notes.clone());
        let (mut host, ws_vocab) = build_host_with_shared_store(
            1,
            cfg.clone(),
            &opts,
            Some(Box::new(spawner)),
            Some(checkpoints.clone()),
            Some(shared_store.clone()),
        )
        .unwrap();

        let mut headless = HeadlessDriver::new();
        headless.push_event(OnyxEvent::Ingress(IngressEvent::Command(
            "edit note.txt".to_string(),
        )));
        let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools.clone())
            .with_mode_handle(mode_handle.clone())
            .with_workspace_vocab(ws_vocab)
            .with_checkpoints(checkpoints)
            .require_read_before_edit(true)
            .wrapping(headless);
        let mut step = 0usize;
        let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
            step += 1;
            match step {
                1 => Some(Reply::call_structured(
                    req.agent,
                    req.request,
                    "fs_read",
                    r#"{"path":"note.txt"}"#,
                )),
                2 => Some(Reply::call_structured(
                    req.agent,
                    req.request,
                    "fs_edit",
                    r#"{"path":"note.txt","old_text":"v1","new_text":"v2"}"#,
                )),
                _ => Some(Reply::done(req.agent, req.request, "updated to v2")),
            }
        })
        .wrapping(ws_bridge);

        host.step(&mut driver);
        host.step(&mut driver);
        assert!(host.run_until_settled(&mut driver, 32));
        assert_eq!(fs::read_to_string(dir.join("note.txt")).unwrap(), "v2\n");
        let _ = shared_store.save_milestone(
            host.ticks().saturating_sub(1),
            "settled",
            &host.graph().export(),
        );

        let orig_pack_bytes = fs::read(snap_dir.join(pack::PACK_FILE_NAME)).unwrap();
        let orig_store = PackSnapshotStore::new(&snap_dir);
        let pre_edit_tick = orig_store
            .entries()
            .into_iter()
            .find(|m| m.tag == "tool:fs_read")
            .map(|m| m.tick)
            .expect("expected tool:fs_read milestone before fs_edit");

        // 1. Verify `prepare_chat_resume` (TEST-03.2): reconciles git to v1, forks pack, preserves origin.
        let plan = prepare_chat_resume(
            &snap_dir,
            Some(pre_edit_tick),
            Some(&fork_dir),
            false,
            &dir,
            &cfg,
            None,
        )
        .unwrap();
        assert_eq!(plan.target_tick, pre_edit_tick);
        assert_eq!(plan.active_snapshot_dir, fork_dir);
        assert_eq!(fs::read_to_string(dir.join("note.txt")).unwrap(), "v1\n");
        assert_eq!(
            fs::read(snap_dir.join(pack::PACK_FILE_NAME)).unwrap(),
            orig_pack_bytes
        );

        // Resume a fresh host from `fork_dir` at `target_tick + 1` and continue with a new LLM reply.
        let fork_opts = SessionOptions {
            snapshot_dir: Some(fork_dir.clone()),
            save_step_snapshots: true,
            ..SessionOptions::default()
        };
        let (mut resumed_host, _) = build_host_with_options(
            1,
            cfg.clone(),
            &fork_opts,
            Some(Box::new(CommandSpawnerSystem::new(true).with_max_steps(16))),
            Some(PendingCheckpoints::default()),
        )
        .unwrap();
        resumed_host.request_restore(&plan.target_tick.to_string());
        resumed_host.step(&mut HeadlessDriver::new());
        resumed_host.set_tick(plan.target_tick.saturating_add(1));
        assert_eq!(resumed_host.ticks(), pre_edit_tick + 1);

        let mut saw_request = false;
        let mut resumed_driver = RecordingBridge::new(resumed_host.vocab(), |req: &AgentView| {
            saw_request = true;
            Some(Reply::done(req.agent, req.request, "alternate branch done"))
        })
        .wrapping(HeadlessDriver::new());
        assert!(resumed_host.run_until_settled(&mut resumed_driver, 16));
        assert!(saw_request, "resumed host must dispatch new LLM turn");
        let root_agent = resumed_host.agents()[0];
        assert_eq!(
            resumed_host.result(root_agent).as_deref(),
            Some("alternate branch done")
        );

        // 2. Verify live `/fork` and `/rewind <tick>` via `apply_session_action` without TTY.
        // Restore note.txt to v2 so we prove live `/rewind` restores git back to v1.
        fs::write(dir.join("note.txt"), "v2\n").unwrap();
        let action_ctx = SessionActionContext {
            workspace_root: dir.clone(),
            config: cfg,
            ws_vocab,
            mode_handle,
            tools,
            snapshot_store: Some(shared_store.clone()),
            pending_action,
            pending_notes,
        };
        let fork_msg = session::apply_session_action(
            &mut host,
            command::SessionAction::Fork {
                label: Some("exp-live".to_string()),
            },
            &action_ctx,
        );
        assert!(fork_msg.contains("[/fork] Created branch"), "{fork_msg}");
        let branched_dir = shared_store.current_dir();
        assert_ne!(branched_dir, snap_dir);
        assert!(branched_dir.join(pack::PACK_FILE_NAME).exists());

        let rewind_msg = session::apply_session_action(
            &mut host,
            command::SessionAction::Rewind {
                requested_tick: pre_edit_tick,
            },
            &action_ctx,
        );
        assert!(
            rewind_msg.contains(&format!("Restored milestone tick {pre_edit_tick}")),
            "{rewind_msg}"
        );
        assert_eq!(host.ticks(), pre_edit_tick + 1);
        assert_eq!(fs::read_to_string(dir.join("note.txt")).unwrap(), "v1\n");
        let root_mem = host.view(host.agents()[0]).memory;
        assert!(
            root_mem
                .iter()
                .any(|(k, t)| k == "system" && t.contains("[/rewind] Restored milestone tick")),
            "{root_mem:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
