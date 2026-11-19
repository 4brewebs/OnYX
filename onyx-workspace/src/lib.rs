//! Capacidades de código, estado en grafo EAV y sandbox de workspace para harnesses sobre `onyx-soul-agents`.

pub mod bridge;
pub mod policy;
pub mod sandbox;
pub mod sprout;
pub mod state;
pub mod tools;

pub use bridge::WorkspaceToolBridge;
pub use policy::{
    PermissionDecision, PermissionRules, WorkspacePolicy, compile_glob_regex,
    is_control_plane_rel_path, is_mutating_tool,
};
pub use sandbox::{
    NetworkPolicy, ProcessJob, ProcessLimits, SandboxCapabilities, SandboxConfig,
    SandboxDriverKind, SandboxMode, SandboxPatch, apply_sanitized_env, clean_process_path,
    configure_process_isolation, detect_container_cli, detect_sandbox_capabilities,
    ensure_sandbox_tmp_dir, is_sensitive_env_key, prepare_sandboxed_shell_command,
};
pub use sprout::{
    ServiceKind, ServiceStatus, SproutAudit, SproutConfig, SproutServiceSpec, SproutSupervisor,
    audit_sprout_workspace, format_services_panel_from_toon,
};
pub use state::{
    EffectReversibility, PendingCheckpoints, RecordedEffect, WorkspaceLedgerSystem, WorkspaceMode,
    WorkspaceModeHandle, WorkspaceVocab, all_recorded_effects, capture_git_tree_checkpoint,
    effects_after_tick, graph_workspace_mode, graph_workspace_plan, graph_workspace_services,
    is_effect_undone, is_file_read, latest_undoable_effect, mark_latest_effect_undone,
    restore_git_tree_checkpoint, set_workspace_mode, set_workspace_plan, set_workspace_services,
    undone_effects,
};
pub use tools::{
    FsEditHunk, PlanStepItem, ToolSchema, WorkspaceTools, discover_skills_summary,
    format_plan_steps, preview_tool_diff,
};
