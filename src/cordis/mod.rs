//! cordis 核心 —— DSH 式插件化架构的 Rust 映射。
//!
//! 与 `@deepseek-ai/cordis`(koishi 系)的概念对照:
//!
//! | cordis (TS)                 | localai (Rust)                        |
//! |-----------------------------|---------------------------------------|
//! | `Context` 事件总线 + DI     | [`context::Context`]                  |
//! | `ctx.on / ctx.emit / ctx.bail` | [`context::Context::on`] / `emit` / `bail` |
//! | `ctx.effect(fn)` 生命周期   | [`context::Context::effect`] + `dispose` |
//! | `ctx.provide / ctx.inject`  | [`context::Context::provide`] / `inject` |
//! | `Service` 懒加载单例        | [`service::Service`]                  |
//! | `Plugin { name, inject, apply }` | [`plugin::Plugin`]                |
//! | Loader 读取 cordis.yml 插拔 | [`loader::Loader`]                    |
//!
//! 核心哲学:一切皆插件,内核只负责插拔。卸载插件时,其注册的所有
//! 事件监听、命令、服务都通过 effect 逆序自动回滚。

pub mod context;
pub mod event;
pub mod loader;
pub mod plugin;
pub mod service;

pub use context::Context;
pub use event::Event;
pub use loader::{LoadedPlugin, Loader, PluginFactory, PluginStatus};
pub use plugin::Plugin;
pub use service::Service;
