//! localai —— DSH 式 cordis 插件模式的 Rust 本地 LLM 壳。
//!
//! 结构:
//! - [`cordis`]:插件化核心(Context / 事件 / Service / Plugin / Loader)
//! - [`llm`]:模型层(OpenAI 兼容客户端 + 微调用协议)
//! - [`plugins`]:内置插件(chat / microtask)
//! - [`tui`]:TUI 交互
//! - [`events`]:应用级事件

pub mod cordis;
pub mod events;
pub mod llm;
pub mod plugins;
pub mod tui;
