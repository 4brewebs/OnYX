use onyx_soul_agents::{
    AgentConfig, AgentHost, AgentSpec, AgentState, AgentView, HeadlessDriver, RecordingBridge,
    ReplayBridge, Reply, SPAWN_AGENT_TOOL, SubagentSystem,
};

#[test]
fn parent_suspends_in_waiting_children_and_resumes_with_replay_equivalence() {
    let config = AgentConfig {
        default_timeout_ticks: 4,
        default_max_steps: 8,
        ..AgentConfig::default()
    };

    let mut host = AgentHost::builder(42)
        .config(config.clone())
        .system(Box::new(SubagentSystem::new(config.clone())))
        .build()
        .unwrap();

    let spec = AgentSpec::new("parent", "coordinate child")
        .with_tools([SPAWN_AGENT_TOOL, "read_only"])
        .with_timeout_ticks(2);
    host.spawn(&spec);

    let mut seen_waiting_children = false;
    let mut driver = RecordingBridge::new(host.vocab(), |req: &AgentView| {
        if req.name == "parent" {
            if let Some((_, obs)) = req.memory.iter().find(|(k, _)| k == "observe") {
                return Some(Reply::done(
                    req.agent,
                    req.request,
                    format!("merged: {obs}"),
                ));
            }
            return Some(Reply::call_structured(
                req.agent,
                req.request,
                SPAWN_AGENT_TOOL,
                r#"{"name":"worker","goal":"inspect module"}"#,
            ));
        }
        // Child agent ("worker"): verifies it inherited `read_only` but not `spawn_agent`.
        assert!(req.tools.contains(&"read_only".to_string()));
        assert!(!req.tools.contains(&SPAWN_AGENT_TOOL.to_string()));
        Some(Reply::done(req.agent, req.request, "child-ok"))
    })
    .wrapping(HeadlessDriver::new());

    for _ in 0..32 {
        host.step(&mut driver);
        if let Some(pid) = host.agents().first().copied() {
            if host.agent_state(pid) == Some(AgentState::WaitingChildren) {
                seen_waiting_children = true;
            }
        }
        if host.is_settled() {
            break;
        }
    }

    assert!(seen_waiting_children, "parent must enter WaitingChildren");
    assert!(host.is_settled());
    let parent_id = host.agents()[0];
    assert_eq!(host.agent_state(parent_id), Some(AgentState::Done));
    let final_result = host.result(parent_id).unwrap();
    assert!(final_result.contains("child-ok"), "{final_result}");

    // Replay determinista: mismo resultado y mismo fingerprint sin responder vivo.
    let transcript = driver.transcript().clone();
    let mut replay_host = AgentHost::builder(42)
        .config(config.clone())
        .system(Box::new(SubagentSystem::new(config)))
        .build()
        .unwrap();
    replay_host.spawn(&spec);

    let mut replay_driver = ReplayBridge::new(transcript).wrapping(HeadlessDriver::new());
    assert!(replay_host.run_until_settled(&mut replay_driver, 64));
    assert_eq!(replay_host.fingerprint(), host.fingerprint());
}
