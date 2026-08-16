//! 交互层渲染(ratatui)。
//!
//! 注意:交互编排(主循环/命令/事件订阅)属于 `tui` 插件(`src/plugins/tui.rs`),
//! 本模块只提供纯渲染状态 [`app::App`]。论证见本地文档 `localai-docs/tui-plugin.md`。

pub mod app;

pub use app::App;
