//! 模型层:核心服务 `llm` + OpenAI 兼容客户端 + 微调用协议。
//!
//! 设计论证详见 `docs/model-layer.md`。

pub mod client;
pub mod micro;

pub use client::{ChatMessage, ChatRequest, ChatResponse, ChatUsage, LlmClient, LlmConfig};
pub use micro::{extract_json, MicroEngine, MicroOutcome, MicroTask};

use crate::cordis::service::Service;

/// 核心服务:所有插件通过 `ctx.inject::<LlmService>()` 拿到共享客户端。
/// 由 Loader 在启动时注入根作用域。
pub struct LlmService {
    pub client: LlmClient,
}

impl Service for LlmService {
    fn service_name_static() -> &'static str {
        "llm"
    }
}
