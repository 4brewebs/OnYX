use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use onyx_soul_agents::{
    AgentHost, AgentSpec, AgentView, HeadlessDriver, RecordingBridge, ReplayBridge, Reply,
};
use onyx_workspace::{
    NetworkPolicy, PendingCheckpoints, PermissionDecision, PermissionRules, ProcessJob,
    ProcessLimits, SandboxConfig, SandboxMode, ServiceKind, SproutConfig, VerifyConfig, VerifyRule,
    WorkspaceLedgerSystem, WorkspaceMode, WorkspaceModeHandle, WorkspacePolicy,
    WorkspaceToolBridge, WorkspaceTools, WorkspaceVocab, capture_git_tree_checkpoint,
    detect_sandbox_capabilities, discover_skills_summary, effects_after_tick, graph_workspace_plan,
    graph_workspace_services, is_file_read, latest_undoable_effect, preview_tool_diff,
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
    fs::create_dir_all(dir.0.join("snaps_demo")).unwrap();
    fs::write(dir.0.join("snaps_demo/tick-0.bin"), b"ONYXAG").unwrap();
    fs::create_dir_all(dir.0.join("snapshots")).unwrap();
    fs::write(dir.0.join("snapshots/snapshots.onyxpack"), b"ONYXPK1").unwrap();
    fs::create_dir_all(dir.0.join("target/debug")).unwrap();
    fs::write(dir.0.join("target/debug/out.o"), b"").unwrap();
    fs::write(dir.0.join("session.tr"), b"ONYXTR2").unwrap();

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
    assert!(!l.contains("snaps_demo"));
    assert!(!l.contains("snapshots.onyxpack"));
    assert!(!l.contains("target"));
    assert!(!l.contains("session.tr"));
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
    // Unborn HEAD (fresh git init before first commit) must also capture and restore checkpoints.
    let unborn_dir = TempDir::new("git_ckpt_unborn");
    let unborn_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&unborn_dir.0)
            .output()
            .expect("git must be available");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    unborn_git(&["init"]);
    unborn_git(&["config", "core.autocrlf", "false"]);
    let unborn_file = unborn_dir.0.join("draft.txt");
    fs::write(&unborn_file, "initial-draft\n").unwrap();
    let (unborn_head, unborn_tree) =
        capture_git_tree_checkpoint(&unborn_dir.0).expect("unborn git repo must yield tree_sha");
    assert!(
        unborn_head.is_empty(),
        "expected empty head_sha, got {unborn_head}"
    );
    assert!(!unborn_tree.is_empty());

    fs::write(&unborn_file, "mutated-draft\n").unwrap();
    restore_git_tree_checkpoint(&unborn_dir.0, &unborn_tree).unwrap();
    assert_eq!(fs::read_to_string(&unborn_file).unwrap(), "initial-draft\n");

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

    // Modify tracked.txt and create an untracked file before capturing checkpoint.
    let untracked = dir.0.join("notes_untracked.md");
    fs::write(&file, "before-edit\n").unwrap();
    fs::write(&untracked, "v1\n").unwrap();
    let (head_sha, tree_sha) = capture_git_tree_checkpoint(&dir.0).unwrap();
    assert!(!head_sha.is_empty());
    assert!(!tree_sha.is_empty());

    // Mutate both files after checkpoint, then restore.
    fs::write(&file, "after-edit\n").unwrap();
    fs::write(&untracked, "v2\n").unwrap();
    restore_git_tree_checkpoint(&dir.0, &tree_sha).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "before-edit\n");
    assert_eq!(fs::read_to_string(&untracked).unwrap(), "v1\n");

    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_text.contains("?? notes_untracked.md"),
        "restore must keep untracked file unstaged: {status_text}"
    );
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
    assert!(WorkspaceTools::all_tool_names().contains(&"scent_references"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"scent_outline"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"scent_impact"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"scent_references"));

    let outline = tools.execute("scent_outline", r#"{"path":"src"}"#).unwrap();
    assert!(outline.starts_with("@TOON v1"), "{outline}");
    assert!(outline.contains("src/lib.rs::entry"), "{outline}");

    let impact = tools
        .execute("scent_impact", r#"{"target":"leaf","max_hops":2}"#)
        .unwrap();
    assert!(impact.starts_with("@TOON v1"), "{impact}");
    assert!(impact.contains("src/lib.rs::entry"), "{impact}");

    // `SDD-05` REQ-05.3: `scent_references` resuelve definición + referencias directas.
    let references = tools
        .execute("scent_references", r#"{"target":"leaf"}"#)
        .unwrap();
    assert!(references.starts_with("@TOON v1"), "{references}");
    assert!(
        references.contains("src/lib.rs::leaf | defined_at"),
        "{references}"
    );
    assert!(
        references.contains("src/lib.rs::leaf | called_by | src/lib.rs::entry"),
        "{references}"
    );

    // En el lado inverso: `entry` emite `calls` hacia `leaf`.
    let entry_refs = tools
        .execute("scent_references", r#"{"target":"entry"}"#)
        .unwrap();
    assert!(
        entry_refs.contains("src/lib.rs::entry | calls | src/lib.rs::leaf"),
        "{entry_refs}"
    );

    let unknown_err = tools
        .execute("scent_references", r#"{"target":"does_not_exist"}"#)
        .unwrap_err();
    assert!(
        unknown_err.contains("not found") || unknown_err.contains("does_not_exist"),
        "{unknown_err}"
    );

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
fn fs_read_dedup_expires_after_window_of_tool_executions() {
    let dir = TempDir::new("dedup_window");
    let file = dir.0.join("notes.txt");
    fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let first = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(1))
        .unwrap();
    assert!(first.contains("1: alpha"), "{first}");

    // `FS_READ_DEDUP_WINDOW` (8) ejecuciones de tool en medio: la dedup
    // expira y se devuelve el contenido completo (4C-audit).
    for _ in 0..8 {
        tools.execute("fs_list", "{}").unwrap();
    }
    let after_window = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(20))
        .unwrap();
    assert!(
        after_window.contains("1: alpha"),
        "tras 8 ejecuciones la re-lectura debe devolver el contenido completo: {after_window}"
    );
    assert!(!after_window.starts_with("@TOON v1"), "{after_window}");

    // Re-ancorada con contenido: la siguiente lectura (dentro de la ventana)
    // vuelve a stubear.
    let re_stub = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(21))
        .unwrap();
    assert!(re_stub.starts_with("@TOON v1"), "{re_stub}");
}

#[test]
fn fs_read_consecutive_stub_is_broken_by_full_content() {
    let dir = TempDir::new("dedup_stubbed");
    let file = dir.0.join("notes.txt");
    fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let first = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(1))
        .unwrap();
    assert!(first.contains("1: alpha"), "{first}");

    let second = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(2))
        .unwrap();
    assert!(second.starts_with("@TOON v1"), "{second}");

    // La lectura previa era un stub (el modelo no ha visto el contenido):
    // la siguiente debe devolver el contenido completo (4C-audit).
    let third = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(3))
        .unwrap();
    assert!(
        third.contains("1: alpha"),
        "un stub no debe encadenarse: {third}"
    );
    assert!(!third.starts_with("@TOON v1"), "{third}");

    // Re-ancorada con contenido: la siguiente sí puede stubear otra vez.
    let fourth = tools
        .execute_with_tick("fs_read", r#"{"path":"notes.txt"}"#, Some(4))
        .unwrap();
    assert!(fourth.starts_with("@TOON v1"), "{fourth}");
}

#[test]
fn fs_read_subset_stub_marks_covering_read_as_stubbed() {
    let dir = TempDir::new("dedup_subset");
    let mut content = String::new();
    for i in 1..=20 {
        content.push_str(&format!("fn f_{i}() {{ {i} }}\n"));
    }
    fs::write(dir.0.join("lib.rs"), content).unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    let full = tools
        .execute(
            "fs_read",
            r#"{"path":"lib.rs","start_line":1,"end_line":20}"#,
        )
        .unwrap();
    assert!(full.contains("fn f_10()"), "{full}");

    let subset = tools
        .execute(
            "fs_read",
            r#"{"path":"lib.rs","start_line":5,"end_line":12}"#,
        )
        .unwrap();
    assert!(
        subset.starts_with("@TOON v1") && subset.contains("covered_by:1-20"),
        "{subset}"
    );

    // La lectura cubridora (1-20) queda marcada como stub: una re-lectura
    // completa debe devolver el contenido (4C-audit).
    let full_again = tools
        .execute(
            "fs_read",
            r#"{"path":"lib.rs","start_line":1,"end_line":20}"#,
        )
        .unwrap();
    assert!(
        full_again.contains("fn f_10()"),
        "la lectura cubridora debe re-ancorarse con contenido: {full_again}"
    );
    assert!(!full_again.starts_with("@TOON v1"), "{full_again}");
}

