//! Capacidades de código, estado en grafo EAV y sandbox de workspace para harnesses sobre `onyx-soul-agents`.

pub mod bridge;
pub mod policy;
pub mod state;
pub mod tools;

pub use bridge::WorkspaceToolBridge;
pub use policy::{
    PermissionDecision, PermissionRules, WorkspacePolicy, compile_glob_regex, is_mutating_tool,
};
pub use state::{
    EffectReversibility, PendingCheckpoints, RecordedEffect, WorkspaceLedgerSystem, WorkspaceMode,
    WorkspaceModeHandle, WorkspaceVocab, all_recorded_effects, capture_git_tree_checkpoint,
    effects_after_tick, graph_workspace_mode, graph_workspace_plan, is_effect_undone, is_file_read,
    latest_undoable_effect, mark_latest_effect_undone, restore_git_tree_checkpoint,
    set_workspace_mode, set_workspace_plan, undone_effects,
};
pub use tools::{
    FsEditHunk, PlanStepItem, ToolSchema, WorkspaceTools, discover_skills_summary,
    format_plan_steps, preview_tool_diff,
};
