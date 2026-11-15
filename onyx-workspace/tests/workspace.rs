use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use onyx_soul_agents::{
    AgentHost, AgentSpec, AgentView, HeadlessDriver, RecordingBridge, Reply,
};
use onyx_workspace::{WorkspacePolicy, WorkspaceToolBridge, WorkspaceTools};

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
        .execute("fs_read", r#"{"path":"src/lib.rs","start_line":2,"end_line":2}"#)
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
