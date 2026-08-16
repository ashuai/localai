//! 内置插件清单(loader 的插件注册表)。

pub mod chat;
pub mod microtask;
pub mod tui;

use crate::cordis::loader::PluginFactory;

/// 内置插件工厂列表(对应 cordis 的插件包清单)。
pub fn builtin() -> Vec<PluginFactory> {
    vec![chat::factory, microtask::factory, tui::factory]
}
