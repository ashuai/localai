use std::any::Any;

/// 事件。cordis 以字符串名注册事件;Rust 版以事件类型为键,
/// `name()` 仅用于展示/日志。
pub trait Event: Any + Send {
    fn name(&self) -> &'static str;
}
