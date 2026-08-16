//! chat 插件:聊天服务。演示 cordis 插件的完整形态:
//! 提供服务(chat)+ 订阅事件(session/input)+ 注册命令(/chat)+ 卸载自动回滚。

use crate::cordis::context::Context;
use crate::cordis::plugin::Plugin;
use crate::cordis::service::Service;
use crate::events::{SessionInput, SessionReply, SessionStatus};
use crate::llm::{ChatMessage, ChatRequest, LlmClient, LlmService};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// 当前进行中的调用令牌(None=空闲)。Esc 取消=置 true,回复丢弃;
    /// 完成后若令牌仍指向自己则清空(允许新调用登记)。
    current: Mutex<Option<Arc<AtomicBool>>>,
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

    /// 是否有调用正在进行(状态栏显示 ● 调用中)。
    pub fn is_busy(&self) -> bool {
        self.current.lock().unwrap().is_some()
    }

    /// 中断当前调用(若有)。返回是否确有调用被中断;取消是"粘性"的,
    /// 调用线程完成后检测到标志会把回复丢弃,只发一条 `[chat] 已取消`。
    pub fn cancel_current(&self) -> bool {
        let mut cur = self.current.lock().unwrap();
        match cur.take() {
            Some(t) => {
                t.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// 异步提交一次聊天调用(登记取消令牌,后台线程调 LLM,完成后发事件)。
    pub fn submit(self: &Arc<Self>, ctx: &Context, text: &str) {
        ctx.emit(SessionStatus {
            text: format!("[chat] 已提交,调用 {} ...", self.model()),
        });
        let token = Arc::new(AtomicBool::new(false));
        *self.current.lock().unwrap() = Some(Arc::clone(&token));
        let svc = Arc::clone(self);
        let ctx = ctx.clone();
        let text = text.to_string();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            // 提交瞬间已被取消(Esc 抢在调用前)→ 直接结束
            let resp = if token.load(Ordering::SeqCst) {
                None
            } else {
                Some(svc.ask(&text))
            };
            let cancelled = token.load(Ordering::SeqCst);
            // 收尾:current 仍指向本次令牌才清空(可能已有新调用登记)
            {
                let mut cur = svc.current.lock().unwrap();
                if cur.as_ref().is_some_and(|t| Arc::ptr_eq(t, &token)) {
                    *cur = None;
                }
            }
            if cancelled {
                ctx.emit(SessionStatus { text: "[chat] 已取消".into() });
                return;
            }
            match resp.expect("取消分支已提前返回") {
                Ok(reply) => {
                    ctx.emit(SessionStatus {
                        text: format!("[chat] 回复完成 ({:?})", started.elapsed()),
                    });
                    ctx.emit(SessionReply { text: reply, user_text: text });
                }
                Err(e) => ctx.emit(SessionStatus {
                    text: format!("[chat] 错误: {e:#}"),
                }),
            }
        });
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
            current: Mutex::new(None),
        });
        ctx.provide(Arc::clone(&svc));

        // 订阅用户输入:后台线程调 LLM(Esc 可中断),完成后发 session/reply
        let svc2 = Arc::clone(&svc);
        let ctx2 = ctx.clone();
        ctx.on(move |ev: &SessionInput| {
            svc2.submit(&ctx2, &ev.text);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmClient, LlmConfig};

    fn test_svc() -> ChatService {
        ChatService {
            client: LlmClient::new(LlmConfig {
                base_url: "http://127.0.0.1:9".into(),
                api_key: "test".into(),
                model: "m".into(),
                timeout_secs: 1,
                max_concurrent: 1,
            }),
            history: Mutex::new(vec![]),
            enable_thinking: false,
            history_turns: 2,
            current: Mutex::new(None),
        }
    }

    #[test]
    fn cancel_current_only_when_busy() {
        let svc = test_svc();
        assert!(!svc.cancel_current(), "空闲时不应有调用可中断");
        assert!(!svc.is_busy());
        // 登记一个进行中的调用令牌
        let token = Arc::new(AtomicBool::new(false));
        *svc.current.lock().unwrap() = Some(Arc::clone(&token));
        assert!(svc.is_busy(), "有调用时应标记 busy");
        assert!(svc.cancel_current(), "有调用时应能中断");
        assert!(!svc.is_busy(), "中断后令牌被取出,应回到空闲");
        assert!(token.load(Ordering::SeqCst), "取消标志应置位(粘性)");
        // 中断后再次取消:无事可做
        assert!(!svc.cancel_current());
    }
}
