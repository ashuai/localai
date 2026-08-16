//! TUI 交互(ratatui)。非交互模式(`--once` / `--list-plugins`)在 main.rs。

pub mod app;
pub use app::run;
