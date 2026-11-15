mod gemini;

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use gemini::{DEFAULT_GEMINI_MODEL, GeminiResponder};
use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSpec, HeadlessDriver, PlatformDriver, RecordingBridge,
    ReplayBridge, Reply, Transcript,
};
use onyx_workspace::{WorkspacePolicy, WorkspaceToolBridge, WorkspaceTools};

struct CliArgs {
    mode: Mode,
    workspace: PathBuf,
    auto_approve: bool,
    save_transcript: Option<PathBuf>,
}

enum Mode {
    Run(String),
    Replay(PathBuf),
}

fn parse_args() -> Result<CliArgs, String> {
    let mut args = env::args().skip(1);
    let Some(cmd) = args.next() else {
        return Err(usage());
    };
    let mut workspace = env::current_dir().map_err(|e| e.to_string())?;
    let mut auto_approve = false;
    let mut save_transcript = None;
    let mut positional = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" | "-w" => {
                let p = args.next().ok_or("--workspace requires a path")?;
                workspace = PathBuf::from(p);
            }
            "--auto-approve" => auto_approve = true,
            "--save-transcript" => {
                let p = args.next().ok_or("--save-transcript requires a path")?;
                save_transcript = Some(PathBuf::from(p));
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
        "replay" => {
            let p = positional.into_iter().next().ok_or("replay requires <transcript>")?;
            Mode::Replay(PathBuf::from(p))
        }
        _ => return Err(usage()),
    };

    Ok(CliArgs {
        mode,
        workspace,
        auto_approve,
        save_transcript,
    })
}

fn usage() -> String {
    "Usage:\n  openonyx run <goal> [--workspace <dir>] [--auto-approve] [--save-transcript <file>]\n  openonyx replay <transcript> [--workspace <dir>]".to_string()
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

fn drive_session(host: &mut AgentHost, driver: &mut dyn PlatformDriver, max_ticks: usize) {
    for _ in 0..max_ticks {
        host.step(driver);
        for view in host.pending_approvals() {
            if let Some(pending) = &view.pending_approval {
                let reply = if prompt_approval(&pending.tool, &pending.args) {
                    Reply::approve(view.agent, view.request)
                } else {
                    Reply::deny(view.agent, view.request, "denied by user")
                };
                host.push(reply.to_event());
            }
        }
        if host.is_settled() {
            break;
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

    let policy = WorkspacePolicy::new(&cli.workspace)?;
    let tools = WorkspaceTools::new(policy);

    let config = AgentConfig {
        default_max_steps: 32,
        default_timeout_ticks: 64,
        ..AgentConfig::default()
    };
    let mut host = AgentHost::builder(1).config(config).build()?;

    match cli.mode {
        Mode::Run(goal) => {
            let api_key = env::var("GEMINI_API_KEY")
                .map_err(|_| "GEMINI_API_KEY environment variable is not set")?;
            let model = env::var("ONYX_GEMINI_MODEL")
                .unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.to_string());

            let mut spec = AgentSpec::new("openonyx", goal)
                .with_tools(WorkspaceTools::all_tool_names().iter().copied());
            if !cli.auto_approve {
                spec = spec.with_approval_tools(
                    WorkspaceTools::approval_required_names().iter().copied(),
                );
            }
            host.spawn(&spec);

            let ws_bridge =
                WorkspaceToolBridge::new(host.vocab(), tools).wrapping(HeadlessDriver::new());
            let mut driver = RecordingBridge::new(
                host.vocab(),
                GeminiResponder::new(api_key, model),
            )
            .wrapping(ws_bridge);

            drive_session(&mut host, &mut driver, 256);

            if let Some(path) = cli.save_transcript {
                fs::write(path, driver.transcript().to_bytes())?;
            }
        }
        Mode::Replay(path) => {
            let bytes = fs::read(path)?;
            let transcript = Transcript::from_bytes(&bytes).map_err(|e| format!("{e}"))?;
            let mut spec = AgentSpec::new("openonyx", "replay")
                .with_tools(WorkspaceTools::all_tool_names().iter().copied());
            if !cli.auto_approve {
                spec = spec.with_approval_tools(
                    WorkspaceTools::approval_required_names().iter().copied(),
                );
            }
            host.spawn(&spec);

            let ws_bridge =
                WorkspaceToolBridge::new(host.vocab(), tools).wrapping(HeadlessDriver::new());
            let mut driver = ReplayBridge::new(transcript).wrapping(ws_bridge);
            drive_session(&mut host, &mut driver, 256);
        }
    }

    if let Some(id) = host.agents().first().copied() {
        println!(
            "\n[openOnyx] state={:?} result:\n{}",
            host.agent_state(id),
            host.result(id).unwrap_or_default()
        );
    }
    Ok(())
}