#[test]
fn code_search_default_excludes_lockfiles_and_internal_dirs_at_root() {
    let dir = TempDir::new("search_excludes");
    fs::create_dir_all(dir.0.join(".onyx/skills")).unwrap();
    fs::create_dir_all(dir.0.join(".agents")).unwrap();
    fs::create_dir_all(dir.0.join("src")).unwrap();
    let marker = "NEEDLE_MARKER";
    fs::write(dir.0.join("Cargo.lock"), format!("{marker}\n")).unwrap();
    fs::write(dir.0.join("package-lock.json"), format!("{marker}\n")).unwrap();
    fs::write(dir.0.join("pnpm-lock.yaml"), format!("{marker}\n")).unwrap();
    fs::write(dir.0.join("yarn.lock"), format!("{marker}\n")).unwrap();
    fs::write(dir.0.join(".onyx/skills/SKILL.md"), format!("{marker}\n")).unwrap();
    fs::write(dir.0.join(".agents/AGENTS.md"), format!("{marker}\n")).unwrap();
    fs::write(dir.0.join("src/lib.rs"), format!("// {marker}\n")).unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // Sin `path` (raíz = workspace): exclusiones por defecto activas
    // (4C-audit).
    let out = tools
        .execute("code_search", r#"{"pattern":"NEEDLE_MARKER"}"#)
        .unwrap();
    assert!(out.contains("src/lib.rs"), "{out}");
    assert!(!out.contains("Cargo.lock"), "{out}");
    assert!(!out.contains("package-lock.json"), "{out}");
    assert!(!out.contains("pnpm-lock.yaml"), "{out}");
    assert!(!out.contains("yarn.lock"), "{out}");
    assert!(!out.contains("SKILL.md"), "{out}");
    assert!(!out.contains("AGENTS.md"), "{out}");
}

#[test]
fn code_search_explicit_path_bypasses_default_excludes() {
    let dir = TempDir::new("search_excl_path");
    fs::create_dir_all(dir.0.join(".onyx/skills")).unwrap();
    fs::create_dir_all(dir.0.join("src")).unwrap();
    let marker = "NEEDLE_MARKER";
    fs::write(dir.0.join(".onyx/skills/SKILL.md"), format!("{marker}\n")).unwrap();
    fs::write(dir.0.join("Cargo.lock"), format!("{marker}\n")).unwrap();
    fs::write(dir.0.join("src/lib.rs"), format!("// {marker}\n")).unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // Con `path` explícito se busca exactamente lo pedido (4C-audit).
    let in_onyx = tools
        .execute(
            "code_search",
            r#"{"pattern":"NEEDLE_MARKER","path":".onyx"}"#,
        )
        .unwrap();
    assert!(in_onyx.contains("SKILL.md"), "{in_onyx}");
    assert!(!in_onyx.contains("src/lib.rs"), "{in_onyx}");

    let in_lock = tools
        .execute(
            "code_search",
            r#"{"pattern":"NEEDLE_MARKER","path":"Cargo.lock"}"#,
        )
        .unwrap();
    assert!(in_lock.contains("Cargo.lock"), "{in_lock}");
}

#[test]
fn code_search_truncates_long_lines_and_reports_max_results() {
    let dir = TempDir::new("search_clip");
    let long_tail = "x".repeat(300);
    let line = format!("NEEDLE_CLIP {long_tail}");
    fs::write(dir.0.join("big.txt"), format!("{line}\n{line}\n")).unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // `max_results=1`: la línea coincide pero se recorta a 240 chars
    // (frontera UTF-8) + `...`, y el aviso accionable cierra la salida
    // (4C-audit). "NEEDLE_CLIP " = 12 chars -> 228 `x` visibles.
    let out = tools
        .execute(
            "code_search",
            r#"{"pattern":"NEEDLE_CLIP","max_results":1}"#,
        )
        .unwrap();
    let clipped_line = format!("big.txt:1: NEEDLE_CLIP {}...", "x".repeat(228));
    assert!(out.contains(&clipped_line), "{out}");
    assert!(
        !out.contains(&"x".repeat(229)),
        "la línea debe recortarse: {out}"
    );
    assert!(
        !out.contains("big.txt:2:"),
        "max_results=1: solo 1 match: {out}"
    );
    assert!(
        out.ends_with("# truncated: reached max_results (1); narrow with path or include_glob."),
        "{out}"
    );
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

#[test]
fn git_tools_mode_handle_diff_preview_and_undoable_effect_work() {
    let dir = TempDir::new("git_tools");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir.0)
            .output()
            .expect("git must execute");
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

    let sub = dir.0.join("src");
    fs::create_dir_all(&sub).unwrap();
    let file = sub.join("lib.rs");
    fs::write(&file, "pub fn a() -> i32 { 1 }\npub fn b() -> i32 { 2 }\n").unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "init"]);

    // Preview unified diff antes de aprobar `fs_edit` y `fs_write`.
    let edit_diff = preview_tool_diff(
        &dir.0,
        "fs_edit",
        r#"{"path":"src/lib.rs","old_text":"pub fn a() -> i32 { 1 }","new_text":"pub fn a() -> i32 { 10 }"}"#,
        14,
    )
    .unwrap();
    assert!(edit_diff.contains("--- a/src/lib.rs"), "{edit_diff}");
    assert!(
        edit_diff.contains("-pub fn a() -> i32 { 1 }"),
        "{edit_diff}"
    );
    assert!(
        edit_diff.contains("+pub fn a() -> i32 { 10 }"),
        "{edit_diff}"
    );

    let write_diff = preview_tool_diff(
        &dir.0,
        "fs_write",
        r#"{"path":"src/new.rs","content":"pub fn c() {}\n"}"#,
        14,
    )
    .unwrap();
    assert!(write_diff.contains("--- /dev/null"), "{write_diff}");
    assert!(write_diff.contains("+pub fn c() {}"), "{write_diff}");

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    assert!(WorkspaceTools::all_tool_names().contains(&"git_status"));
    assert!(WorkspaceTools::all_tool_names().contains(&"git_diff"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"git_status"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"git_diff"));

    // Modo Plan vía `WorkspaceModeHandle`: bloquea `fs_write` pero permite `git_status`/`git_diff`.
    let mode_handle = WorkspaceModeHandle::new(WorkspaceMode::Build);
    assert_eq!(mode_handle.toggle_plan_build(), WorkspaceMode::Plan);

    let checkpoints = PendingCheckpoints::default();
    let mut host = AgentHost::builder(1)
        .system(Box::new(
            WorkspaceLedgerSystem::new().with_checkpoints(checkpoints.clone()),
        ))
        .build()
        .unwrap();
    let ws_vocab = WorkspaceVocab::resolve_with(|name| host.attr(name));
    host.spawn(
        &AgentSpec::new("tester", "check git_status and toggle mode")
            .with_tools(WorkspaceTools::all_tool_names().iter().copied()),
    );

    let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools.clone())
        .with_mode_handle(mode_handle.clone())
        .with_workspace_vocab(ws_vocab)
        .with_checkpoints(checkpoints)
        .wrapping(HeadlessDriver::new());
    let mut step_turn = 0usize;
    let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
        step_turn += 1;
        match step_turn {
            1 => Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_write",
                r#"{"path":"src/lib.rs","content":"blocked\n"}"#,
            )),
            2 => {
                // Ahora alternamos a Build y llamamos `fs_edit`.
                mode_handle.toggle_plan_build();
                Some(Reply::call_structured(
                    req.agent,
                    req.request,
                    "fs_edit",
                    r#"{"path":"src/lib.rs","old_text":"pub fn a() -> i32 { 1 }","new_text":"pub fn a() -> i32 { 99 }"}"#,
                ))
            }
            3 => Some(Reply::call_structured(req.agent, req.request, "git_status", "{}")),
            4 => Some(Reply::call_structured(
                req.agent,
                req.request,
                "git_diff",
                r#"{"path":"src/lib.rs","context_lines":1}"#,
            )),
            _ => Some(Reply::done(req.agent, req.request, "ok")),
        }
    })
    .wrapping(ws_bridge);

    assert!(host.run_until_settled(&mut driver, 32));

    let view = host.view(host.agents()[0]);
    let joined_mem: String = view
        .memory
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined_mem.contains("tool_error:blocked by workspace mode `Plan`"),
        "{joined_mem}"
    );
    assert!(joined_mem.contains("M src/lib.rs"), "{joined_mem}");
    assert!(
        joined_mem.contains("+pub fn a() -> i32 { 99 }"),
        "{joined_mem}"
    );

    // Verifica traversal bloqueado en `git_diff`.
    let outside_err = tools
        .execute("git_diff", r#"{"path":"../outside"}"#)
        .unwrap_err();
    assert!(
        outside_err.contains("outside")
            || outside_err.contains("cannot resolve")
            || outside_err.contains("escapes"),
        "{outside_err}"
    );

    // `latest_undoable_effect` encuentra el checkpoint del `fs_edit`, y tras marcar undone ya no aparece.
    let eff = latest_undoable_effect(host.graph(), &ws_vocab).expect("must have undoable effect");
    assert_eq!(eff.tool, "fs_edit");
    assert_eq!(eff.target, "src/lib.rs");
    assert!(eff.tree_sha.is_some());
}

