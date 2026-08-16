//! chat 插件:聊天服务。演示 cordis 插件的完整形态:
//! 提供服务(chat)+ 订阅事件(session/input)+ 注册命令(/chat)+ 卸载自动回滚。
//!
//! 可用上下文管理由 `memory` 插件提供(设计论证见本地文档 `context-compression.md`):
//! - memory 插件已加载 → chat 注入 [`MemoryService`]:评分/审计/critical/memory/持久化;
//! - memory 插件未加载/卸载 → chat 回退**纯滑动窗口**(本文件内的降级路径)。

use crate::cordis::context::Context;
use crate::cordis::plugin::Plugin;
use crate::cordis::service::Service;
use crate::events::{SessionInput, SessionReply, SessionStatus};
use crate::llm::{ChatMessage, ChatRequest, LlmClient, LlmService};
use crate::plugins::memory::MemoryService;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 固定系统提示词(稳定 → 吃 oMLX prompt 缓存)。
/// 启动时会把本机系统信息(见 [`crate::system::SystemInfo`])追加为"系统环境"段,
/// 让模型感知 OS/shell/工作目录;Windows 上附带 shell 策略(cmd 优先,PS 兜底)。
///
/// 工具回路:模型需要查看/操作文件或执行命令时,输出一行以 `/` 开头的工具命令,
/// chat 服务会执行并把结果回灌,再让模型基于结果作答。
const CHAT_SYSTEM: &str = "你是 localai,一个运行在本地的小型 AI 助手。回答保持简洁、直接、准确,使用与用户相同的语言。
你拥有本地工具能力。当用户的问题需要查看/操作文件系统或执行命令时,单独输出一行以 / 开头的工具命令(独占一行),例如:
/fs ls .        列出当前目录
/fs cat 文件名   读取文件内容
/fs stat 文件    查看文件信息
/run pwd        执行命令(Windows 自动走 cmd /C)
工具会立即执行并把结果回传给你,你基于结果回答,不要让用户自己动手。
工具命令只放一行:不要包在代码块里,不要加结尾标点,命令行之后不要继续写解释。不需要工具时用普通文字回答。";

/// 工具执行器:入参为"去掉 / 的命令行"(如 `fs ls .`),返回命令输出。
/// chat 插件 apply 时用 cordis 命令通道实现(None=无工具能力)。
type ToolRunner = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
/// 工具步骤状态回调(发 SessionStatus 给 TUI)。
type ToolStatusFn = Arc<dyn Fn(&str) + Send + Sync>;

/// 模型可触发的工具命令白名单(/quit /unload 等绝不暴露给模型)
const TOOL_WHITELIST: [&str; 4] = ["fs", "run", "pwd", "mode"];
/// 单次用户消息最多连续工具轮数(防模型死循环烧 token)
const MAX_TOOL_ROUNDS: usize = 3;

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
    /// base prompt(固定 + 启动时收集的系统环境)
    base: String,
    /// 降级路径(memory 插件未加载时):纯滑动窗口历史
    fallback: Mutex<Vec<ChatMessage>>,
    enable_thinking: bool,
    history_turns: usize,
    /// 当前进行中的调用令牌(None=空闲)。Esc 取消=置 true,回复丢弃;
    /// 完成后若令牌仍指向自己则清空(允许新调用登记)。
    current: Mutex<Option<Arc<AtomicBool>>>,
    /// 工具执行器(模型输出 /命令 时调用)
    tools: Option<ToolRunner>,
    /// 工具步骤状态回调
    on_tool: Option<ToolStatusFn>,
    /// 可用上下文管理(memory 插件提供;None = 纯滑动窗口降级)
    memory: Option<Arc<MemoryService>>,
}

impl Service for ChatService {
    fn service_name_static() -> &'static str {
        "chat"
    }
}

impl ChatService {
    /// 一轮问答:记录用户输入(评分/审计由 memory 服务负责)→ 主调用(工具回路)→ 落盘。
    pub fn ask(&self, user_text: &str) -> anyhow::Result<String> {
        if let Some(mem) = &self.memory {
            mem.record_user(user_text);
            mem.audit_if_needed();
        } else {
            let mut h = self.fallback.lock().unwrap();
            h.push(ChatMessage { role: "user".into(), content: user_text.to_string() });
            trim_fallback(&mut h, self.history_turns);
        }
        let mut reply = self.call_llm()?;
        let mut rounds = 0;
        while let Some(cmdline) = first_tool_cmd(&reply) {
            rounds += 1;
            if rounds > MAX_TOOL_ROUNDS {
                break;
            }
            if let Some(f) = &self.on_tool {
                f(&format!("[chat] 执行工具: /{cmdline}"));
            }
            let output = match &self.tools {
                Some(runner) => runner(&cmdline).unwrap_or_else(|| "(命令未注册)".into()),
                None => "(工具不可用)".into(),
            };
            let result = format!("[工具结果 /{cmdline}]\n{output}");
            if let Some(mem) = &self.memory {
                mem.record_tool_result(&result);
            } else {
                self.fallback
                    .lock()
                    .unwrap()
                    .push(ChatMessage { role: "user".into(), content: result });
            }
            reply = self.call_llm()?;
        }
        let final_reply = sanitize_tool_lines(&reply);
        if let Some(mem) = &self.memory {
            mem.persist();
        }
        Ok(final_reply)
    }

