//! Capacidades de código, estado en grafo EAV y sandbox de workspace para harnesses sobre `onyx-soul-agents`.

pub mod bridge;
pub mod external_driver;
pub mod impact;
pub mod policy;
pub mod sandbox;
pub mod sprout;
pub mod state;
pub mod tools;
pub mod trust;
pub mod worktree;

pub use bridge::WorkspaceToolBridge;
pub use external_driver::{
    builtin_external_driver_presets, load_external_drivers, merge_external_drivers,
    parse_external_drivers_toml,
};
pub use impact::{
    ImpactAssessment, ImpactConfig, ImpactPatch, apply_impact_escalation,
    extract_unified_diff_line_ranges,
};
pub use policy::{
    MergeDeltaVerdict, PermissionDecision, PermissionRules, WorkspacePolicy, classify_merge_delta,
    compile_glob_regex, is_control_plane_rel_path, is_governance_rel_path, is_mutating_tool,
};
pub use sandbox::{
    ContainerCli, NetworkPolicy, ProcessJob, ProcessLimits, ResolvedProcessBinary,
    SandboxCapabilities, SandboxConfig, SandboxDriverKind, SandboxMode, SandboxPatch,
    apply_sanitized_env, apply_sanitized_env_for_external_harness, clean_process_path,
    configure_process_isolation, detect_container_cli, detect_sandbox_capabilities,
    ensure_sandbox_tmp_dir, is_sensitive_env_key, prepare_sandboxed_shell_command,
    resolve_process_binary, resolve_process_binary_in, validate_oci_image_ref,
};
pub use sprout::{
    ServiceKind, ServiceStatus, SproutAudit, SproutConfig, SproutServiceSpec, SproutSupervisor,
    audit_sprout_workspace, format_services_panel_from_toon, validate_container_name,
    validate_exec_user,
};
pub use state::{
    EffectReversibility, PendingCheckpoints, RecordedEffect, RecordedSubagent,
    WorkspaceLedgerSystem, WorkspaceMode, WorkspaceModeHandle, WorkspaceVocab,
    all_recorded_effects, capture_git_tree_checkpoint, effects_after_tick, graph_subagent_records,
    graph_workspace_mode, graph_workspace_plan, graph_workspace_services, is_effect_undone,
    is_file_read, latest_undoable_effect, mark_latest_effect_undone, restore_git_tree_checkpoint,
    restore_git_tree_checkpoint_for_targets, set_workspace_mode, set_workspace_plan,
    set_workspace_services, undone_effects, update_subagent_status, upsert_subagent_record,
};
pub use tools::{
    FsEditHunk, PlanStepItem, ToolSchema, WorkspaceTools, discover_skills_summary,
    format_plan_steps, is_async_subagent_tool, preview_tool_diff, scrub_secrets,
    spawn_subagent_driver_from_args,
};
pub use trust::{
    ONYX_TRUST_FILE_ENV, WorkspaceTrustRecord, WorkspaceTrustReport, WorkspaceTrustStatus,
    default_trust_store_path, evaluate_workspace_trust, hash_file_blake3, list_trusted_workspaces,
    normalize_workspace_key, revoke_workspace_trust, trust_workspace,
};
pub use worktree::{
    ExternalDriverConfig, GitVersion, MIN_GIT_VERSION, ONYX_WT_DIR_ENV, PromptTransport,
    SubagentNetworkPolicy, SubagentProposal, WorktreeHandle, WorktreeInfo, WorktreeManager,
    WorktreeMergeOutcome, check_git_version, parse_git_version, resolve_worktree_base_dir,
    validate_job_id,
};
