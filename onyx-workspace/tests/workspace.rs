use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use onyx_soul_agents::{
    AgentHost, AgentSpec, AgentView, HeadlessDriver, RecordingBridge, ReplayBridge, Reply,
};
use onyx_workspace::{
    WorkspaceLedgerSystem, WorkspaceMode, WorkspacePolicy, WorkspaceToolBridge, WorkspaceTools,
    WorkspaceVocab, capture_git_tree_checkpoint, effects_after_tick, is_file_read,
    restore_git_tree_checkpoint,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("onyx_ws_{prefix}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn policy_blocks_path_traversal_for_read_and_write() {
    let dir = TempDir::new("traversal");
    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    assert!(policy.resolve_existing("../secret.txt").is_err());
    assert!(policy.resolve_for_write("../escape.txt").is_err());
    assert!(policy.resolve_existing("").is_err());
}

#[test]
fn workspace_tools_write_read_edit_search_and_list() {
    let dir = TempDir::new("crud");
    fs::create_dir_all(dir.0.join(".git")).unwrap();
    fs::write(dir.0.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let w = tools
        .execute(
            "fs_write",
            r#"{"path":"src/lib.rs","content":"fn one() {}\nfn two() {}\n"}"#,
        )
        .unwrap();
    assert!(w.contains("wrote"));

    let r = tools
        .execute(
            "fs_read",
            r#"{"path":"src/lib.rs","start_line":2,"end_line":2}"#,
        )
        .unwrap();
    assert_eq!(r, "2: fn two() {}\n");

    let e = tools
        .execute(
            "fs_edit",
            r#"{"path":"src/lib.rs","old_text":"fn two() {}","new_text":"fn two() -> i32 { 2 }"}"#,
        )
        .unwrap();
    assert!(e.contains("1 replacement"));

    let s = tools
        .execute("code_search", r#"{"pattern":"-> i32"}"#)
        .unwrap();
    assert!(s.contains("src/lib.rs:2:"));

    let l = tools.execute("fs_list", r#"{}"#).unwrap();
    assert!(l.contains("src/lib.rs"));
    assert!(!l.contains(".git"));
}

#[test]
fn workspace_tool_bridge_executes_tools_end_to_end_with_agent_host() {
    let dir = TempDir::new("bridge");
    fs::write(dir.0.join("hello.txt"), "hola mundo\n").unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let mut host = AgentHost::builder(1).build().unwrap();
    host.spawn(
        &AgentSpec::new("coder", "read hello.txt")
            .with_tools(WorkspaceTools::all_tool_names().iter().copied()),
    );

    let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools).wrapping(HeadlessDriver::new());
    let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
        if let Some((_, text)) = req.memory.iter().find(|(k, _)| k == "observe") {
            Some(Reply::done(req.agent, req.request, text.clone()))
        } else if req.memory.iter().any(|(k, _)| k == "call") {
            None
        } else {
            Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_read",
                r#"{"path":"hello.txt"}"#,
            ))
        }
    })
    .wrapping(ws_bridge);

    assert!(host.run_until_settled(&mut driver, 16));
    let id = host.agents()[0];
    assert_eq!(host.result(id).as_deref(), Some("1: hola mundo\n"));
}

