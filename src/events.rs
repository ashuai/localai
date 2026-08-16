//! 应用级事件(类型化事件总线,见 cordis::event)。

use crate::cordis::event::Event;

/// 用户输入:用户在 TUI 输入框提交,或 `--once` 模式注入。
pub struct SessionInput {
    pub text: String,
}
impl Event for SessionInput {
    fn name(&self) -> &'static str {
        "session/input"
    }
}

/// 助手回复:chat 插件在后台线程完成 LLM 调用后发出。
pub struct SessionReply {
    pub text: String,
    /// 对应的用户原文(microtask 等插件做环境微调用时使用)
    pub user_text: String,
}
impl Event for SessionReply {
    fn name(&self) -> &'static str {
        "session/reply"
    }
}

/// 状态行:任意插件向 TUI 状态区发一行文本。
pub struct SessionStatus {
    pub text: String,
}
impl Event for SessionStatus {
    fn name(&self) -> &'static str {
        "session/status"
    }
}
