//! Capacidades de código y sandbox de workspace para harnesses sobre `onyx-soul-agents`.

pub mod bridge;
pub mod policy;
pub mod tools;

pub use bridge::WorkspaceToolBridge;
pub use policy::WorkspacePolicy;
pub use tools::{ToolSchema, WorkspaceTools};