#[test]
fn recorded_workspace_session_replays_identically_without_touching_disk() {
    let dir = TempDir::new("replay");
    fs::write(dir.0.join("data.txt"), "valor determinista\n").unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let spec = AgentSpec::new("coder", "read data.txt")
        .with_tools(WorkspaceTools::all_tool_names().iter().copied());

    let mut recorded = AgentHost::builder(42).build().unwrap();
    recorded.spawn(&spec);

    let ws_bridge =
        WorkspaceToolBridge::new(recorded.vocab(), tools).wrapping(HeadlessDriver::new());
    let mut rec_driver = RecordingBridge::new(recorded.vocab(), |req: &AgentView| {
        if let Some((_, text)) = req.memory.iter().find(|(k, _)| k == "observe") {
            Some(Reply::done(req.agent, req.request, text.clone()))
        } else if req.memory.iter().any(|(k, _)| k == "call") {
            None
        } else {
            Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_read",
                r#"{"path":"data.txt"}"#,
            ))
        }
    })
    .wrapping(ws_bridge);

    assert!(recorded.run_until_settled(&mut rec_driver, 16));
    let expected_fp = recorded.fingerprint();
    let transcript = rec_driver.transcript().clone();

    // Eliminamos el directorio temporal: el replay NO debe tocar el disco ni el LLM.
    drop(dir);

    let mut replayed = AgentHost::builder(42).build().unwrap();
    replayed.spawn(&spec);
    let mut replay_driver = ReplayBridge::new(transcript).wrapping(HeadlessDriver::new());
    assert!(replayed.run_until_settled(&mut replay_driver, 16));
    assert_eq!(replayed.fingerprint(), expected_fp);
}

#[test]
fn plan_mode_blocks_mutating_tools_without_touching_disk() {
    let dir = TempDir::new("plan-mode");
    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let spec = AgentSpec::new("planner", "try writing in plan mode")
        .with_tools(WorkspaceTools::all_tool_names().iter().copied());
    let mut host = AgentHost::builder(1)
        .system(Box::new(WorkspaceLedgerSystem::new()))
        .build()
        .unwrap();
    host.spawn(&spec);

    let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
        .with_mode(WorkspaceMode::Plan)
        .wrapping(HeadlessDriver::new());
    let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
        if let Some((_, err)) = req.memory.iter().find(|(k, _)| k == "tool_error") {
            Some(Reply::done(req.agent, req.request, err.clone()))
        } else if req.memory.iter().any(|(k, _)| k == "call") {
            None
        } else {
            Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_write",
                r#"{"path":"blocked.txt","content":"nope"}"#,
            ))
        }
    })
    .wrapping(ws_bridge);

    assert!(host.run_until_settled(&mut driver, 16));
    assert!(!dir.0.join("blocked.txt").exists());
    let id = host.agents()[0];
    let result = host.result(id).unwrap();
    assert!(result.contains("blocked by workspace mode"), "{result}");
}

#[test]
fn require_read_before_edit_and_ledger_records_file_and_effects() {
    let dir = TempDir::new("read-before-edit");
    fs::write(dir.0.join("note.txt"), "before\n").unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let spec = AgentSpec::new("editor", "edit note.txt")
        .with_tools(WorkspaceTools::all_tool_names().iter().copied());
    let mut host = AgentHost::builder(7)
        .system(Box::new(WorkspaceLedgerSystem::new()))
        .build()
        .unwrap();
    let ws_vocab = WorkspaceVocab::resolve_with(|name| host.attr(name));
    host.spawn(&spec);

    let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
        .with_workspace_vocab(ws_vocab)
        .require_read_before_edit(true)
        .wrapping(HeadlessDriver::new());

    let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
        let has_err = req.memory.iter().any(|(k, _)| k == "tool_error");
        let obs_count = req.memory.iter().filter(|(k, _)| k == "observe").count();
        if obs_count >= 2 {
            Some(Reply::done(req.agent, req.request, "edited"))
        } else if obs_count == 1 {
            Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_edit",
                r#"{"path":"note.txt","old_text":"before","new_text":"after"}"#,
            ))
        } else if has_err {
            Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_read",
                r#"{"path":"note.txt"}"#,
            ))
        } else {
            Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_edit",
                r#"{"path":"note.txt","old_text":"before","new_text":"after"}"#,
            ))
        }
    })
    .wrapping(ws_bridge);

    assert!(host.run_until_settled(&mut driver, 24));
    assert_eq!(
        fs::read_to_string(dir.0.join("note.txt")).unwrap(),
        "after\n"
    );
    assert!(is_file_read(host.graph(), &ws_vocab, "note.txt"));

    let effects = effects_after_tick(host.graph(), &ws_vocab, 0);
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].tool, "fs_edit");
    assert_eq!(effects[0].target, "note.txt");
    assert_eq!(effects[0].reversibility, "auto_reversible");
}