    /// 用当前上下文调用一次模型;助手回复入历史。
    fn call_llm(&self) -> anyhow::Result<String> {
        let messages = match &self.memory {
            Some(mem) => mem.build_context(&self.base),
            None => self.fallback.lock().unwrap().clone(),
        };
        let req = ChatRequest {
            model: self.client.model(),
            messages,
            max_tokens: 900,
            json: false,
            enable_thinking: self.enable_thinking,
        };
        let resp = self.client.chat(&req)?;
        if let Some(mem) = &self.memory {
            mem.record_assistant(&resp.content);
        } else {
            self.fallback
                .lock()
                .unwrap()
                .push(ChatMessage { role: "assistant".into(), content: resp.content.clone() });
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

/// 降级路径的有界裁剪(纯滑动窗口)。
fn trim_fallback(h: &mut Vec<ChatMessage>, turns: usize) {
    let keep = turns.saturating_mul(2);
    if h.len() > keep {
        let drop_n = h.len() - keep;
        h.drain(0..drop_n);
    }
}

/// 取回复中的第一条工具命令(以 `/` 开头且在白名单内,去掉斜杠)。
fn first_tool_cmd(text: &str) -> Option<String> {
    text.lines().map(str::trim).find_map(tool_cmd_of)
}

/// 某行是否为白名单工具命令(必须以 `/` 开头,避免误伤正文里的斜杠词)。
fn tool_cmd_of(line: &str) -> Option<String> {
    let rest = line.strip_prefix('/')?;
    let name = rest.split_whitespace().next().unwrap_or("");
    if TOOL_WHITELIST.contains(&name) {
        Some(rest.trim().to_string())
    } else {
        None
    }
}

/// 从最终回复里去掉残留的工具命令行(模型偶尔会把命令留在正文)。
fn sanitize_tool_lines(text: &str) -> String {
    text.lines()
        .filter(|l| tool_cmd_of(l.trim()).is_none())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
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
        let base = format!(
            "{CHAT_SYSTEM}\n\n# 系统环境(工具启动时收集)\n{}",
            crate::system::SystemInfo::collect().to_prompt()
        );
        // memory 插件可选注入:未加载 → 回退滑动窗口
        let memory = ctx.inject::<MemoryService>();
        if memory.is_none() {
            eprintln!("[chat] 未检测到 memory 插件,可用上下文回退为滑动窗口(加载 memory 插件可启用压缩记忆)");
        }
        let svc = Arc::new(ChatService {
            client,
            base,
            fallback: Mutex::new(Vec::new()),
            enable_thinking: opts.enable_thinking,
            history_turns: opts.history_turns,
            current: Mutex::new(None),
            // 工具回路:模型输出的 /命令 走 cordis 命令通道执行(白名单内的 fs/run/pwd/mode)
            tools: Some({
                let ctx = ctx.clone();
                Arc::new(move |line: &str| ctx.run_command(line))
            }),
            on_tool: Some({
                let ctx = ctx.clone();
                Arc::new(move |msg: &str| {
                    ctx.emit(SessionStatus { text: msg.to_string() });
                })
            }),
            memory,
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
    use crate::plugins::memory::{MemoryOptions, MemoryService, PersistOptions};
    use std::collections::VecDeque;
    use std::io::{Read, Write};

    fn test_svc() -> ChatService {
        ChatService {
            client: LlmClient::new(LlmConfig {
                base_url: "http://127.0.0.1:9".into(),
                api_key: "test".into(),
                model: "m".into(),
                timeout_secs: 1,
                max_concurrent: 1,
            }),
            base: "sys".into(),
            fallback: Mutex::new(Vec::new()),
            enable_thinking: false,
            history_turns: 4,
            current: Mutex::new(None),
            tools: None,
            on_tool: None,
            memory: None,
        }
    }

    /// 按请求内容分发的 mock LLM(主链路测试用;无 memory 时不触发评分/审计)。
    fn mock_llm(main: Vec<&'static str>, expected_requests: usize) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let main_q: Arc<Mutex<VecDeque<String>>> =
            Arc::new(Mutex::new(main.iter().map(|s| s.to_string()).collect()));
        let handle = std::thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut sock, _) = listener.accept().unwrap();
                read_full_request(&mut sock);
                let reply = main_q.lock().unwrap().pop_front().unwrap_or("(mock 主回复耗尽)".into());
                let body = serde_json::json!({
                    "choices": [{"message": {"content": reply}, "finish_reason": "stop"}],
                    "usage": {}
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn read_full_request(sock: &mut std::net::TcpStream) {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = |b: &[u8]| b.windows(4).any(|w| w == b"\r\n\r\n");
        while !header_end(&buf) {
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let header_bytes = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
            .unwrap_or(buf.len());
        let head = String::from_utf8_lossy(&buf[..header_bytes.min(buf.len())]);
        let content_len: usize = head
            .lines()
            .find_map(|l| {
                let l = l.trim().to_ascii_lowercase();
                l.strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse().ok())
            })
            .unwrap_or(0);
        while buf.len() < header_bytes + content_len {
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
    }

    #[test]
    fn cancel_current_only_when_busy() {
        let svc = test_svc();
        assert!(!svc.cancel_current(), "空闲时不应有调用可中断");
        assert!(!svc.is_busy());
        let token = Arc::new(AtomicBool::new(false));
        *svc.current.lock().unwrap() = Some(Arc::clone(&token));
        assert!(svc.is_busy(), "有调用时应标记 busy");
        assert!(svc.cancel_current(), "有调用时应能中断");
        assert!(!svc.is_busy(), "中断后令牌被取出,应回到空闲");
        assert!(token.load(Ordering::SeqCst), "取消标志应置位(粘性)");
        assert!(!svc.cancel_current());
    }

    #[test]
    fn tool_cmd_detection_and_sanitize() {
        assert_eq!(first_tool_cmd("我先看看:\n/fs ls .").as_deref(), Some("fs ls ."));
        assert_eq!(first_tool_cmd("无命令").as_deref(), None);
        assert_eq!(first_tool_cmd("/quit 退出").as_deref(), None, "白名单外的命令不触发");
        assert_eq!(first_tool_cmd("正文里的 /fs 不是命令").as_deref(), None);
        assert_eq!(first_tool_cmd("/run pwd\n/fs ls").as_deref(), Some("run pwd"), "取第一条");
        assert_eq!(sanitize_tool_lines("/fs ls .\n这是答案\n"), "这是答案");
        assert_eq!(sanitize_tool_lines("答案\n/quit\n"), "答案\n/quit", "白名单外不清理");
    }

    #[test]
    fn ask_runs_tool_loop_with_results() {
        let (base, handle) = mock_llm(vec!["我先查目录:\n/fs ls .", "当前目录有 1 个文件: hello.txt"], 2);
        let client = LlmClient::new(LlmConfig {
            base_url: base,
            api_key: "t".into(),
            model: "m".into(),
            timeout_secs: 5,
            max_concurrent: 2,
        });
        let executed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ex2 = Arc::clone(&executed);
        let tools: ToolRunner = Arc::new(move |line: &str| {
            ex2.lock().unwrap().push(line.to_string());
            Some("hello.txt".into())
        });
        let mut svc = test_svc();
        svc.client = client;
        svc.tools = Some(tools);
        let out = svc.ask("当前目录有什么文件").unwrap();
        assert_eq!(out, "当前目录有 1 个文件: hello.txt");
        assert_eq!(*executed.lock().unwrap(), vec!["fs ls ."], "工具应恰好执行一次");
        handle.join().unwrap();
    }

    #[test]
    fn ask_returns_without_tools_when_none() {
        let (base, handle) = mock_llm(vec!["直接回答,不用工具"], 1);
        let client = LlmClient::new(LlmConfig {
            base_url: base,
            api_key: "t".into(),
            model: "m".into(),
            timeout_secs: 5,
            max_concurrent: 1,
        });
        let mut svc = test_svc();
        svc.client = client;
        let out = svc.ask("你好").unwrap();
        assert_eq!(out, "直接回答,不用工具");
        handle.join().unwrap();
    }

    #[test]
    fn ask_with_memory_service_records_and_persists() {
        // chat 注入 memory 服务:主调用 1 次(无 compressor → 无评分/审计调用)
        let (base, handle) = mock_llm(vec!["回复"], 1);
        let client = LlmClient::new(LlmConfig {
            base_url: base.clone(),
            api_key: "t".into(),
            model: "m".into(),
            timeout_secs: 5,
            max_concurrent: 1,
        });
        let dir = tempfile::TempDir::new().unwrap();
        let memory = Arc::new(MemoryService::new(
            MemoryOptions {
                enabled: true,
                history_turns: 10,
                trigger_turns: 100,
                persist: PersistOptions {
                    dir: Some(dir.path().to_string_lossy().to_string()),
                },
                ..Default::default()
            },
            LlmClient::new(LlmConfig {
                base_url: base,
                api_key: "t".into(),
                model: "m".into(),
                timeout_secs: 5,
                max_concurrent: 1,
            }),
        ));
        let mut svc = test_svc();
        svc.client = client;
        svc.memory = Some(Arc::clone(&memory));
        let out = svc.ask("你好").unwrap();
        assert_eq!(out, "回复");
        // 历史经由 memory 服务管理:user + assistant 两条
        assert_eq!(memory.history().lock().unwrap().len(), 2);
        // 已落盘:context.json 快照 + conversation.jsonl 档案
        assert!(dir.path().join("context.json").exists());
        let archive = std::fs::read_to_string(dir.path().join("conversation.jsonl")).unwrap();
        assert!(archive.contains("你好"), "档案应包含本轮条目: {archive}");
        handle.join().unwrap();
    }
}
