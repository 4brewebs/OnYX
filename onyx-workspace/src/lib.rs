//! Capacidades de código, estado en grafo EAV y sandbox de workspace para harnesses sobre `onyx-soul-agents`.

pub mod bridge;
pub mod policy;
pub mod state;
pub mod tools;

pub use bridge::WorkspaceToolBridge;
pub use policy::WorkspacePolicy;
pub use state::{
    EffectReversibility, PendingCheckpoints, RecordedEffect, WorkspaceLedgerSystem, WorkspaceMode,
    WorkspaceModeHandle, WorkspaceVocab, capture_git_tree_checkpoint, effects_after_tick,
    graph_workspace_mode, is_file_read, latest_undoable_effect, mark_latest_effect_undone,
    restore_git_tree_checkpoint, set_workspace_mode, undone_effects,
};
pub use tools::{ToolSchema, WorkspaceTools, preview_tool_diff};