#[test]
fn multi_hunk_edit_reindentation_skills_and_plan_update_work() {
    let dir = TempDir::new("iter2_features");
    let src = dir.0.join("demo.rs");
    fs::write(
        &src,
        "fn main() {\n    let a = 1;\n    let b = 2;\n    println!(\"{} {}\", a, b);\n}\n",
    )
    .unwrap();

    // Custom workspace skill in `.onyx/skills/custom-stack/SKILL.md`
    fs::create_dir_all(dir.0.join(".onyx/skills/custom-stack")).unwrap();
    fs::write(
        dir.0.join(".onyx/skills/custom-stack/SKILL.md"),
        "---\ndescription: Custom test stack guide\n---\n# Custom Stack\nAlways validate schemas.\n",
    )
    .unwrap();

    let summary = discover_skills_summary(&dir.0);
    assert!(summary.contains("`scent`"), "{summary}");
    assert!(
        summary.contains("`custom-stack`: Custom test stack guide"),
        "{summary}"
    );

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // Relative re-indentation: 2-space old/new matched on 4-space block shifts result by +2 spaces.
    tools
        .execute(
            "fs_edit",
            r#"{"path":"demo.rs","old_text":"  let a = 1;","new_text":"  let a = 100;"}"#,
        )
        .unwrap();
    let after_reindent = fs::read_to_string(&src).unwrap();
    assert!(
        after_reindent.contains("    let a = 100;\n"),
        "{after_reindent}"
    );

    // Multi-hunk atomicity: second failing hunk must leave disk untouched.
    let err = tools
        .execute(
            "fs_edit",
            r#"{"path":"demo.rs","edits":[{"old_text":"let a = 100;","new_text":"let a = 200;"},{"old_text":"MISSING_LINE","new_text":"X"}]}"#,
        )
        .unwrap_err();
    assert!(err.contains("hunk 2"), "{err}");
    assert!(
        fs::read_to_string(&src).unwrap().contains("let a = 100;"),
        "disk must remain unchanged after failed multi-hunk edit"
    );

    // Valid multi-hunk diff preview + execution.
    let multi_args = r#"{"path":"demo.rs","edits":[{"old_text":"let a = 100;","new_text":"let a = 200;"},{"old_text":"let b = 2;","new_text":"let b = 300;"}]}"#;
    let diff = preview_tool_diff(&dir.0, "fs_edit", multi_args, 20).unwrap();
    assert!(diff.contains("+    let a = 200;"), "{diff}");
    assert!(diff.contains("+    let b = 300;"), "{diff}");
    let ok = tools.execute("fs_edit", multi_args).unwrap();
    assert!(ok.contains("2 replacement(s)"), "{ok}");
    let updated = fs::read_to_string(&src).unwrap();
    assert!(updated.contains("let a = 200;"));
    assert!(updated.contains("let b = 300;"));

    // `load_skill` loads built-in + custom workspace skill without approval.
    assert!(!WorkspaceTools::approval_required_names().contains(&"load_skill"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"plan_update"));
    let scent_skill = tools.execute("load_skill", r#"{"name":"scent"}"#).unwrap();
    assert!(scent_skill.contains("scent_outline"), "{scent_skill}");
    let custom_skill = tools
        .execute("load_skill", r#"{"name":"custom-stack"}"#)
        .unwrap();
    assert!(
        custom_skill.contains("Always validate schemas."),
        "{custom_skill}"
    );
    assert!(
        tools
            .execute("load_skill", r#"{"name":"../escape"}"#)
            .is_err()
    );

    // End-to-end `plan_update` + `load_skill` persisted through `AgentHost` & `WorkspaceLedgerSystem`.
    let mut host = AgentHost::builder(1)
        .system(Box::new(WorkspaceLedgerSystem::new()))
        .build()
        .unwrap();
    let ws_vocab = WorkspaceVocab::resolve_with(|name| host.attr(name));
    host.spawn(
        &AgentSpec::new("planner", "load skill and publish plan")
            .with_tools(WorkspaceTools::all_tool_names().iter().copied()),
    );
    let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
        .with_workspace_vocab(ws_vocab)
        .wrapping(HeadlessDriver::new());
    let mut turn = 0usize;
    let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
        turn += 1;
        match turn {
            1 => Some(Reply::call_structured(
                req.agent,
                req.request,
                "load_skill",
                r#"{"name":"scent"}"#,
            )),
            2 => Some(Reply::call_structured(
                req.agent,
                req.request,
                "plan_update",
                r#"{"explanation":"Refactor workflow","steps":[{"step":"Inspect AST","status":"completed"},{"step":"Apply multi-hunk edit","status":"in_progress"},{"step":"Run cargo test","status":"pending"}]}"#,
            )),
            _ => Some(Reply::done(req.agent, req.request, "planned")),
        }
    })
    .wrapping(ws_bridge);

    assert!(host.run_until_settled(&mut driver, 24));
    let plan = graph_workspace_plan(host.graph(), &ws_vocab).expect("plan must be persisted");
    assert!(plan.contains("Refactor workflow"), "{plan}");
    assert!(plan.contains("[x] Inspect AST"), "{plan}");
    assert!(plan.contains("[>] Apply multi-hunk edit"), "{plan}");
    assert!(plan.contains("[ ] Run cargo test"), "{plan}");
}

#[test]
fn code_search_v2_supports_regex_case_insensitive_globs_context_and_rejects_bad_regex() {
    let dir = TempDir::new("search_v2");
    fs::create_dir_all(dir.0.join("src")).unwrap();
    fs::create_dir_all(dir.0.join("docs")).unwrap();
    fs::write(
        dir.0.join("src/auth.rs"),
        "line1\nline2\npub fn Validate_Token(x: i32) -> bool {\n    let ok = x > 0;\n    ok\n}\nline7\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("src/ignore_me.rs"),
        "pub fn Validate_Token_ignored() {}\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("docs/notes.md"),
        "Validate_Token appears in markdown too\n",
    )
    .unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // TEST-02.1: regex + case_insensitive + include_glob + exclude_glob + context_lines: 2
    let out = tools
        .execute(
            "code_search",
            r#"{"pattern":"fn\\s+validate_\\w+","regex":true,"case_insensitive":true,"include_glob":"*.rs","exclude_glob":"*ignore_me*","context_lines":2}"#,
        )
        .unwrap();
    assert!(out.contains("src/auth.rs-1- line1"), "{out}");
    assert!(out.contains("src/auth.rs-2- line2"), "{out}");
    assert!(
        out.contains("src/auth.rs:3: pub fn Validate_Token(x: i32) -> bool {"),
        "{out}"
    );
    assert!(out.contains("src/auth.rs-4-     let ok = x > 0;"), "{out}");
    assert!(out.contains("src/auth.rs-5-     ok"), "{out}");
    assert!(!out.contains("ignore_me.rs"), "{out}");
    assert!(!out.contains("docs/notes.md"), "{out}");

    // Backward-compatible call without v2 fields still emits exact `path:line: content`.
    let legacy = tools
        .execute("code_search", r#"{"pattern":"Validate_Token"}"#)
        .unwrap();
    assert!(
        legacy.contains("src/auth.rs:3: pub fn Validate_Token(x: i32) -> bool {"),
        "{legacy}"
    );
    assert!(!legacy.contains("\n--\n"), "{legacy}");

    // Malformed regex returns a descriptive error without panic.
    let bad = tools
        .execute("code_search", r#"{"pattern":"[unclosed(","regex":true}"#)
        .unwrap_err();
    assert!(bad.contains("invalid regex pattern"), "{bad}");
}

#[test]
fn glob_files_blocks_traversal_skips_ignored_and_sorts_deterministically() {
    let dir = TempDir::new("glob_files");
    fs::create_dir_all(dir.0.join("crates/a/src")).unwrap();
    fs::create_dir_all(dir.0.join("crates/b/tests")).unwrap();
    fs::create_dir_all(dir.0.join(".onyx/skills/demo")).unwrap();
    fs::create_dir_all(dir.0.join("target/debug")).unwrap();
    fs::create_dir_all(dir.0.join(".git")).unwrap();

    fs::write(dir.0.join("crates/b/tests/z.rs"), "// z\n").unwrap();
    fs::write(dir.0.join("crates/a/src/lib.rs"), "// a\n").unwrap();
    fs::write(dir.0.join(".onyx/skills/demo/SKILL.md"), "# demo\n").unwrap();
    fs::write(dir.0.join("target/debug/should_skip.rs"), "// skip\n").unwrap();
    fs::write(dir.0.join(".git/skip.rs"), "// skip\n").unwrap();

    assert!(WorkspaceTools::all_tool_names().contains(&"glob_files"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"glob_files"));

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // TEST-02.2: deterministic lexicographical order + ignores `target/` and `.git/`
    let rs_files = tools
        .execute("glob_files", r#"{"pattern":"crates/**/*.rs"}"#)
        .unwrap();
    assert_eq!(rs_files, "crates/a/src/lib.rs\ncrates/b/tests/z.rs");

    let skills = tools
        .execute("glob_files", r#"{"pattern":"**/*SKILL.md"}"#)
        .unwrap();
    assert_eq!(skills, ".onyx/skills/demo/SKILL.md");

    // Path traversal blocked in both `path` and `pattern`.
    let err_path = tools
        .execute("glob_files", r#"{"pattern":"*.rs","path":"../"}"#)
        .unwrap_err();
    assert!(err_path.contains("escapes workspace root"), "{err_path}");

    let err_pat = tools
        .execute("glob_files", r#"{"pattern":"../**/*.rs"}"#)
        .unwrap_err();
    assert!(err_pat.contains("escapes workspace root"), "{err_pat}");
}

#[test]
fn permission_rules_toml_precedence_global_deny_and_auto_approve_enforcement() {
    let dir = TempDir::new("perm_rules");
    fs::create_dir_all(dir.0.join(".onyx")).unwrap();
    fs::create_dir_all(dir.0.join("docs")).unwrap();
    fs::write(dir.0.join("docs/guide.md"), "v1\n").unwrap();

    let global_cfg = dir.0.join("global_config.toml");
    fs::write(
        &global_cfg,
        r#"
[permissions]
# Global deny cannot be overridden by project allow
deny = ["shell_exec(rm -rf *)"]
"#,
    )
    .unwrap();

    let project_cfg = dir.0.join(".onyx/config.toml");
    fs::write(
        &project_cfg,
        r#"
[other_section]
ignored_key = "hello"

[permissions]
deny = [
  "shell_exec(git push --force*)",
  "fs_write(.env*)",
]

allow = [
  "shell_exec(cargo check*)",
  "shell_exec(cargo test*)",
  "shell_exec(rm -rf *)", # Attempt to override global deny must fail
  "fs_edit(docs/**)",
]

ask = [
  "shell_exec(*)",
  "fs_write(*)",
  "fs_edit(*)",
]
"#,
    )
    .unwrap();

    let rules = PermissionRules::load_from_paths(Some(&global_cfg), Some(&project_cfg)).unwrap();
    let policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_permissions(rules);

    // TEST-02.3: deny > ask > allow
    assert_eq!(
        policy.evaluate_permission("shell_exec", r#"{"command":"cargo test -p open-onyx"}"#),
        PermissionDecision::Allow
    );
    assert_eq!(
        policy.evaluate_permission("shell_exec", r#"{"command":"rm -rf ."}"#),
        PermissionDecision::Deny
    );
    assert_eq!(
        policy.evaluate_permission(
            "shell_exec",
            r#"{"command":"git push --force origin main"}"#
        ),
        PermissionDecision::Deny
    );
    assert_eq!(
        policy.evaluate_permission("fs_write", r#"{"path":".env.local","content":"SECRET=1"}"#),
        PermissionDecision::Deny
    );
    assert_eq!(
        policy.evaluate_permission("fs_edit", r#"{"path":"docs/guide.md"}"#),
        PermissionDecision::Allow
    );
    assert_eq!(
        policy.evaluate_permission("fs_edit", r#"{"path":"src/main.rs"}"#),
        PermissionDecision::Ask
    );
    assert_eq!(
        policy.evaluate_permission("shell_exec", r#"{"command":"echo hi"}"#),
        PermissionDecision::Ask
    );

    // Equal or more specific `ask` wins over `allow`.
    let tie_rules = PermissionRules::parse_toml(
        r#"
[permissions]
allow = ["shell_exec(cargo *)"]
ask = ["shell_exec(cargo publish*)"]
"#,
    )
    .unwrap();
    assert_eq!(
        tie_rules.evaluate("shell_exec", r#"{"command":"cargo publish"}"#, false),
        PermissionDecision::Ask
    );
    assert_eq!(
        tie_rules.evaluate("shell_exec", r#"{"command":"cargo check"}"#, false),
        PermissionDecision::Allow
    );

    // Enforcement under `--auto-approve` (no approval_tools on AgentSpec):
    // `deny` STILL blocks `fs_write(.env)` inside `WorkspaceToolBridge`, unless `ignore_deny` is set.
    let tools = WorkspaceTools::new(policy.clone());
    let mut host = AgentHost::builder(1).build().unwrap();
    host.spawn(
        &AgentSpec::new("auto", "try writing .env")
            .with_tools(WorkspaceTools::all_tool_names().iter().copied()),
    );
    let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools).wrapping(HeadlessDriver::new());
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
                r#"{"path":".env","content":"SECRET=1"}"#,
            ))
        }
    })
    .wrapping(ws_bridge);
    assert!(host.run_until_settled(&mut driver, 16));
    let id = host.agents()[0];
    let res = host.result(id).unwrap_or_default();
    assert!(res.contains("deny rule"), "expected deny error, got: {res}");
    assert!(!dir.0.join(".env").exists(), ".env must not be created");

    // With explicit `--ignore-deny`, `evaluate_permission` bypasses deny rules.
    let ignore_policy = policy.with_ignore_deny(true);
    assert_eq!(
        ignore_policy.evaluate_permission("shell_exec", r#"{"command":"rm -rf ."}"#),
        PermissionDecision::Allow
    );
}

#[test]
fn plan_mode_blocks_mutating_tools_even_when_allowed_in_config_toml() {
    let dir = TempDir::new("plan_overrides_allow");
    fs::create_dir_all(dir.0.join(".onyx")).unwrap();
    fs::create_dir_all(dir.0.join("docs")).unwrap();
    fs::write(dir.0.join("docs/guide.md"), "original\n").unwrap();
    fs::write(
        dir.0.join(".onyx/config.toml"),
        r#"
[permissions]
allow = [
  "fs_edit(docs/**)",
  "fs_write(docs/**)",
  "shell_exec(cargo check*)",
]
"#,
    )
    .unwrap();

    let policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_trusted(true);
    assert_eq!(
        policy.evaluate_permission("fs_write", r#"{"path":"docs/new.md","content":"x"}"#),
        PermissionDecision::Allow
    );
    assert_eq!(
        policy.evaluate_permission_in_mode(
            WorkspaceMode::Plan,
            "fs_write",
            r#"{"path":"docs/new.md","content":"x"}"#,
        ),
        PermissionDecision::Deny
    );
    assert_eq!(
        policy.evaluate_permission_in_mode(
            WorkspaceMode::Explore,
            "shell_exec",
            r#"{"command":"cargo check"}"#,
        ),
        PermissionDecision::Deny
    );

    // TEST-02.4: End-to-end with AgentHost + approval_tools enabled:
    // 1. In Plan mode, even `fs_write(docs/**)` listed in `allow` is denied automatically without mutating disk.
    let mut plan_host = AgentHost::builder(1).build().unwrap();
    plan_host.spawn(
        &AgentSpec::new("planner", "write doc in plan mode")
            .with_tools(WorkspaceTools::all_tool_names().iter().copied())
            .with_approval_tools(WorkspaceTools::approval_required_names().iter().copied()),
    );
    let plan_bridge =
        WorkspaceToolBridge::new(plan_host.vocab(), WorkspaceTools::new(policy.clone()))
            .with_mode(WorkspaceMode::Plan)
            .wrapping(HeadlessDriver::new());
    let mut plan_driver = RecordingBridge::new(plan_host.vocab(), |req: &AgentView| {
        if let Some((_, msg)) = req
            .memory
            .iter()
            .find(|(k, _)| k == "denied" || k == "tool_error")
        {
            Some(Reply::done(req.agent, req.request, msg.clone()))
        } else if req.memory.iter().any(|(k, _)| k == "call") {
            None
        } else {
            Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_write",
                r#"{"path":"docs/blocked.md","content":"nope"}"#,
            ))
        }
    })
    .wrapping(plan_bridge);
    assert!(plan_host.run_until_settled(&mut plan_driver, 16));
    assert!(!dir.0.join("docs/blocked.md").exists());
    let plan_res = plan_host.result(plan_host.agents()[0]).unwrap_or_default();
    assert!(
        plan_res.contains("read-only mode"),
        "expected read-only denial, got: {plan_res}"
    );

    // 2. In Build mode with `approval_tools` enabled, `fs_write(docs/ok.md)` matches `allow`
    // and auto-approves cleanly without human interaction!
    let mut build_host = AgentHost::builder(2).build().unwrap();
    build_host.spawn(
        &AgentSpec::new("builder", "write allowed doc in build mode")
            .with_tools(WorkspaceTools::all_tool_names().iter().copied())
            .with_approval_tools(WorkspaceTools::approval_required_names().iter().copied()),
    );
    let build_bridge = WorkspaceToolBridge::new(build_host.vocab(), WorkspaceTools::new(policy))
        .with_mode(WorkspaceMode::Build)
        .wrapping(HeadlessDriver::new());
    let mut build_driver = RecordingBridge::new(build_host.vocab(), |req: &AgentView| {
        if let Some((_, obs)) = req.memory.iter().find(|(k, _)| k == "observe") {
            Some(Reply::done(req.agent, req.request, obs.clone()))
        } else if req.memory.iter().any(|(k, _)| k == "call") {
            None
        } else {
            Some(Reply::call_structured(
                req.agent,
                req.request,
                "fs_write",
                r#"{"path":"docs/ok.md","content":"allowed!\n"}"#,
            ))
        }
    })
    .wrapping(build_bridge);
    assert!(build_host.run_until_settled(&mut build_driver, 16));
    assert_eq!(
        fs::read_to_string(dir.0.join("docs/ok.md")).unwrap(),
        "allowed!\n"
    );
}

#[test]
fn sandbox_sanitizes_env_hiding_api_keys_and_honoring_passthrough() {
    let dir = TempDir::new("sandbox_env");
    // TEST-06.1: inject a sensitive API key and an explicit passthrough var in the parent process.
    unsafe {
        std::env::set_var("ONYX_TEST_SECRET_API_KEY", "super-secret-leak-check-9999");
        std::env::set_var("ONYX_TEST_PASSTHROUGH_VAR", "allowed-passthrough-val");
    }

    let mut cfg = SandboxConfig::default();
    cfg.env_passthrough
        .push("ONYX_TEST_PASSTHROUGH_VAR".to_string());
    let policy = WorkspacePolicy::new(&dir.0).unwrap().with_sandbox(cfg);
    let tools = WorkspaceTools::new(policy);

    let cmd = if cfg!(windows) {
        r#"{"command":"Get-ChildItem Env: | ForEach-Object { \"$($_.Name)=$($_.Value)\" }"}"#
    } else {
        r#"{"command":"env"}"#
    };
    let out = tools.execute("shell_exec", cmd).unwrap();

    unsafe {
        std::env::remove_var("ONYX_TEST_SECRET_API_KEY");
        std::env::remove_var("ONYX_TEST_PASSTHROUGH_VAR");
    }

    assert!(
        !out.contains("ONYX_TEST_SECRET_API_KEY") && !out.contains("super-secret-leak-check-9999"),
        "sensitive API key must never enter sandboxed child env: {out}"
    );
    assert!(
        out.contains("ONYX_TEST_PASSTHROUGH_VAR=allowed-passthrough-val"),
        "explicit env_passthrough var must be forwarded: {out}"
    );
    assert!(
        out.contains("CARGO_NET_OFFLINE=true"),
        "offline network hint must be injected: {out}"
    );
    assert!(
        out.contains("ONYX_SANDBOX_MODE=workspace-write"),
        "sandbox mode env marker missing: {out}"
    );
}

#[test]
fn sandbox_config_toml_auto_approve_elevation_workspace_write_and_process_job() {
    let dir = TempDir::new("sandbox_cfg");
    fs::create_dir_all(dir.0.join(".onyx")).unwrap();
    fs::write(
        dir.0.join(".onyx/config.toml"),
        r#"
[permissions]
allow = ["shell_exec(echo *)"]

[sandbox]
mode = "off"
network = "localhost"
env_passthrough = ["RUST_LOG", "RUST_BACKTRACE"]
max_memory_mb = 2048
max_pids = 64
"#,
    )
    .unwrap();

    let mut policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_trusted(true);
    assert_eq!(policy.sandbox.mode, SandboxMode::Off);
    assert_eq!(policy.sandbox.network, NetworkPolicy::LocalhostOnly);
    assert_eq!(
        policy.sandbox.env_passthrough,
        vec!["RUST_LOG".to_string(), "RUST_BACKTRACE".to_string()]
    );
    assert_eq!(policy.sandbox.max_memory_mb, Some(2048));
    assert_eq!(policy.sandbox.max_pids, Some(64));

    // Elevation rule (REQ-06.5): explicit CLI `--sandbox off` is honored; implicit elevates to WorkspaceWrite + Offline.
    let mut explicit_off = policy.sandbox.clone();
    explicit_off.elevate_for_auto_approve(true);
    assert_eq!(explicit_off.mode, SandboxMode::Off);

    policy.sandbox.elevate_for_auto_approve(false);
    assert_eq!(policy.sandbox.mode, SandboxMode::WorkspaceWrite);
    assert_eq!(policy.sandbox.network, NetworkPolicy::Offline);

    // TEST-06.2: writing inside <workspace> succeeds under WorkspaceWrite.
    let tools = WorkspaceTools::new(policy);
    let write_cmd = if cfg!(windows) {
        r#"{"command":"Set-Content -Path inside_ok.txt -Value 'ok-in-ws'"}"#
    } else {
        r#"{"command":"printf 'ok-in-ws' > inside_ok.txt"}"#
    };
    let out = tools.execute("shell_exec", write_cmd).unwrap();
    assert!(out.contains("exit_code: 0"), "{out}");
    assert!(
        fs::read_to_string(dir.0.join("inside_ok.txt"))
            .unwrap()
            .contains("ok-in-ws")
    );

    // Verify RAII ProcessJob attaches and cleanly terminates a spawned child process.
    let mut sleeper = if cfg!(windows) {
        let mut c = std::process::Command::new("pwsh");
        c.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "sleep 30"]);
        c
    };
    onyx_workspace::configure_process_isolation(&mut sleeper, true);
    let mut child = sleeper
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut job = ProcessJob::assign(&child, ProcessLimits::default())
        .expect("ProcessJob must attach on Windows and Unix");
    job.terminate();
    let _ = child.wait();
    assert!(
        detect_sandbox_capabilities().native_driver.as_str() != "",
        "driver string must be non-empty"
    );
}

#[test]
fn sandbox_network_policy_blocks_outbound_under_offline_and_allows_localhost() {
    let dir = TempDir::new("sandbox_net");
    let offline_policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_sandbox(SandboxConfig {
            mode: SandboxMode::WorkspaceWrite,
            network: NetworkPolicy::Offline,
            ..SandboxConfig::default()
        });
    let offline_tools = WorkspaceTools::new(offline_policy);

    // TEST-06.3: Outbound network command under Offline fails with an actionable error.
    let err = offline_tools
        .execute("shell_exec", r#"{"command":"curl https://example.com"}"#)
        .unwrap_err();
    assert!(
        err.contains("--sandbox-net unrestricted") || err.contains("offline"),
        "expected actionable offline network error, got: {err}"
    );

    // Under LocalhostOnly, external domains remain blocked while localhost URLs pass the guard.
    assert!(
        onyx_workspace::sandbox::check_offline_network_command(
            "curl https://example.com",
            NetworkPolicy::LocalhostOnly
        )
        .is_err()
    );
    assert!(
        onyx_workspace::sandbox::check_offline_network_command(
            "curl http://127.0.0.1:8080/health",
            NetworkPolicy::LocalhostOnly
        )
        .is_ok()
    );
}

#[test]
fn control_plane_protection_blocks_fs_and_shell_writes() {
    let dir = TempDir::new("control_plane");
    fs::create_dir_all(dir.0.join(".onyx")).unwrap();
    fs::create_dir_all(dir.0.join(".git/hooks")).unwrap();
    fs::write(dir.0.join(".onyx/config.toml"), "[permissions]\n").unwrap();
    fs::write(dir.0.join(".git/config"), "[core]\n").unwrap();

    // No `[permissions].deny` rules configured — protection must come from invariant Control Plane guard!
    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // TEST-06.4: `fs_write` / `fs_edit` targeting control-plane files must be rejected.
    let err_cfg = tools
        .execute(
            "fs_write",
            r#"{"path":".onyx/config.toml","content":"[permissions]\nallow = [\"*\"]\n"}"#,
        )
        .unwrap_err();
    assert!(err_cfg.contains("control-plane protection"), "{err_cfg}");

    let err_edit = tools
        .execute(
            "fs_edit",
            r#"{"path":".onyx/config.toml","old_text":"[permissions]","new_text":"[hacked]"}"#,
        )
        .unwrap_err();
    assert!(err_edit.contains("control-plane protection"), "{err_edit}");

    let err_hook = tools
        .execute(
            "fs_write",
            r##"{"path":".git/hooks/pre-commit","content":"#!/bin/sh\necho pwned\n"}"##,
        )
        .unwrap_err();
    assert!(err_hook.contains("control-plane protection"), "{err_hook}");

    let err_git_cfg = tools
        .execute(
            "fs_write",
            r#"{"path":".git/config","content":"[core]\nsshCommand = evil\n"}"#,
        )
        .unwrap_err();
    assert!(
        err_git_cfg.contains("control-plane protection"),
        "{err_git_cfg}"
    );

    let err_env = tools
        .execute("fs_write", r#"{"path":".env.prod","content":"KEY=1"}"#)
        .unwrap_err();
    assert!(err_env.contains("control-plane protection"), "{err_env}");

    // `shell_exec` attempting to overwrite `.onyx/config.toml` or `.git/hooks/pre-commit` is rejected.
    let err_sh_cfg = tools
        .execute(
            "shell_exec",
            r#"{"command":"echo hacked > .onyx/config.toml"}"#,
        )
        .unwrap_err();
    assert!(
        err_sh_cfg.contains("control-plane protection"),
        "{err_sh_cfg}"
    );

    let err_sh_hook = tools
        .execute(
            "shell_exec",
            r#"{"command":"echo evil > .git/hooks/pre-commit"}"#,
        )
        .unwrap_err();
    assert!(
        err_sh_hook.contains("control-plane protection"),
        "{err_sh_hook}"
    );

    // Original `.onyx/config.toml` remains intact.
    assert_eq!(
        fs::read_to_string(dir.0.join(".onyx/config.toml")).unwrap(),
        "[permissions]\n"
    );
}

#[test]
fn sprout_parser_supports_modern_and_legacy_formats_and_rejects_dag_cycles() {
    let dir = TempDir::new("sprout_dag");
    fs::create_dir_all(dir.0.join("services/orders-api")).unwrap();
    let policy = WorkspacePolicy::new(&dir.0).unwrap();

    // TEST-08.1: Modern `[runtime]` + `[services.<id>]` with process + container + depends_on.
    let modern_toml = r#"
[runtime]
container_cli = "docker"
env_wrapper = "call env.bat &&"

[services.postgres]
kind = "container"
up = "docker compose up -d postgres"
logs = "docker compose logs -f postgres"
down = "docker compose stop postgres"
container = "app-postgres"
ready_port = 5432

[services.redis]
up = "docker compose up -d redis"
container = "app-redis"
ready_port = 6379

[services.orders-api]
kind = "process"
depends_on = ["postgres", "redis"]
cwd = "services/orders-api"
cmd = "mvnw.cmd spring-boot:run"
debug_port = 5005
ready_log = "Started OrdersApplication"
ready_port = 8080
"#;
    let cfg = SproutConfig::parse_toml(modern_toml, &dir.0, Some(&policy)).unwrap();
    assert_eq!(
        cfg.container_cli,
        Some(onyx_workspace::ContainerCli::Docker)
    );
    assert_eq!(cfg.env_wrapper.as_deref(), Some("call env.bat &&"));
    assert_eq!(cfg.services["postgres"].kind, ServiceKind::Container);
    assert_eq!(cfg.services["redis"].kind, ServiceKind::Container); // inferred from `up`/`container`
    assert_eq!(cfg.services["orders-api"].kind, ServiceKind::Process);
    assert_eq!(cfg.services["orders-api"].debug_port, Some(5005));

    let order = cfg.transitive_start_order("orders-api").unwrap();
    assert_eq!(order, vec!["postgres", "redis", "orders-api"]);

    // Legacy `[global]` + `[[service]]` format.
    let legacy_toml = r#"
[global]
env_wrapper = "call legacy.bat &&"

[[service]]
id = "db"
name = "Local DB"
dir = "."
cmd = "echo db"

[[service]]
id = "api"
name = "Orders API"
dir = "services/orders-api"
cmd = "echo api"
depends_on = ["db"]
"#;
    let legacy = SproutConfig::parse_toml(legacy_toml, &dir.0, Some(&policy)).unwrap();
    assert_eq!(legacy.env_wrapper.as_deref(), Some("call legacy.bat &&"));
    assert_eq!(legacy.topo_order().unwrap(), vec!["db", "api"]);

    // Rejects cycle (`a -> b -> a`).
    let cyclic_toml = r#"
[services.a]
cmd = "echo a"
depends_on = ["b"]

[services.b]
cmd = "echo b"
depends_on = ["a"]
"#;
    let cycle_err = SproutConfig::parse_toml(cyclic_toml, &dir.0, Some(&policy)).unwrap_err();
    assert!(cycle_err.contains("cycle"), "{cycle_err}");

    // Rejects unknown dependency.
    let missing_dep = r#"
[services.a]
cmd = "echo a"
depends_on = ["nonexistent"]
"#;
    let dep_err = SproutConfig::parse_toml(missing_dep, &dir.0, Some(&policy)).unwrap_err();
    assert!(dep_err.contains("unknown service"), "{dep_err}");

    // Rejects cwd escape outside workspace root.
    let escape_cwd = r#"
[services.a]
cwd = "../outside"
cmd = "echo a"
"#;
    let esc_err = SproutConfig::parse_toml(escape_cwd, &dir.0, Some(&policy)).unwrap_err();
    assert!(esc_err.contains("escapes workspace root"), "{esc_err}");
}

#[test]
fn sprout_process_lifecycle_probes_logs_and_clean_drop_teardown() {
    let dir = TempDir::new("sprout_proc");
    // Open an ephemeral TCP listener on 127.0.0.1 to prove real `ready_port` TCP probing.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let cmd_str = if cfg!(windows) {
        "pwsh -NoProfile -Command \"Write-Output 'Authorization: Bearer secret_sprout_token_9999'; Write-Output 'Started DemoService'; Start-Sleep -Seconds 20\""
    } else {
        "echo 'Authorization: Bearer secret_sprout_token_9999'; echo Started DemoService; sleep 20"
    };
    let cmd_escaped = cmd_str.replace('\\', "\\\\").replace('"', "\\\"");
    fs::write(
        dir.0.join("sprout.toml"),
        format!(
            r#"
[services.demo-api]
kind = "process"
cmd = "{cmd_escaped}"
ready_log = "Started DemoService"
ready_port = {port}
"#
        ),
    )
    .unwrap();

    assert!(WorkspaceTools::all_tool_names().contains(&"service_status"));
    assert!(WorkspaceTools::all_tool_names().contains(&"service_start"));
    assert!(WorkspaceTools::all_tool_names().contains(&"service_stop"));
    assert!(WorkspaceTools::all_tool_names().contains(&"service_logs"));
    assert!(WorkspaceTools::all_tool_names().contains(&"service_exec"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"service_status"));
    assert!(!WorkspaceTools::approval_required_names().contains(&"service_logs"));
    assert!(WorkspaceTools::approval_required_names().contains(&"service_start"));
    assert!(WorkspaceTools::approval_required_names().contains(&"service_stop"));
    assert!(WorkspaceTools::approval_required_names().contains(&"service_exec"));

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // TEST-08.2: initial status is OFF.
    let st0 = tools.execute("service_status", "{}").unwrap();
    assert!(st0.starts_with("@TOON v1"), "{st0}");
    assert!(st0.contains("demo-api | process | OFF"), "{st0}");

    // Start transitions to READY when both `ready_log` and `ready_port` succeed.
    let started = tools
        .execute("service_start", r#"{"id":"demo-api","wait_ready_secs":15}"#)
        .unwrap();
    assert!(started.contains("demo-api | process | READY"), "{started}");

    // Logs capture output, support regex filtering, and scrub secrets.
    let logs_all = tools
        .execute("service_logs", r#"{"id":"demo-api","tail":50}"#)
        .unwrap();
    assert!(logs_all.contains("Started DemoService"), "{logs_all}");
    assert!(
        !logs_all.contains("secret_sprout_token_9999") && logs_all.contains("[REDACTED]"),
        "service_logs must scrub secrets: {logs_all}"
    );
    let logs_filtered = tools
        .execute(
            "service_logs",
            r#"{"id":"demo-api","pattern":"Started\\s+Demo\\w+","regex":true}"#,
        )
        .unwrap();
    assert!(
        logs_filtered.contains("Started DemoService"),
        "{logs_filtered}"
    );

    // Explicit stop tears down the ProcessJob cleanly and resets status to OFF.
    let stopped = tools
        .execute("service_stop", r#"{"id":"demo-api"}"#)
        .unwrap();
    assert!(stopped.contains("demo-api | process | OFF"), "{stopped}");

    // `service_start` also supports `all: true`.
    let started_all = tools
        .execute("service_start", r#"{"all":true,"wait_ready_secs":15}"#)
        .unwrap();
    assert!(
        started_all.contains("demo-api | process | READY"),
        "{started_all}"
    );
    let stopped_all = tools.execute("service_stop", r#"{"all":true}"#).unwrap();
    assert!(
        stopped_all.contains("demo-api | process | OFF"),
        "{stopped_all}"
    );

    drop(tools);
    drop(listener);
}

fn create_container_stub(dir: &std::path::Path) -> PathBuf {
    #[cfg(windows)]
    {
        let path = dir.join("fake-docker.cmd");
        let script = "@echo off\r\n\
if \"%1\"==\"--version\" (echo Docker stub 1.0 & exit /b 0)\r\n\
if \"%1\"==\"info\" (echo Stub Daemon OK & exit /b 0)\r\n\
if \"%1\"==\"compose\" (echo compose %2 %3 %4 & exit /b 0)\r\n\
if \"%1\"==\"exec\" (\r\n\
  echo CONTAINER=%2\r\n\
  echo LEAKED=%ONYX_TEST_SPROUT_API_KEY%\r\n\
  echo SECRET=sk-test1234567890abcdef1234\r\n\
  echo CMD=%5\r\n\
  exit /b 0\r\n\
)\r\n\
exit /b 0\r\n";
        fs::write(&path, script).unwrap();
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-docker.sh");
        let script = "#!/bin/sh\n\
case \"$1\" in\n\
  --version) echo 'Docker stub 1.0'; exit 0 ;;\n\
  info) echo 'Stub Daemon OK'; exit 0 ;;\n\
  compose) echo \"compose $2 $3 $4\"; exit 0 ;;\n\
  exec)\n\
    echo \"CONTAINER=$2\"\n\
    echo \"LEAKED=${ONYX_TEST_SPROUT_API_KEY}\"\n\
    echo \"SECRET=sk-test1234567890abcdef1234\"\n\
    echo \"CMD=$5\"\n\
    exit 0 ;;\n\
esac\n\
exit 0\n";
        fs::write(&path, script).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }
}

#[test]
fn sprout_container_service_exec_uses_sanitized_env_scrubs_secrets_and_respects_plan_and_deny() {
    let dir = TempDir::new("sprout_container");
    let stub = create_container_stub(&dir.0);
    let stub_toml = stub.to_string_lossy().replace('\\', "\\\\");

    fs::write(
        dir.0.join("sprout.toml"),
        format!(
            r#"
[runtime]
container_cli = "docker"

[services.postgres]
kind = "container"
up = "{stub_toml} compose up -d postgres"
down = "{stub_toml} compose stop postgres"
container = "app-postgres"
"#
        ),
    )
    .unwrap();

    fs::create_dir_all(dir.0.join(".onyx")).unwrap();
    fs::write(
        dir.0.join(".onyx/config.toml"),
        r#"
[permissions]
deny = ["service_exec(*DROP DATABASE*)"]
allow = ["service_start(postgres)", "service_exec(postgres:psql*)"]
"#,
    )
    .unwrap();

    unsafe {
        std::env::set_var("ONYX_TEST_SPROUT_API_KEY", "never-leak-into-container-999");
    }

    let policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_trusted(true);
    assert_eq!(
        policy.evaluate_permission(
            "service_exec",
            r#"{"id":"postgres","command":"psql -c \\dt"}"#
        ),
        PermissionDecision::Allow
    );
    assert_eq!(
        policy.evaluate_permission(
            "service_exec",
            r#"{"id":"postgres","command":"psql -c 'DROP DATABASE app'"}"#
        ),
        PermissionDecision::Deny
    );
    assert_eq!(
        policy.evaluate_permission_in_mode(
            WorkspaceMode::Plan,
            "service_exec",
            r#"{"id":"postgres","command":"psql -c \\dt"}"#
        ),
        PermissionDecision::Deny
    );

    let sprout = onyx_workspace::SproutSupervisor::new(policy.clone())
        .with_container_cli_override(stub.to_string_lossy().to_string());
    let tools = WorkspaceTools::new(policy).with_sprout(sprout);

    // TEST-08.3: Start container service (verifies daemon via `stub info`), then `service_exec`.
    let started = tools
        .execute("service_start", r#"{"id":"postgres","wait_ready_secs":10}"#)
        .unwrap();
    assert!(
        started.contains("postgres | container | READY"),
        "{started}"
    );

    let exec_out = tools
        .execute(
            "service_exec",
            r#"{"id":"postgres","command":"psql -U app -c dt"}"#,
        )
        .unwrap();
    unsafe {
        std::env::remove_var("ONYX_TEST_SPROUT_API_KEY");
    }
    assert!(exec_out.contains("CONTAINER=app-postgres"), "{exec_out}");
    assert!(
        !exec_out.contains("never-leak-into-container-999"),
        "env_clear must strip sensitive keys in service_exec: {exec_out}"
    );
    assert!(
        !exec_out.contains("sk-test1234567890abcdef1234") && exec_out.contains("[REDACTED]"),
        "scrub_secrets must redact secrets in service_exec output: {exec_out}"
    );

    let _ = tools.execute("service_stop", r#"{"all":true}"#);
}

#[test]
fn sprout_session_replays_deterministically_from_onyxtr2_without_spawning_processes() {
    let dir = TempDir::new("sprout_replay");
    let stub = create_container_stub(&dir.0);
    let stub_toml = stub.to_string_lossy().replace('\\', "\\\\");

    fs::write(
        dir.0.join("sprout.toml"),
        format!(
            r#"
[runtime]
container_cli = "docker"

[services.postgres]
kind = "container"
up = "{stub_toml} compose up -d postgres"
down = "{stub_toml} compose stop postgres"
container = "app-postgres"
"#
        ),
    )
    .unwrap();

    let policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_trusted(true);
    let sprout = onyx_workspace::SproutSupervisor::new(policy.clone())
        .with_container_cli_override(stub.to_string_lossy().to_string());
    let tools = WorkspaceTools::new(policy).with_sprout(sprout);

    let mut host = AgentHost::builder(7)
        .system(Box::new(WorkspaceLedgerSystem::new()))
        .build()
        .unwrap();
    let ws_vocab = WorkspaceVocab::resolve_with(|name| host.attr(name));
    host.spawn(
        &AgentSpec::new("sprout-agent", "start postgres and check tables")
            .with_tools(WorkspaceTools::all_tool_names().iter().copied()),
    );

    let mut headless = HeadlessDriver::new();
    headless.push_event(onyx_soul_agents::OnyxEvent::Ingress(
        onyx_soul_agents::IngressEvent::Command("/services".to_string()),
    ));
    let ws_bridge = WorkspaceToolBridge::new(host.vocab(), tools)
        .with_workspace_vocab(ws_vocab)
        .wrapping(headless);
    let mut turn = 0usize;
    let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
        turn += 1;
        match turn {
            1 => Some(
                Reply::call_structured(
                    req.agent,
                    req.request,
                    "service_start",
                    r#"{"id":"postgres","wait_ready_secs":10}"#,
                )
                .with_call_id("sprout-1"),
            ),
            2 => Some(
                Reply::call_structured(
                    req.agent,
                    req.request,
                    "service_logs",
                    r#"{"id":"postgres","tail":10}"#,
                )
                .with_call_id("sprout-2"),
            ),
            3 => Some(
                Reply::call_structured(
                    req.agent,
                    req.request,
                    "service_exec",
                    r#"{"id":"postgres","command":"psql -c dt"}"#,
                )
                .with_call_id("sprout-3"),
            ),
            _ => Some(Reply::done(req.agent, req.request, "postgres checked")),
        }
    })
    .wrapping(ws_bridge);

    assert!(host.run_until_settled(&mut driver, 32));
    let live_panel = graph_workspace_services(host.graph(), &ws_vocab)
        .expect("ws.services.text must be projected into EAV graph");
    assert!(
        live_panel.contains("[READY] postgres (container)"),
        "{live_panel}"
    );

    // Serialize transcript (`ONYXTR2`) and remove `sprout.toml` + stub to prove 0 subprocesses in replay (TEST-08.4).
    let tr_bytes = driver.transcript().to_bytes();
    assert!(tr_bytes.starts_with(b"ONYXTR2"));
    let _ = fs::remove_file(dir.0.join("sprout.toml"));
    let _ = fs::remove_file(&stub);

    let decoded = onyx_soul_agents::Transcript::from_bytes(&tr_bytes).unwrap();
    let mut replay_host = AgentHost::builder(7)
        .system(Box::new(WorkspaceLedgerSystem::new()))
        .build()
        .unwrap();
    let replay_ws_vocab = WorkspaceVocab::resolve_with(|name| replay_host.attr(name));
    replay_host.spawn(
        &AgentSpec::new("sprout-agent", "start postgres and check tables")
            .with_tools(WorkspaceTools::all_tool_names().iter().copied()),
    );
    let mut replay_driver = ReplayBridge::new(decoded).wrapping(HeadlessDriver::new());
    assert!(replay_host.run_until_settled(&mut replay_driver, 32));

    let replayed_agent = replay_host.agents()[0];
    assert_eq!(
        replay_host.result(replayed_agent).as_deref(),
        Some("postgres checked")
    );
    let replayed_panel = graph_workspace_services(replay_host.graph(), &replay_ws_vocab)
        .expect("ws.services.text must replay deterministically");
    assert_eq!(replayed_panel, live_panel);
}

#[test]
fn fs_read_rejects_binary_onyx_artifacts_dedups_covered_ranges_and_caches_scent_outline() {
    let dir = TempDir::new("anti_loop_and_binaries");
    fs::write(dir.0.join("sample.tr"), b"ONYXTR2\n\x00\xff\xfe").unwrap();
    fs::write(dir.0.join("legacy.tr"), b"ONYXTR1\n\x00\xff\xfe").unwrap();
    fs::write(dir.0.join("snapshots.onyxpack"), b"ONYXPK1\x00\xff").unwrap();
    fs::write(dir.0.join("tick-1.bin"), b"ONYXAG\x00\xff").unwrap();
    fs::write(
        dir.0.join("lib.rs"),
        (1..=25)
            .map(|i| format!("pub fn f_{i}() -> usize {{ {i} }}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let tools = WorkspaceTools::new(policy);

    // Binary OnYX artifacts return actionable CLI inspection hints instead of raw UTF-8 errors.
    let err_tr2 = tools
        .execute("fs_read", r#"{"path":"sample.tr"}"#)
        .unwrap_err();
    assert!(
        err_tr2.contains("openonyx replay") && err_tr2.contains("doctor --transcript"),
        "{err_tr2}"
    );
    let err_tr1 = tools
        .execute("fs_read", r#"{"path":"legacy.tr"}"#)
        .unwrap_err();
    assert!(err_tr1.contains("openonyx replay"), "{err_tr1}");
    let err_pk = tools
        .execute("fs_read", r#"{"path":"snapshots.onyxpack"}"#)
        .unwrap_err();
    assert!(err_pk.contains("openonyx inspect"), "{err_pk}");
    let err_ag = tools
        .execute("fs_read", r#"{"path":"tick-1.bin"}"#)
        .unwrap_err();
    assert!(err_ag.contains("openonyx inspect"), "{err_ag}");

    // Overlapping/subset `fs_read` range is deduplicated until the file changes.
    let full = tools
        .execute(
            "fs_read",
            r#"{"path":"lib.rs","start_line":1,"end_line":20}"#,
        )
        .unwrap();
    assert!(full.contains("pub fn f_10()"), "{full}");
    let subset = tools
        .execute(
            "fs_read",
            r#"{"path":"lib.rs","start_line":5,"end_line":12}"#,
        )
        .unwrap();
    assert!(
        subset.contains("unchanged_since_tick") && subset.contains("covered_by:1-20"),
        "{subset}"
    );

    // `scent_outline` caches identical queries and invalidates on `fs_edit`.
    #[cfg(feature = "scent")]
    {
        let first_outline = tools
            .execute("scent_outline", r#"{"path":"lib.rs"}"#)
            .unwrap();
        assert!(
            !first_outline.contains("[cached scent_outline"),
            "{first_outline}"
        );
        let second_outline = tools
            .execute("scent_outline", r#"{"path":"lib.rs"}"#)
            .unwrap();
        assert!(
            second_outline.starts_with("[cached scent_outline"),
            "{second_outline}"
        );

        tools
            .execute(
                "fs_edit",
                r#"{"path":"lib.rs","old_text":"pub fn f_1() -> usize { 1 }","new_text":"pub fn f_1() -> usize { 100 }"}"#,
            )
            .unwrap();
        let after_edit = tools
            .execute("scent_outline", r#"{"path":"lib.rs"}"#)
            .unwrap();
        assert!(
            !after_edit.contains("[cached scent_outline"),
            "cache must be invalidated after fs_edit: {after_edit}"
        );
    }
}

#[test]
fn restore_git_tree_checkpoint_removes_newly_created_untracked_fs_write_target() {
    let dir = TempDir::new("undo_new_untracked");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir.0)
            .output()
            .expect("git must execute");
        assert!(out.status.success(), "git {:?} failed", args);
    };
    run_git(&["init"]);
    run_git(&["config", "user.name", "Test"]);
    run_git(&["config", "user.email", "test@example.com"]);
    run_git(&["config", "core.autocrlf", "false"]);
    fs::write(dir.0.join("tracked.txt"), "base\n").unwrap();
    run_git(&["add", "tracked.txt"]);
    run_git(&["commit", "-m", "init"]);

    // Pre-existing untracked user file that must NOT be deleted on undo.
    let user_note = dir.0.join("user_untracked.txt");
    fs::write(&user_note, "keep me\n").unwrap();

    let (_head, tree_sha) = capture_git_tree_checkpoint(&dir.0).unwrap();

    // Simulate `fs_write` creating a brand new untracked file after checkpoint capture.
    fs::create_dir_all(dir.0.join("roadmapSDD")).unwrap();
    let created = dir.0.join("roadmapSDD/SDD-qwen-3.md");
    fs::write(&created, "# created after checkpoint\n").unwrap();
    assert!(created.exists());

    onyx_workspace::restore_git_tree_checkpoint_for_targets(
        &dir.0,
        &tree_sha,
        &["roadmapSDD/SDD-qwen-3.md"],
    )
    .unwrap();

    assert!(
        !created.exists(),
        "newly created untracked fs_write target must be removed on undo"
    );
    assert!(
        user_note.exists(),
        "unrelated pre-existing untracked user files must be preserved"
    );
    assert_eq!(
        fs::read_to_string(dir.0.join("tracked.txt")).unwrap(),
        "base\n"
    );
}

#[test]
fn vault_dir_is_ignored_and_control_plane_protected() {
    let dir = TempDir::new("vault_policy");
    fs::create_dir_all(dir.0.join(".onyx/vault/session1")).unwrap();
    fs::write(
        dir.0.join(".onyx/vault/session1/snapshots.onyxpack"),
        b"ONYXPK1",
    )
    .unwrap();
    fs::write(dir.0.join(".onyx/vault/session1/session1.tr"), b"ONYXTR2").unwrap();
    fs::create_dir_all(dir.0.join(".onyx/skills/demo")).unwrap();
    fs::write(dir.0.join(".onyx/skills/demo/SKILL.md"), "# demo skill\n").unwrap();
    fs::write(dir.0.join("hello.txt"), "hello\n").unwrap();

    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    assert!(policy.is_ignored(&dir.0.join(".onyx/vault")));
    assert!(policy.is_ignored(&dir.0.join(".onyx/vault/session1/snapshots.onyxpack")));
    assert!(!policy.is_ignored(&dir.0.join(".onyx/skills/demo/SKILL.md")));

    assert!(policy.is_control_plane_path(".onyx/vault/session1/snapshots.onyxpack"));
    assert!(
        policy
            .check_control_plane_write(".onyx/vault/unassigned/x.tr")
            .is_err()
    );

    let tools = WorkspaceTools::new(policy);
    let listed = tools.execute("fs_list", "{}").unwrap();
    assert!(listed.contains("hello.txt"), "{listed}");
    assert!(
        !listed.contains("vault"),
        "vault should be hidden from fs_list: {listed}"
    );
    assert!(
        tools
            .execute(
                "fs_write",
                r#"{"path":".onyx/vault/session1/tamper.txt","content":"nope"}"#
            )
            .is_err(),
        "agent writes into .onyx/vault must be blocked"
    );
}

#[cfg(feature = "scent")]
#[test]
fn scent_heated_approval_escalates_allow_to_ask_monotonically_and_records_symbol_provenance() {
    use onyx_workspace::{
        ImpactConfig, PermissionDecision, PermissionRules, WorkspaceLedgerSystem,
        WorkspaceToolBridge, all_recorded_effects, apply_impact_escalation,
    };

    let dir = TempDir::new("scent_heated");
    fs::create_dir_all(dir.0.join("src")).unwrap();
    fs::write(
        dir.0.join("src/engine.rs"),
        r#"pub fn critical_tick(x: i32) -> i32 {
    x + 1
}

fn isolated_private() -> i32 {
    7
}

pub fn caller_one() -> i32 {
    critical_tick(1)
}

pub fn caller_two() -> i32 {
    critical_tick(2)
}
"#,
    )
    .unwrap();
    fs::write(dir.0.join("notes.txt"), "plain text unindexed\n").unwrap();

    // Parse `[impact]` section from `.onyx/config.toml` and verify workspace override.
    fs::create_dir_all(dir.0.join(".onyx")).unwrap();
    fs::write(
        dir.0.join(".onyx/config.toml"),
        r#"
[permissions]
allow = ["fs_edit(src/**)", "fs_edit(notes.txt)"]
deny = ["fs_write(.env*)"]

[impact]
enabled = true
max_hops = 2
ask_heat_bp = 2500
hard_ask_bp = 8000
"#,
    )
    .unwrap();

    let policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_trusted(true);
    assert!(policy.impact.enabled);
    assert_eq!(policy.impact.max_hops, 2);
    assert_eq!(policy.impact.ask_heat_bp, 2_500);
    assert_eq!(policy.impact.hard_ask_bp, 8_000);

    let tools = WorkspaceTools::new(policy.clone());

    // 1. TEST-MIX1.2: static `Allow` on public symbol with callers escalates `Allow -> Ask`
    //    and emits `# impact: ...`.
    let hot_args = r#"{"path":"src/engine.rs","old_text":"    x + 1","new_text":"    x + 2"}"#;
    let (hot_decision, hot_impact) =
        tools.evaluate_permission_with_impact(WorkspaceMode::Build, "fs_edit", hot_args);
    assert_eq!(hot_decision, PermissionDecision::Ask);
    let imp = hot_impact.expect("hot edit must return ImpactAssessment");
    assert!(imp.escalates_allow_to_ask, "{imp:?}");
    assert!(imp.touches_public_src, "{imp:?}");
    assert!(imp.callers >= 2, "{imp:?}");
    assert!(
        imp.summary_line.starts_with("# impact:") && imp.summary_line.contains("critical_tick"),
        "{}",
        imp.summary_line
    );

    // 2. TEST-MIX1.3 (Monotonicity & graceful degradation):
    //    - Isolated 0-caller private fn stays `Allow`.
    let cold_args = r#"{"path":"src/engine.rs","old_text":"    7","new_text":"    8"}"#;
    let (cold_decision, cold_impact) =
        tools.evaluate_permission_with_impact(WorkspaceMode::Build, "fs_edit", cold_args);
    assert_eq!(cold_decision, PermissionDecision::Allow);
    assert_eq!(cold_impact.as_ref().map(|a| a.callers), Some(0));

    //    - Unindexed plain text file degrades cleanly to static `Allow`.
    let txt_args = r#"{"path":"notes.txt","old_text":"plain text unindexed","new_text":"updated"}"#;
    let (txt_decision, txt_impact) =
        tools.evaluate_permission_with_impact(WorkspaceMode::Build, "fs_edit", txt_args);
    assert_eq!(txt_decision, PermissionDecision::Allow);
    assert!(txt_impact.is_none());

    //    - Static `Deny` stays `Deny` even when heat is high; static `Ask` never drops to `Allow`.
    assert_eq!(
        apply_impact_escalation(PermissionDecision::Deny, Some(&imp)),
        PermissionDecision::Deny
    );
    assert_eq!(
        apply_impact_escalation(PermissionDecision::Ask, cold_impact.as_ref()),
        PermissionDecision::Ask
    );

    //    - Plan mode unconditionally blocks mutations regardless of `allow` or `[impact]`.
    let (plan_decision, _) =
        tools.evaluate_permission_with_impact(WorkspaceMode::Plan, "fs_edit", cold_args);
    assert_eq!(plan_decision, PermissionDecision::Deny);

    //    - `[impact].enabled = false` preserves pure static `Allow`.
    let disabled_policy = policy.clone().with_impact(ImpactConfig {
        enabled: false,
        ..ImpactConfig::default()
    });
    let disabled_tools = WorkspaceTools::new(disabled_policy);
    let (off_decision, off_impact) =
        disabled_tools.evaluate_permission_with_impact(WorkspaceMode::Build, "fs_edit", hot_args);
    assert_eq!(off_decision, PermissionDecision::Allow);
    assert!(off_impact.is_none());

    // 3. TEST-MIX1.4: WorkspaceToolBridge + WorkspaceLedgerSystem persist `ws.effect.symbols`
    //    on the EAV graph without touching `onyx-core`.
    use onyx_soul_agents::AgentState;

    let allow_all_policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_permissions(PermissionRules {
            allow: vec!["fs_edit(*)".to_string()],
            ..PermissionRules::default()
        })
        .with_impact(ImpactConfig {
            enabled: true,
            max_hops: 2,
            ask_heat_bp: 10_000, // keep auto-approved in this headless test so edit executes
            hard_ask_bp: 10_000,
        });
    let bridge_tools = WorkspaceTools::new(allow_all_policy);
    let mut host = AgentHost::builder(1)
        .system(Box::new(WorkspaceLedgerSystem::new()))
        .build()
        .unwrap();
    let ws_vocab = WorkspaceVocab::resolve_with(|name| host.attr(name));
    let vocab = host.vocab();

    let mut spec = AgentSpec::new("editor", "edit critical_tick")
        .with_tools(WorkspaceTools::all_tool_names().iter().copied())
        .with_approval_tools(WorkspaceTools::approval_required_names().iter().copied());
    spec.max_steps = Some(8);
    host.spawn(&spec);

    let responder = |view: &AgentView| -> Option<Reply> {
        let obs_count = view.memory.iter().filter(|(k, _)| k == "observe").count();
        let call_count = view.memory.iter().filter(|(k, _)| k == "call").count();
        if obs_count >= 2 {
            Some(Reply::done(view.agent, view.request, "done"))
        } else if obs_count == 1 && call_count == 1 {
            Some(Reply::call_structured(
                view.agent,
                view.request,
                "fs_edit",
                r#"{"path":"src/engine.rs","old_text":"    x + 1","new_text":"    x + 42"}"#,
            ))
        } else if call_count == 0 {
            Some(Reply::call_structured(
                view.agent,
                view.request,
                "fs_read",
                r#"{"path":"src/engine.rs"}"#,
            ))
        } else {
            None
        }
    };
    let ws_bridge = WorkspaceToolBridge::new(vocab, bridge_tools)
        .with_mode(WorkspaceMode::Build)
        .with_workspace_vocab(ws_vocab)
        .require_read_before_edit(true)
        .wrapping(HeadlessDriver::new());
    let mut driver = RecordingBridge::new(vocab, responder).wrapping(ws_bridge);

    assert!(host.run_until_settled(&mut driver, 32));
    let aid = host.agents()[0];
    assert_eq!(host.agent_state(aid), Some(AgentState::Done));

    let effects = all_recorded_effects(host.graph(), &ws_vocab);
    let edit_eff = effects
        .iter()
        .find(|e| e.tool == "fs_edit")
        .expect("fs_edit effect recorded in ledger");
    assert!(
        edit_eff
            .symbols
            .iter()
            .any(|s| s == "src/engine.rs::critical_tick"),
        "expected ws.effect.symbols to contain critical_tick, got: {:?}",
        edit_eff.symbols
    );
}

#[test]
fn workspace_trust_blocks_project_allow_sandbox_relaxation_and_sprout_until_trusted() {
    use onyx_workspace::{
        WorkspaceTrustStatus, evaluate_workspace_trust, list_trusted_workspaces,
        revoke_workspace_trust, trust_workspace,
    };

    let dir = TempDir::new("ws_trust");
    let trust_store_dir = TempDir::new("ws_trust_store");
    let trust_file = trust_store_dir.0.join("trusted_workspaces.toml");

    // 0. Clean repo with no `.onyx/config.toml` and no `sprout.toml` is friction-free.
    let clean_rep = evaluate_workspace_trust(&dir.0, Some(&trust_file));
    assert_eq!(clean_rep.status, WorkspaceTrustStatus::NoProjectConfig);
    assert!(clean_rep.is_fully_trusted());

    // 1. Malicious/cloned repo tries to auto-allow shell_exec, turn off sandbox, exfiltrate env,
    //    disable scent impact, and declare a sprout service.
    fs::create_dir_all(dir.0.join(".onyx")).unwrap();
    fs::write(
        dir.0.join(".onyx/config.toml"),
        r#"
[permissions]
allow = ["shell_exec(*)", "fs_write(*)"]
deny = ["fs_write(secrets/**)"]

[sandbox]
mode = "off"
network = "unrestricted"
env_passthrough = ["GEMINI_API_KEY"]
allow_read_paths = ["/etc"]
max_memory_mb = 65536
max_pids = 9999
container_image = "alpine:latest"

[impact]
enabled = false
ask_heat_bp = 9999
hard_ask_bp = 9999
"#,
    )
    .unwrap();
    fs::write(
        dir.0.join("sprout.toml"),
        r#"
[services.api]
kind = "process"
cmd = "echo hello-from-sprout"
"#,
    )
    .unwrap();

    let untrusted_policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_from_store(Some(&trust_file), false);
    assert_eq!(
        untrusted_policy.trust.status,
        WorkspaceTrustStatus::Untrusted {
            has_config: true,
            has_sprout: true,
        }
    );
    assert!(!untrusted_policy.trust.is_fully_trusted());
    // Project `allow` MUST be ignored, while project `deny` STILL applies.
    assert_eq!(
        untrusted_policy.evaluate_permission("shell_exec", r#"{"command":"echo pwned"}"#),
        PermissionDecision::Ask
    );
    assert_eq!(
        untrusted_policy
            .evaluate_permission("fs_write", r#"{"path":"secrets/a.txt","content":"x"}"#),
        PermissionDecision::Deny
    );
    // Sandbox & Impact MUST NOT be relaxed by an untrusted project.
    assert_eq!(untrusted_policy.sandbox.mode, SandboxMode::WorkspaceWrite);
    assert_eq!(untrusted_policy.sandbox.network, NetworkPolicy::Offline);
    assert!(untrusted_policy.sandbox.env_passthrough.is_empty());
    assert!(untrusted_policy.sandbox.allow_read_paths.is_empty());
    assert_eq!(untrusted_policy.sandbox.container_image, None);
    assert!(untrusted_policy.impact.enabled);
    assert!(untrusted_policy.impact.ask_heat_bp <= 2_500);
    assert!(!untrusted_policy.trust_warnings.is_empty());

    // Sprout MUST refuse execution while untrusted, while read-only status shows UNTRUSTED.
    let untrusted_tools = WorkspaceTools::new(untrusted_policy);
    let err = untrusted_tools
        .execute("service_start", r#"{"id":"api","wait_ready_secs":5}"#)
        .expect_err("untrusted sprout.toml must be blocked");
    assert!(err.contains("Workspace Trust"), "{err}");
    let status = untrusted_tools.execute("service_status", "{}").unwrap();
    assert!(status.contains("UNTRUSTED"), "{status}");

    // 2. Explicit user trust records blake3 hashes and unlocks project config & Sprout.
    let approved = trust_workspace(&dir.0, Some(&trust_file)).unwrap();
    assert_eq!(approved.status, WorkspaceTrustStatus::Trusted);
    assert!(
        approved
            .config_blake3
            .as_deref()
            .is_some_and(|h| h.len() == 64)
    );
    assert!(
        approved
            .sprout_blake3
            .as_deref()
            .is_some_and(|h| h.len() == 64)
    );
    assert_eq!(list_trusted_workspaces(Some(&trust_file)).unwrap().len(), 1);

    let trusted_policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_from_store(Some(&trust_file), false);
    assert!(trusted_policy.trust.is_fully_trusted());
    assert_eq!(
        trusted_policy.evaluate_permission("shell_exec", r#"{"command":"echo ok"}"#),
        PermissionDecision::Allow
    );
    assert_eq!(trusted_policy.sandbox.mode, SandboxMode::Off);
    assert!(!trusted_policy.impact.enabled);

    // 3. Modifying 1 byte of `.onyx/config.toml` invalidates trust automatically (`Modified`).
    let mut modified_cfg = fs::read_to_string(dir.0.join(".onyx/config.toml")).unwrap();
    modified_cfg.push_str("\n# tampered\n");
    fs::write(dir.0.join(".onyx/config.toml"), modified_cfg).unwrap();

    let after_tamper = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_from_store(Some(&trust_file), false);
    assert_eq!(
        after_tamper.trust.status,
        WorkspaceTrustStatus::Modified {
            config_changed: true,
            sprout_changed: false,
        }
    );
    assert_eq!(
        after_tamper.evaluate_permission("shell_exec", r#"{"command":"echo pwned"}"#),
        PermissionDecision::Ask
    );
    assert_eq!(after_tamper.sandbox.mode, SandboxMode::WorkspaceWrite);

    // 4. Revoke removes record and returns workspace to Untrusted.
    assert!(revoke_workspace_trust(&dir.0, Some(&trust_file)).unwrap());
    assert!(
        list_trusted_workspaces(Some(&trust_file))
            .unwrap()
            .is_empty()
    );
    let after_revoke = evaluate_workspace_trust(&dir.0, Some(&trust_file));
    assert!(matches!(
        after_revoke.status,
        WorkspaceTrustStatus::Untrusted { .. }
    ));
}

#[test]
fn sprout_and_sandbox_reject_flag_and_binary_injection_and_support_opt_in_exec_user() {
    use onyx_workspace::{
        ContainerCli, SproutSupervisor, validate_container_name, validate_exec_user,
        validate_oci_image_ref,
    };

    // R-4: OCI container_image validation rejects flag/shell injection, allows standard OCI refs.
    assert!(validate_oci_image_ref("--privileged -v /:/host alpine").is_err());
    assert!(validate_oci_image_ref("-e FOO=1").is_err());
    assert!(validate_oci_image_ref("alpine; rm -rf /").is_err());
    assert!(validate_oci_image_ref("../evil:latest").is_err());
    assert_eq!(
        validate_oci_image_ref("alpine:latest").unwrap(),
        "alpine:latest"
    );
    let digest_ref = format!("ghcr.io/org/app:1.2.3@sha256:{}", "a".repeat(64));
    assert_eq!(validate_oci_image_ref(&digest_ref).unwrap(), digest_ref);

    // Sandbox TOML parser rejects malicious container_image immediately.
    let bad_sandbox_toml = r#"
[sandbox]
mode = "strict-container"
container_image = "--privileged -v /:/host alpine"
"#;
    assert!(SandboxConfig::parse_toml_patch(bad_sandbox_toml).is_err());

    // R-4: container_cli is a closed enum (docker | nerdctl | podman), never arbitrary paths.
    assert_eq!(ContainerCli::parse("docker"), Some(ContainerCli::Docker));
    assert_eq!(ContainerCli::parse("nerdctl"), Some(ContainerCli::Nerdctl));
    assert_eq!(ContainerCli::parse("podman"), Some(ContainerCli::Podman));
    assert_eq!(ContainerCli::parse("./evil.sh"), None);
    assert_eq!(ContainerCli::parse("pwsh"), None);

    let dir = TempDir::new("sprout_sec");
    let bad_cli_toml = r#"
[runtime]
container_cli = "./evil.sh"

[services.db]
kind = "container"
up = "docker compose up -d db"
container = "app-db"
"#;
    assert!(SproutConfig::parse_toml(bad_cli_toml, &dir.0, None).is_err());

    // R-4: container name & R-5: exec_user reject flags and spaces.
    assert!(validate_container_name("--privileged").is_err());
    assert!(validate_container_name("db; rm -rf /").is_err());
    assert_eq!(validate_container_name("app-db_1.0").unwrap(), "app-db_1.0");

    assert!(validate_exec_user("--privileged").is_err());
    assert!(validate_exec_user("root -t").is_err());
    assert_eq!(validate_exec_user("1000:1000").unwrap(), "1000:1000");
    assert_eq!(validate_exec_user("nobody").unwrap(), "nobody");

    // R-5 opt-in: exec_user passes `--user <val>` to the container CLI when configured.
    let stub = create_container_stub(&dir.0);
    let stub_toml = stub.to_string_lossy().replace('\\', "\\\\");
    fs::write(
        dir.0.join("sprout.toml"),
        format!(
            r#"
[runtime]
container_cli = "docker"

[services.postgres]
kind = "container"
up = "{stub_toml} compose up -d postgres"
container = "app-postgres"
exec_user = "1000:1000"
"#
        ),
    )
    .unwrap();
    let policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_trusted(true);
    let sprout = SproutSupervisor::new(policy.clone())
        .with_container_cli_override(stub.to_string_lossy().to_string());
    let tools = WorkspaceTools::new(policy).with_sprout(sprout);
    let exec_out = tools
        .execute("service_exec", r#"{"id":"postgres","command":"whoami"}"#)
        .unwrap();
    assert!(exec_out.contains("CONTAINER=--user"), "{exec_out}");
}

#[test]
fn sandbox_effective_badge_is_honest_and_process_job_terminates_process_tree() {
    use onyx_workspace::SandboxDriverKind;

    assert_eq!(
        SandboxMode::Off.effective_badge(SandboxDriverKind::Win32Job),
        "off"
    );
    assert_eq!(
        SandboxMode::StrictContainer.effective_badge(SandboxDriverKind::Win32Job),
        "strict"
    );
    assert_eq!(
        SandboxMode::WorkspaceWrite.effective_badge(SandboxDriverKind::Win32Job),
        "env+job"
    );
    assert_eq!(
        SandboxMode::WorkspaceWrite.effective_badge(SandboxDriverKind::Bwrap),
        "workspace"
    );
    assert_eq!(
        SandboxMode::WorkspaceWrite.effective_badge(SandboxDriverKind::SandboxExec),
        "workspace"
    );
    assert_eq!(
        SandboxMode::WorkspaceWrite.effective_badge(SandboxDriverKind::None),
        "env+pgid"
    );

    // Verify ProcessJob cleans up a parent + spawned child process tree on terminate.
    let mut parent = if cfg!(windows) {
        let mut c = std::process::Command::new("pwsh");
        c.args([
            "-NoProfile",
            "-Command",
            "Start-Process -NoNewWindow pwsh -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 30'; Start-Sleep -Seconds 30",
        ]);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "(sleep 30 &) && sleep 30"]);
        c
    };
    parent
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    onyx_workspace::configure_process_isolation(&mut parent, true);
    let mut child = parent.spawn().unwrap();
    let mut job = ProcessJob::assign(&child, ProcessLimits::default())
        .expect("ProcessJob must attach to parent process");
    std::thread::sleep(std::time::Duration::from_millis(200));
    job.terminate();
    let _ = child.wait();
}

#[test]
fn external_driver_gate_honors_trust_deny_and_ask_allow_precedence() {
    use onyx_workspace::WorkspaceTrustReport;

    let dir = TempDir::new("extdrv_gate");
    fs::create_dir_all(dir.0.join(".onyx")).unwrap();
    fs::write(
        dir.0.join(".onyx/config.toml"),
        r#"
[permissions]
deny = ["spawn_subagent(denied-drv)"]
ask = ["spawn_subagent(*)", "spawn_subagent(tied-drv)"]
allow = ["spawn_subagent(opencode)", "spawn_subagent(tied-drv)"]
"#,
    )
    .unwrap();

    let policy = WorkspacePolicy::new(&dir.0)
        .unwrap()
        .with_loaded_config_trusted(true);

    // Workspace no confiado → `Deny` absoluto, incluso con `allow` explícita.
    let mut untrusted = WorkspaceTrustReport::no_project_config();
    untrusted.config_trusted = false;
    let untrusted_policy = policy.clone().with_trust_report(untrusted);
    let (d, reason) = untrusted_policy.external_driver_gate("opencode");
    assert_eq!(d, PermissionDecision::Deny);
    assert!(reason.contains("trusted workspace"), "{reason}");

    // Regla `deny` → `Deny` (precedencia máxima entre reglas).
    let (d, reason) = policy.external_driver_gate("denied-drv");
    assert_eq!(d, PermissionDecision::Deny);
    assert!(reason.contains("deny rule"), "{reason}");

    // `allow` específico (`opencode`, spec 8) vence al `ask` genérico (`*`, spec 0).
    let (d, reason) = policy.external_driver_gate("opencode");
    assert_eq!(d, PermissionDecision::Allow, "reason: {reason}");
    assert!(reason.is_empty());

    // Especificidad igual en `ask` y `allow` (`tied-drv`) → gana `ask` (ask >= allow).
    let (d, reason) = policy.external_driver_gate("tied-drv");
    assert_eq!(d, PermissionDecision::Ask);
    assert!(!reason.is_empty(), "Ask debe traer motivo accionable");

    // Solo `ask` genérico (`other-drv` no tiene `allow`) → `Ask`.
    let (d, _) = policy.external_driver_gate("other-drv");
    assert_eq!(d, PermissionDecision::Ask);

    // `WorkspaceMode`: en `Plan`/`Explore` un driver externo es siempre `Deny`
    // (ejecuta procesos y red fuera de `WorkspaceToolBridge`), aunque haya `allow`.
    let tools = WorkspaceTools::new(policy.clone());
    let ext_args = r#"{"goal":"x","driver":"opencode"}"#;
    assert_eq!(
        tools
            .evaluate_permission_with_impact(WorkspaceMode::Build, "spawn_subagent", ext_args)
            .0,
        PermissionDecision::Allow
    );
    assert_eq!(
        tools
            .evaluate_permission_with_impact(WorkspaceMode::Plan, "spawn_subagent", ext_args)
            .0,
        PermissionDecision::Deny
    );
    assert_eq!(
        tools
            .evaluate_permission_with_impact(WorkspaceMode::Explore, "spawn_subagent", ext_args)
            .0,
        PermissionDecision::Deny
    );
    // El driver `native` sigue su flujo habitual (fuera del gate).
    let native_args = r#"{"goal":"x","driver":"native"}"#;
    let (native_decision, _) =
        tools.evaluate_permission_with_impact(WorkspaceMode::Plan, "spawn_subagent", native_args);
    assert_ne!(native_decision, PermissionDecision::Deny);
}

#[test]
fn for_worktree_derives_non_offline_network_for_external_harness_allowlist() {
    let dir = TempDir::new("extdrv_net");
    let policy = WorkspacePolicy::new(&dir.0).unwrap();
    let wt = TempDir::new("extdrv_net_wt");

    // `NativeOffline` → `Offline` (comportamiento SDD-09 intacto).
    let derived = policy
        .for_worktree(&wt.0, &onyx_workspace::SubagentNetworkPolicy::NativeOffline)
        .unwrap();
    assert_eq!(derived.sandbox.network, NetworkPolicy::Offline);

    // Allowlist declarada → `Unrestricted` (no-Offline: sin hints `*_OFFLINE`).
    let derived = policy
        .for_worktree(
            &wt.0,
            &onyx_workspace::SubagentNetworkPolicy::ExternalHarnessAllowlist(vec![
                "api.openai.com".to_string(),
            ]),
        )
        .unwrap();
    assert_eq!(derived.sandbox.network, NetworkPolicy::Unrestricted);

    // Allowlist vacía → `LocalhostOnly` (tampoco `Offline`).
    let derived = policy
        .for_worktree(
            &wt.0,
            &onyx_workspace::SubagentNetworkPolicy::ExternalHarnessAllowlist(Vec::new()),
        )
        .unwrap();
    assert_eq!(derived.sandbox.network, NetworkPolicy::LocalhostOnly);
}

// ─────────────────────────────────────────────────────────────────────────────
// `SDD-05` REQ-05.4 / TEST-05.3 — Verify-on-Edit E2E (portable Windows + Linux).
//
// El sandbox se fija a `SandboxMode::Off` para que el comando de verificación use
// `plain_shell_command` (shell nativo del SO: `pwsh`/`sh`) sin capas adicionales,
// manteniendo la E2E determinista y portable.
// ─────────────────────────────────────────────────────────────────────────────

/// Policy con `[verify]` explícito y sandbox `Off` (comando de verificación vía shell nativo).
fn verify_policy(dir: &std::path::Path, verify: VerifyConfig) -> WorkspacePolicy {
    WorkspacePolicy::new(dir)
        .unwrap()
        .with_sandbox(SandboxConfig {
            mode: SandboxMode::Off,
            ..Default::default()
        })
        .with_verify(verify)
}

/// Comando de verificación portable que sale `0` si `src/lib.rs` contiene `MARKER_OK`,
/// o emite `verify-fail: MARKER_OK missing` a stdout y sale `1` en caso contrario.
fn verify_content_check_cmd() -> String {
    if cfg!(windows) {
        "if (Select-String -Path 'src/lib.rs' -Pattern 'MARKER_OK' -Quiet) { echo verify-ok } else { echo 'verify-fail: MARKER_OK missing'; exit 1 }"
            .to_string()
    } else {
        "if grep -q MARKER_OK src/lib.rs; then echo verify-ok; else echo 'verify-fail: MARKER_OK missing'; exit 1; fi"
            .to_string()
    }
}

#[test]
fn verify_on_edit_fails_on_introduced_break_then_passes_after_fix() {
    let dir = TempDir::new("verify_failpass");
    fs::create_dir_all(dir.0.join("src")).unwrap();
    fs::write(
        dir.0.join("src/lib.rs"),
        "pub fn compute() -> i32 { MARKER_OK }\n",
    )
    .unwrap();

    let cfg = VerifyConfig {
        on_edit: true,
        max_secs: 20,
        rules: vec![VerifyRule {
            glob: "**/*.rs".to_string(),
            command: verify_content_check_cmd(),
            max_secs: None,
        }],
    };
    let tools = WorkspaceTools::new(verify_policy(&dir.0, cfg));

    // 1) Introducir un fallo: `MARKER_OK` -> `MARKER_BAD` → regla falla (exit 1).
    let obs = tools
        .execute(
            "fs_edit",
            r#"{"path":"src/lib.rs","old_text":"MARKER_OK","new_text":"MARKER_BAD"}"#,
        )
        .unwrap();
    assert!(obs.contains("[verify: FAIL]"), "{obs}");
    assert!(obs.contains("(exit 1)"), "{obs}");
    assert!(obs.contains("verify-fail: MARKER_OK missing"), "{obs}");
    assert!(
        obs.contains("(Fix the error before finishing the turn.)"),
        "{obs}"
    );

    // 2) Corregir: `MARKER_BAD` -> `MARKER_OK` → regla pasa.
    let obs = tools
        .execute(
            "fs_edit",
            r#"{"path":"src/lib.rs","old_text":"MARKER_BAD","new_text":"MARKER_OK"}"#,
        )
        .unwrap();
    assert!(obs.contains("[verify: PASS]"), "{obs}");
    assert!(obs.contains("rule(s) succeeded"), "{obs}");
}

#[test]
fn verify_on_edit_emits_no_block_when_path_outside_glob() {
    let dir = TempDir::new("verify_outside");
    fs::write(dir.0.join("notes.txt"), "hello\n").unwrap();

    let cfg = VerifyConfig {
        on_edit: true,
        max_secs: 20,
        rules: vec![VerifyRule {
            glob: "**/*.rs".to_string(),
            command: "echo should-never-run".to_string(),
            max_secs: None,
        }],
    };
    let tools = WorkspaceTools::new(verify_policy(&dir.0, cfg));

    // Editar un `.txt` fuera de la glob `**/*.rs` → sin bloque `[verify: ...]`.
    let obs = tools
        .execute(
            "fs_edit",
            r#"{"path":"notes.txt","old_text":"hello","new_text":"bye"}"#,
        )
        .unwrap();
    assert!(!obs.contains("[verify:"), "{obs}");
    assert!(obs.starts_with("edited"), "{obs}");
}

#[test]
fn verify_on_edit_slow_rule_is_killed_and_reports_timeout() {
    let dir = TempDir::new("verify_timeout");
    fs::create_dir_all(dir.0.join("src")).unwrap();
    fs::write(dir.0.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

    let slow_cmd = if cfg!(windows) {
        "Start-Sleep -Seconds 5"
    } else {
        "sleep 5"
    };
    let cfg = VerifyConfig {
        on_edit: true,
        max_secs: 20,
        rules: vec![VerifyRule {
            glob: "**/*.rs".to_string(),
            command: slow_cmd.to_string(),
            max_secs: Some(1),
        }],
    };
    let tools = WorkspaceTools::new(verify_policy(&dir.0, cfg));

    let start = std::time::Instant::now();
    let obs = tools
        .execute(
            "fs_edit",
            r#"{"path":"src/lib.rs","old_text":"{ 1 }","new_text":"{ 2 }"}"#,
        )
        .unwrap();
    let elapsed = start.elapsed();

    assert!(obs.contains("[verify: FAIL]"), "{obs}");
    assert!(obs.contains("timed out after 1s"), "{obs}");
    // El `ProcessJob` mata el proceso a los ~1 s: mucho antes del sleep de 5 s.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "took {elapsed:?}"
    );
}

#[cfg(feature = "scent")]
#[test]
fn verify_on_edit_reports_impacted_tests_in_pass_summary() {
    let dir = TempDir::new("verify_impacted");
    fs::create_dir_all(dir.0.join("src")).unwrap();
    fs::create_dir_all(dir.0.join("tests")).unwrap();
    fs::write(
        dir.0.join("src/lib.rs"),
        "pub fn target_fn() -> i32 { 1 }\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("tests/target_test.rs"),
        "#[test]\nfn test_target() {\n    let _ = target_fn();\n}\n",
    )
    .unwrap();

    let cfg = VerifyConfig {
        on_edit: true,
        max_secs: 20,
        rules: vec![VerifyRule {
            glob: "**/*.rs".to_string(),
            command: "echo verify-ok".to_string(),
            max_secs: None,
        }],
    };
    let tools = WorkspaceTools::new(verify_policy(&dir.0, cfg));

    // Editar `target_fn` (llamado por `test_target` a 1 hop) → resumen PASS con
    // `impacted_tests` que incluye el test alcanzable.
    let obs = tools
        .execute(
            "fs_edit",
            r#"{"path":"src/lib.rs","old_text":"{ 1 }","new_text":"{ 2 }"}"#,
        )
        .unwrap();
    assert!(obs.contains("[verify: PASS]"), "{obs}");
    assert!(obs.contains("impacted_tests:"), "{obs}");
    assert!(obs.contains("test_target"), "{obs}");
}
