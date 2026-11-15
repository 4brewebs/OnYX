//! Capacidades de código, estado en grafo EAV y sandbox de workspace para harnesses sobre `onyx-soul-agents`.

pub mod bridge;
pub mod policy;
pub mod state;
pub mod tools;

pub use bridge::WorkspaceToolBridge;
pub use policy::WorkspacePolicy;
pub use state::{
    EffectReversibility, PendingCheckpoints, RecordedEffect, WorkspaceLedgerSystem, WorkspaceMode,
    WorkspaceVocab, capture_git_tree_checkpoint, effects_after_tick, is_file_read,
    restore_git_tree_checkpoint,
};
pub use tools::{ToolSchema, WorkspaceTools};
