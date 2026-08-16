//! 内置插件清单(loader 的插件注册表)。

pub mod chat;
pub mod memory;
pub mod microtask;
pub mod tools;
pub mod tui;

use crate::cordis::loader::PluginFactory;

/// 内置插件工厂列表(对应 cordis 的插件包清单)。
pub fn builtin() -> Vec<PluginFactory> {
    vec![memory::factory, chat::factory, microtask::factory, tools::factory, tui::factory]
}
