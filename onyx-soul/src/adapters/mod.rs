//! Adaptadores de plataforma del soul default.

pub mod headless;
pub mod prompt;
//pub mod tui;

pub use headless::HeadlessDriver;
pub use prompt::PromptDriver;
//pub use tui::TuiDriver;
