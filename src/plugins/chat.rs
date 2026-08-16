//! chat 插件:聊天服务。演示 cordis 插件的完整形态:
//! 提供服务(chat)+ 订阅事件(session/input)+ 注册命令(/chat)+ 卸载自动回滚。

use crate::cordis::context::Context;
use crate::cordis::plugin::Plugin;
use crate::cordis::service::Service;
use crate::events::{SessionInput, SessionReply, SessionStatus};
use crate::llm::{ChatMessage, ChatRequest, LlmClient, LlmService};
use std::sync::{Arc, Mutex};

/// 固定系统提示词(稳定 → 吃 oMLX prompt 缓存)
const CHAT_SYSTEM: &str = "你是 localai,一个运行在本地的小型 AI 助手。回答保持简洁、直接、准确,使用与用户相同的语言。";

pub fn factory() -> Box<dyn Plugin> {
    Box::new(ChatPlugin)
}

pub struct ChatPlugin;

#[derive(serde::Deserialize, Default)]
pub struct ChatOptions {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub enable_thinking: bool,
    #[serde(default = "default_turns")]
    pub history_turns: usize,
}

fn default_turns() -> usize {
    6
}

pub struct ChatService {
    client: LlmClient,
    /// 有界对话历史(上下文纪律:只保留最近 N 轮)
    history: Mutex<Vec<ChatMessage>>,
    enable_thinking: bool,
    history_turns: usize,
}

impl Service for ChatService {
    fn service_name_static() -> &'static str {
        "chat"
    }
}

impl ChatService {
    pub fn ask(&self, user_text: &str) -> anyhow::Result<String> {
        let messages = {
            let mut h = self.history.lock().unwrap();
            h.push(ChatMessage { role: "user".into(), content: user_text.to_string() });
            let keep = self.history_turns.saturating_mul(2).saturating_add(1); // N 轮 × 2 + system
            if h.len() > keep {
                let drop_n = h.len() - keep;
                h.drain(0..drop_n);
            }
            h.clone()
        };
        let req = ChatRequest {
            model: self.client.model(),
            messages,
            max_tokens: 900,
            json: false,
            enable_thinking: self.enable_thinking,
        };
        let resp = self.client.chat(&req)?;
        {
            let mut h = self.history.lock().unwrap();
            h.push(ChatMessage { role: "assistant".into(), content: resp.content.clone() });
        }
        Ok(resp.content)
    }

    pub fn model(&self) -> String {
        self.client.model()
    }
}

impl Plugin for ChatPlugin {
    fn name(&self) -> &'static str {
        "chat"
    }

    fn inject(&self) -> &'static [&'static str] {
        &["llm"]
    }

    fn apply(&self, ctx: &Context) -> anyhow::Result<()> {
        let llm = ctx.inject::<LlmService>().ok_or_else(|| anyhow::anyhow!("缺少 llm 服务"))?;
        let opts: ChatOptions = ctx.options()?;
        let client = llm.client.clone();
        if let Some(m) = &opts.model {
            client.set_model(m.clone());
        }
        let svc = Arc::new(ChatService {
            client,
            history: Mutex::new(vec![ChatMessage { role: "system".into(), content: CHAT_SYSTEM.into() }]),
            enable_thinking: opts.enable_thinking,
            history_turns: opts.history_turns,
        });
        ctx.provide(Arc::clone(&svc));

        // 订阅用户输入:后台线程调 LLM,完成后发 session/reply
        let svc2 = Arc::clone(&svc);
        let ctx2 = ctx.clone();
        ctx.on(move |ev: &SessionInput| {
            let svc = Arc::clone(&svc2);
            let ctx = ctx2.clone();
            let text = ev.text.clone();
            let started = std::time::Instant::now();
            ctx.emit(SessionStatus {
                text: format!("[chat] 已提交,调用 {} ...", svc.model()),
            });
            std::thread::spawn(move || match svc.ask(&text) {
                Ok(reply) => {
                    ctx.emit(SessionStatus {
                        text: format!("[chat] 回复完成 ({:?})", started.elapsed()),
                    });
                    ctx.emit(SessionReply { text: reply, user_text: text });
                }
                Err(e) => ctx.emit(SessionStatus {
                    text: format!("[chat] 错误: {e:#}"),
                }),
            });
        });

        // 同步命令 `/chat <text>`(脚本/调试用)
        let svc3 = Arc::clone(&svc);
        ctx.on_command("chat", move |rest: &str| match svc3.ask(rest) {
            Ok(r) => r,
            Err(e) => format!("错误: {e:#}"),
        });

        Ok(())
    }
}