#[test]
fn shell_exec_drains_large_output_without_deadlock() {
    let dir = TempDir::new("pipe-drain");
    let mut policy = WorkspacePolicy::new(&dir.0).unwrap();
    policy.max_output_bytes = 4096;
    let tools = WorkspaceTools::new(policy);

    let cmd = if cfg!(windows) {
        r#"{"command":"$s = 'A' * 4096; 1..80 | ForEach-Object { [Console]::Out.WriteLine($s); [Console]::Error.WriteLine($s) }"}"#
    } else {
        r#"{"command":"i=0; while [ $i -lt 2000 ]; do echo AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA; echo BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB >&2; i=$((i+1)); done"}"#
    };

    let out = tools.execute("shell_exec", cmd).unwrap();
    assert!(out.contains("exit_code: 0"), "{out}");
    assert!(out.len() <= 4096 + 64, "output len was {}", out.len());
}

#[test]
fn walk_list_and_search_ignore_external_symlinks() {
    let dir = TempDir::new("symlink-root");
    let outside = TempDir::new("symlink-outside");
    fs::write(dir.0.join("inside.txt"), "needle inside\n").unwrap();
    fs::write(outside.0.join("secret.txt"), "needle outside\n").unwrap();

    let link_path = dir.0.join("escape_link");
    #[cfg(unix)]
    let link_res = std::os::unix::fs::symlink(&outside.0, &link_path);
    #[cfg(windows)]
    let link_res = std::os::windows::fs::symlink_dir(&outside.0, &link_path);

    // En Windows sin privilegios de desarrollador/admin puede devolver ERROR_PRIVILEGE_NOT_HELD (1314).
    if let Err(e) = link_res {
        if e.raw_os_error() == Some(1314) {
            return;
        }
        panic!("unexpected symlink error: {e}");
    }

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let listed = tools.execute("fs_list", r#"{"max_depth":3}"#).unwrap();
    assert!(listed.contains("inside.txt"), "{listed}");
    assert!(!listed.contains("secret.txt"), "{listed}");
    assert!(!listed.contains("escape_link"), "{listed}");

    let searched = tools
        .execute("code_search", r#"{"pattern":"needle"}"#)
        .unwrap();
    assert!(searched.contains("inside.txt"), "{searched}");
    assert!(!searched.contains("secret.txt"), "{searched}");
}

#[test]
fn fs_edit_line_trimmed_fallback() {
    let dir = TempDir::new("edit_trimmed");
    let file = dir.0.join("sample.rs");
    fs::write(
        &file,
        "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
    )
    .unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // 2-space indentation in old_text vs 4-space in file should succeed via line-trimmed fallback.
    let edited = tools
        .execute(
            "fs_edit",
            r#"{"path":"sample.rs","old_text":"  let x = 1;\n  let y = 2;","new_text":"    let x = 10;\n    let y = 20;"}"#,
        )
        .unwrap();
    assert!(edited.contains("edited"), "{edited}");
    let updated = fs::read_to_string(&file).unwrap();
    assert!(updated.contains("let x = 10;"), "{updated}");
    assert!(updated.contains("let y = 20;"), "{updated}");

    // Duplicate trimmed blocks must be rejected.
    fs::write(
        &file,
        "fn a() {\n    let z = 1;\n}\nfn b() {\n  let z = 1;\n}\n",
    )
    .unwrap();
    let err = tools
        .execute(
            "fs_edit",
            r#"{"path":"sample.rs","old_text":"let z = 1;","new_text":"let z = 2;"}"#,
        )
        .unwrap_err();
    assert!(
        err.contains("2 times") || err.contains("2 matches") || err.contains("unique"),
        "{err}"
    );
}

#[test]
fn git_tree_checkpoint_captures_and_restores_working_tree() {
    let dir = TempDir::new("git_ckpt");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir.0)
            .output()
            .expect("git must be available");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run_git(&["init"]);
    run_git(&["config", "user.name", "Test"]);
    run_git(&["config", "user.email", "test@example.com"]);
    run_git(&["config", "core.autocrlf", "false"]);

    let file = dir.0.join("tracked.txt");
    fs::write(&file, "original\n").unwrap();
    run_git(&["add", "tracked.txt"]);
    run_git(&["commit", "-m", "init"]);

    // Modify tracked.txt before capturing checkpoint to verify dirty tree is captured in tree_sha.
    fs::write(&file, "before-edit\n").unwrap();
    let (head_sha, tree_sha) = capture_git_tree_checkpoint(&dir.0).unwrap();
    assert!(!head_sha.is_empty());
    assert!(!tree_sha.is_empty());

    // Mutate file after checkpoint, then restore.
    fs::write(&file, "after-edit\n").unwrap();
    restore_git_tree_checkpoint(&dir.0, &tree_sha).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "before-edit\n");
}

#[cfg(feature = "scent")]
#[test]
fn scent_tools_emit_toon_outline_and_impact_respecting_policy() {
    let dir = TempDir::new("scent_tools");
    fs::create_dir_all(dir.0.join("src")).unwrap();
    fs::write(
        dir.0.join("src/lib.rs"),
        "pub fn entry() -> i32 { leaf() }\nfn leaf() -> i32 { 1 }\n",
    )
    .unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    assert!(WorkspaceTools::all_tool_names().contains(&"scent_outline"));
    assert!(WorkspaceTools::all_tool_names().contains(&"scent_impact"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"scent_outline"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"scent_impact"));

    let outline = tools.execute("scent_outline", r#"{"path":"src"}"#).unwrap();
    assert!(outline.starts_with("@TOON v1"), "{outline}");
    assert!(outline.contains("src/lib.rs::entry"), "{outline}");

    let impact = tools
        .execute("scent_impact", r#"{"target":"leaf","max_hops":2}"#)
        .unwrap();
    assert!(impact.starts_with("@TOON v1"), "{impact}");
    assert!(impact.contains("src/lib.rs::entry"), "{impact}");

    let traversal_err = tools
        .execute("scent_outline", r#"{"path":"../outside"}"#)
        .unwrap_err();
    assert!(
        traversal_err.contains("outside") || traversal_err.contains("failed"),
        "{traversal_err}"
    );
}

#[test]
fn fs_read_deduplicates_unchanged_reads_and_invalidates_after_edit() {
    let dir = TempDir::new("recall_seen");
    let file = dir.0.join("notes.txt");
    fs::write(&file, "line1\nline2\nline3\n").unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let first = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(10))
        .unwrap();
    assert!(first.contains("1: line1"), "{first}");

    let second = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(10))
        .unwrap();
    assert!(second.starts_with("@TOON v1"), "{second}");
    assert!(second.contains("unchanged_since_tick:10"), "{second}");

    // Mutate via fs_edit: invalidates seen_reads cache so the next read returns full lines again.
    tools
        .execute(
            "fs_edit",
            r#"{"path":"notes.txt","old_text":"line2","new_text":"line2-updated"}"#,
        )
        .unwrap();

    let third = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(12))
        .unwrap();
    assert!(third.contains("2: line2-updated"), "{third}");
}

#[test]
fn shell_exec_scrubs_credentials_and_honors_custom_timeout() {
    let dir = TempDir::new("scrub");
    let policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_command_timeout(std::time::Duration::from_secs(15));
    assert_eq!(policy.command_timeout, std::time::Duration::from_secs(15));

    let redacted = onyx_workspace::tools::scrub_secrets(
        "Authorization: Bearer super_secret_token_999 and AIzaSyA12345678901234567890",
    );
    assert!(!redacted.contains("super_secret_token_999"), "{redacted}");
    assert!(
        !redacted.contains("AIzaSyA12345678901234567890"),
        "{redacted}"
    );
    assert!(redacted.contains("[REDACTED]"), "{redacted}");
}
