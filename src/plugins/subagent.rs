//! subagent 插件 —— 自主委派执行器(设计论证见本地文档 `subagent-plugin.md`)。
//!
//! **纯插件实现,不进 core**:
//! - 模型档案(档案/路由表/轻提示)常驻本插件,`/models` 按需输出(渐进式载入);
//! - `/subagent` 是工具回路的执行端:主 agent 在回复中输出
//!   `/subagent <任务> [--model M] [--parallel "A" | "B"] [--tools]`,
//!   chat 工具回路执行本命令,本插件在**隔离上下文**中跑子任务
//!   (新鲜上下文,不吃 memory/历史),结果作为工具结果回灌主对话;
//! - 子任务:自检循环(max_rounds)+ 可选工具(--tools 时内部迷你工具回路,
//!   命令经 cordis 通道执行,fs fence 天然生效)+ 取消令牌(预留;
//!   当前 Esc 取消作用于 chat 整个 ask 层面,子任务调用随之一并丢弃)。

use crate::cordis::context::Context;
use crate::cordis::plugin::Plugin;
use crate::cordis::service::Service;
use crate::events::SessionStatus;
use crate::llm::{ChatMessage, ChatRequest, LlmClient, LlmService};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub fn factory() -> Box<dyn Plugin> {
    Box::new(SubagentPlugin)
}

pub struct SubagentPlugin;

// ---------- 模型档案(本插件内维护;内容来自 2026-08-16 实测基准) ----------

pub const REGISTRY_VERSION: &str = "2026-08-16";

pub struct ModelProfile {
    pub id: &'static str,
    pub capability: &'static str,
    pub strengths: &'static str,
    pub weaknesses: &'static str,
    pub speed: &'static str,
    pub context: &'static str,
    pub usage: &'static str,
}

/// 本机 oMLX 服务器已知模型档案(192.168.0.5:9870)。
pub fn builtin_profiles() -> Vec<ModelProfile> {
    vec![
        ModelProfile {
            id: "Qwen3.6-35b",
            capability: "文本",
            strengths: "中文对话/JSON 微调用/审计(memory 密度高、丢弃精准)/推理代码全对/指令遵循",
            weaknesses: "工具纪律:会编造不存在的命令(/fs pwd、/fs find)",
            speed: "平均 1.35s;审计重任务 4.8~5.7s",
            context: "262144",
            usage: "主对话默认;快任务、通用委派",
        },
        ModelProfile {
            id: "Qwen3.8-27B-4bit",
            capability: "视觉 + 文本",
            strengths: "多模态识图/critical 提取保真/工具纪律好",
            weaknesses: "慢(生成速度低)/memory 压缩偏简",
            speed: "平均 3.47s(慢 2.5x);审计 10.6~14.1s",
            context: "262144",
            usage: "视觉任务;需严格工具纪律的探索;critical 保真的记忆审计",
        },
        ModelProfile {
            id: "Fun-ASR-Nano-2512-8bit",
            capability: "音频→文本",
            strengths: "语音识别",
            weaknesses: "非 ASR 任务",
            speed: "未测",
            context: "131072",
            usage: "语音输入转写",
        },
        ModelProfile {
            id: "bge",
            capability: "嵌入",
            strengths: "文本向量化(检索/聚类)",
            weaknesses: "非嵌入任务",
            speed: "未测",
            context: "8194",
            usage: "语义检索、去重",
        },
        ModelProfile {
            id: "mlx-community--snac_24khz",
            capability: "音频",
            strengths: "音频 token 化",
            weaknesses: "非音频任务",
            speed: "未测",
            context: "252144",
            usage: "音频模型链路内部",
        },
        ModelProfile {
            id: "MarkItDown",
            capability: "文档",
            strengths: "文档→Markdown",
            weaknesses: "非文档任务",
            speed: "未测",
            context: "—",
            usage: "文档内容提取",
        },
    ]
}

/// `/models` 输出:完整路由表 + 委派准则(渐进式载入,不进常驻 base prompt)。
pub fn routing_table() -> String {
    let mut s = format!("# 模型路由表(档案 {REGISTRY_VERSION})\n");
    for p in builtin_profiles() {
        s.push_str(&format!(
            "- {}: 能力[{}] 擅长[{}] 不擅长[{}] 速度[{}] 上下文[{}] 建议[{}]\n",
            p.id, p.capability, p.strengths, p.weaknesses, p.speed, p.context, p.usage
        ));
    }
    s.push_str(
        "\n# 委派准则\n\
         - 能直接答的问题不要委派;\n\
         - 需要其他模型能力(看图/转写/文档)→ 用 /subagent --model <模型> 委派;\n\
         - 可分解为多个独立子任务 → 用 /subagent --parallel \"任务A\" | \"任务B\" 并发(≤3);\n\
         - 探索型任务(翻文件/长调研)→ 用 /subagent --tools 委派,隔离试错过程;\n\
         - 收到子任务结果先审查:与已知事实矛盾/不合常识/没回答任务 → 不整合,\n\
           指出问题,必要时带审查意见重委派一次;\n\
         - 委派有成本(每次约 1~7 秒),优先用最快的模型完成。",
    );
    s
}

/// 常驻轻提示(≈50 token;chat 的 base prompt 引用,与 subagent 插件保持同步)。
/// 含结果审查准则:子任务结果回来主 agent 必须先 review 再整合(§2.5)。
pub const LEAN_HINT: &str = "你有委派工具 /subagent 与模型查询 /models。可用模型:文本 Qwen3.6-35b(快)、视觉 Qwen3.8-27B-4bit、语音 Fun-ASR-Nano、文档 MarkItDown。详细档案与委派准则:/models 查询。收到子任务结果先审查:与已知事实矛盾/没答任务 → 不整合、指出问题。";

// ---------- 子任务内部工具回路(自包含,不进 core;命令经 cordis 通道执行) ----------

/// 子任务可触发的工具白名单(与 chat 一致;不含 subagent 自身,防递归委派)
const SUB_WHITELIST: [&str; 4] = ["fs", "run", "pwd", "mode"];
/// 子任务单次最多连续工具轮数
const MAX_SUB_TOOL_ROUNDS: usize = 3;

fn first_cmd(text: &str) -> Option<String> {
    text.lines().map(str::trim).find_map(|l| {
        let rest = l.strip_prefix('/')?;
        let name = rest.split_whitespace().next()?;
        if SUB_WHITELIST.contains(&name) {
            Some(rest.trim().to_string())
        } else {
            None
        }
    })
}

fn sanitize(text: &str) -> String {
    text.lines()
        .filter(|l| first_cmd(l.trim()).is_none())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

// ---------- 参数解析 ----------

pub struct SubagentArgs {
    /// 非空 = --parallel 模式(每个元素一个子任务)
    pub parallel: Vec<String>,
    pub model: Option<String>,
    pub tools: bool,
    /// 单任务模式的纯任务文本
    pub task: String,
}

/// 分词:`"引号段"` 作为一个 token(可含空格/`|`),其余按空白拆分。
fn tokenize(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in s.chars() {
        if c == '"' {
            if in_quote {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
                in_quote = false;
            } else {
                in_quote = true;
            }
        } else if in_quote {
            cur.push(c);
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                toks.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
}

fn parse_args(args: &str) -> SubagentArgs {
    let toks = tokenize(args);
    let mut parallel = Vec::new();
    let mut model = None;
    let mut tools = false;
    let mut saw_parallel = false;
    let mut task_parts = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        match toks[i].as_str() {
            "--parallel" => saw_parallel = true,
            "--model" => {
                i += 1;
                if i < toks.len() {
                    model = Some(toks[i].clone());
                }
            }
            "--tools" => tools = true,
            "|" => {}
            t if saw_parallel => parallel.push(t.to_string()),
            t => task_parts.push(t.to_string()),
        }
        i += 1;
    }
    SubagentArgs {
        // 只有显式 --parallel 才算并行模式;引号单任务保持单任务
        parallel: if saw_parallel { parallel } else { Vec::new() },
        model,
        tools,
        task: task_parts.join(" "),
    }
}

// ---------- 子任务模板(固定,吃 prompt 缓存) ----------

fn sub_system(use_tools: bool) -> String {
    if use_tools {
        "你是 localai 的子任务执行 agent,由主 agent 委派。你在独立上下文中执行,不知道主对话的任何历史。\n\
         要求:\n\
         - 直接输出任务的最终结果(结论/代码/数据),不要复述任务、不要寒暄、不要输出思考过程;\n\
         - 需要查看文件或执行命令时,单独输出一行以 / 开头的命令(/fs ls . /fs cat 文件 /run 命令),\
         结果会回传,你再基于结果回答;\n\
         - 仅使用:/fs ls|cat|write|edit|stat|log、/run、/pwd、/mode。"
            .into()
    } else {
        "你是 localai 的子任务执行 agent,由主 agent 委派。你在独立上下文中执行,不知道主对话的任何历史。\n\
         要求:\n\
         - 直接输出任务的最终结果(结论/代码/数据),不要复述任务、不要寒暄、不要输出思考过程;\n\
         - 你没有工具,基于你的知识直接回答。"
            .into()
    }
}

fn check_system(task: &str, answer: &str) -> String {
    format!(
        "你是 localai 的子任务复核。下面是主 agent 委派的任务和你的一次回答。\n\
         检查:① 是否回答了任务;② 有无明显错误或遗漏。\n\
         若有需要补充修正的地方,直接输出修正后的最终回答;若已完整,原样返回。\n\
         只输出最终回答,不要输出分析过程。\n\n\
         任务:\n{task}\n\n\
         你的回答:\n{answer}"
    )
}

// ---------- SubagentService ----------

pub struct SubagentService {
    client: LlmClient,
    ctx: Context,
    default_model: Option<String>,
    max_rounds: usize,
    parallel_limit: usize,
    tools_default: bool,
    next_id: AtomicU64,
    /// 子任务取消令牌(预留:当前取消在 chat ask 层面,子任务随之一并丢弃)
    active: Mutex<HashMap<u64, Arc<AtomicBool>>>,
}

impl Service for SubagentService {
    fn service_name_static() -> &'static str {
        "subagent"
    }
}

impl SubagentService {
    fn new(opts: SubagentOptions, client: LlmClient, ctx: Context) -> Self {
        Self {
            client,
            ctx,
            default_model: opts.model,
            max_rounds: opts.max_rounds.max(1),
            parallel_limit: opts.parallel_limit.max(1),
            tools_default: opts.tools,
            next_id: AtomicU64::new(1),
            active: Mutex::new(HashMap::new()),
        }
    }

    /// `/models`:按需载出模型档案 + 委派准则(渐进式载入)。
    fn handle_models(&self) -> String {
        routing_table()
    }

    /// `/subagent` 命令入口(工具回路执行端)。
    fn handle_subagent(self: &Arc<Self>, args: &str) -> String {
        let parsed = parse_args(args);
        let model = parsed
            .model
            .unwrap_or_else(|| self.default_model.clone().unwrap_or_else(|| self.client.model()));
        let use_tools = parsed.tools || self.tools_default;
        if parsed.parallel.is_empty() {
            let task = parsed.task;
            if task.is_empty() {
                return "用法: /subagent <任务> [--model <模型>] [--parallel \"A\" | \"B\"] [--tools]".into();
            }
            self.run_subtask(&task, &model, use_tools)
        } else {
            let tasks: Vec<String> = parsed
                .parallel
                .into_iter()
                .filter(|t| !t.trim().is_empty())
                .take(self.parallel_limit)
                .collect();
            if tasks.is_empty() {
                return "用法: /subagent --parallel \"任务A\" | \"任务B\"".into();
            }
            let handles: Vec<_> = tasks
                .into_iter()
                .map(|t| {
                    let svc = Arc::clone(self);
                    let model = model.clone();
                    std::thread::spawn(move || svc.run_subtask(&t, &model, use_tools))
                })
                .collect();
            let results: Vec<String> = handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| "(子任务线程异常)".into()))
                .collect();
            results
                .iter()
                .enumerate()
                .map(|(i, r)| format!("[子任务{}]\n{r}", i + 1))
                .collect::<Vec<_>>()
                .join("\n\n")
        }
    }

    /// 在隔离上下文执行一个子任务:首轮 + 自检循环;返回最终文本。
    fn run_subtask(&self, task: &str, model: &str, use_tools: bool) -> String {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let token = Arc::new(AtomicBool::new(false));
        self.active.lock().unwrap().insert(id, Arc::clone(&token));
        let started = Instant::now();
        self.ctx.emit(SessionStatus {
            text: format!("[subagent#{id}] 开始: {task}"),
        });
        let mut answer = self.call_once(&sub_system(use_tools), task, model, use_tools, &token);
        for _ in 1..self.max_rounds {
            if token.load(Ordering::SeqCst) {
                break;
            }
            let check = check_system(task, &answer);
            answer = self.call_once(&check, "", model, false, &token);
        }
        self.active.lock().unwrap().remove(&id);
        let elapsed = started.elapsed();
        if token.load(Ordering::SeqCst) {
            self.ctx.emit(SessionStatus {
                text: format!("[subagent#{id}] 已取消"),
            });
            return "(已取消)".into();
        }
        self.ctx.emit(SessionStatus {
            text: format!("[subagent#{id}] 完成 ({:?})", elapsed),
        });
        answer
    }

    /// 一次 LLM 调用(可选内部工具回路,仅首轮 use_tools=true 时开启)。
    fn call_once(
        &self,
        system: &str,
        user: &str,
        model: &str,
        use_tools: bool,
        token: &Arc<AtomicBool>,
    ) -> String {
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: system.to_string(),
        }];
        if !user.is_empty() {
            messages.push(ChatMessage {
                role: "user".into(),
                content: user.to_string(),
            });
        }
        let mut reply = self.chat(model, &messages, token);
        if !use_tools {
            return reply;
        }
        for _ in 0..MAX_SUB_TOOL_ROUNDS {
            let Some(cmd) = first_cmd(&reply) else { break };
            messages.push(ChatMessage {
                role: "assistant".into(),
                content: reply.clone(),
            });
            let out = self
                .ctx
                .run_command(&cmd)
                .unwrap_or_else(|| "(命令未注册)".into());
            messages.push(ChatMessage {
                role: "user".into(),
                content: format!("[工具结果 /{cmd}]\n{out}"),
            });
            reply = self.chat(model, &messages, token);
        }
        sanitize(&reply)
    }

    /// 裸调用(enable_thinking 关,JSON 不需要;失败返回错误文案,不 panic)。
    fn chat(&self, model: &str, messages: &[ChatMessage], token: &Arc<AtomicBool>) -> String {
        if token.load(Ordering::SeqCst) {
            return String::new();
        }
        let req = ChatRequest {
            model: model.to_string(),
            messages: messages.to_vec(),
            max_tokens: 900,
            json: false,
            enable_thinking: false,
        };
        match self.client.chat(&req) {
            Ok(r) => r.content,
            Err(e) => format!("(子任务调用失败: {e:#})"),
        }
    }
}

// ---------- 插件 ----------

#[derive(serde::Deserialize)]
#[serde(default)]
pub struct SubagentOptions {
    /// 子任务默认模型(None = 跟随主模型)
    pub model: Option<String>,
    /// 自检循环:1 = 单次直出;2 = 自检一轮(默认)
    pub max_rounds: usize,
    /// --parallel 上限(默认 3;本地 AI 并发不是强项,克制)
    pub parallel_limit: usize,
    /// 子任务默认是否带工具(默认 false;探索型任务用 --tools 显式开启)
    pub tools: bool,
}

impl Default for SubagentOptions {
    fn default() -> Self {
        Self {
            model: None,
            max_rounds: 2,
            parallel_limit: 3,
            tools: false,
        }
    }
}

impl Plugin for SubagentPlugin {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn inject(&self) -> &'static [&'static str] {
        &["llm"]
    }

    fn apply(&self, ctx: &Context) -> anyhow::Result<()> {
        let opts: SubagentOptions = ctx.options()?;
        let llm = ctx.inject::<LlmService>().ok_or_else(|| anyhow::anyhow!("缺少 llm 服务"))?;
        let svc = Arc::new(SubagentService::new(opts, llm.client.clone(), ctx.clone()));
        ctx.provide(Arc::clone(&svc));

        // 工具回路执行端:主 agent 输出 /subagent ... 时,chat 的工具回路调用本命令
        let svc2 = Arc::clone(&svc);
        ctx.on_command("subagent", move |args: &str| svc2.handle_subagent(args));
        // 渐进式载入:模型档案 + 委派准则
        let svc3 = Arc::clone(&svc);
        ctx.on_command("models", move |_: &str| svc3.handle_models());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cordis::loader::{Loader, LoaderService};
    use crate::llm::{LlmClient, LlmConfig};
    use std::io::{Read, Write};

    /// 极简 mock OpenAI 兼容服务器:依次返回给定 content。
    fn mock_llm(contents: &[&str]) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let contents: Vec<String> = contents.iter().map(|s| s.to_string()).collect();
        let handle = std::thread::spawn(move || {
            for content in &contents {
                let (mut sock, _) = listener.accept().unwrap();
                read_full_request(&mut sock);
                let body = serde_json::json!({
                    "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
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
                l.strip_prefix("content-length:").and_then(|v| v.trim().parse().ok())
            })
            .unwrap_or(0);
        while buf.len() < header_bytes + content_len {
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
    }

    fn test_loader(base: String, max_rounds: usize) -> Arc<Mutex<Loader>> {
        let client = LlmClient::new(LlmConfig {
            base_url: base,
            api_key: "t".into(),
            model: "Qwen3.6-35b".into(),
            timeout_secs: 5,
            max_concurrent: 2,
        });
        let loader = Arc::new(Mutex::new(Loader::new(client, vec![factory])));
        {
            let l = loader.lock().unwrap();
            l.root().provide(Arc::new(LoaderService { loader: Arc::clone(&loader) }));
        }
        let opts: serde_yaml::Value = serde_yaml::from_str(&format!(
            "max_rounds: {max_rounds}\nparallel_limit: 3"
        ))
        .unwrap();
        loader.lock().unwrap().load("subagent", opts).unwrap();
        loader
    }

    #[test]
    fn routing_table_contains_profiles() {
        let t = routing_table();
        assert!(t.contains("Qwen3.6-35b") && t.contains("Qwen3.8-27B-4bit"));
        assert!(t.contains("委派准则"));
        assert!(t.contains(REGISTRY_VERSION));
    }

    #[test]
    fn parse_args_variants() {
        let p = parse_args("--parallel \"任务A\" | \"任务B\" --model Qwen3.8-27B-4bit");
        assert_eq!(p.parallel, vec!["任务A", "任务B"]);
        assert_eq!(p.model.as_deref(), Some("Qwen3.8-27B-4bit"));
        assert!(!p.tools);

        let s = parse_args("--tools --model M 帮我统计文件行数");
        assert!(s.parallel.is_empty());
        assert!(s.tools);
        assert_eq!(s.task, "帮我统计文件行数");

        let plain = parse_args("写一个冒泡排序");
        assert!(plain.parallel.is_empty());
        assert_eq!(plain.task, "写一个冒泡排序");

        // 带引号的单任务(无 --parallel)必须是单任务模式,不是并行
        let quoted_single = parse_args("\"帮我查一下文档内容\"");
        assert!(quoted_single.parallel.is_empty(), "无 --parallel 时引号任务应保持单任务");
        assert_eq!(quoted_single.task, "帮我查一下文档内容");

        // 带引号的 --model 值应被正确消费,不落入任务/并行
        let quoted_model = parse_args("--model \"Qwen3.8-27B-4bit\" 看图描述");
        assert_eq!(quoted_model.model.as_deref(), Some("Qwen3.8-27B-4bit"));
        assert!(quoted_model.parallel.is_empty());
        assert_eq!(quoted_model.task, "看图描述");

        // 并行模式里 --model 值不被当成子任务
        let p2 = parse_args("--parallel \"A\" --model M | \"B\"");
        assert_eq!(p2.parallel, vec!["A", "B"]);
        assert_eq!(p2.model.as_deref(), Some("M"));
    }

    #[test]
    fn subagent_single_task_with_selfcheck() {
        // 首轮出结果,自检一轮原样返回
        let (base, handle) = mock_llm(&["子任务结果:排序算法 O(n²)", "子任务结果:排序算法 O(n²)"]);
        let loader = test_loader(base, 2);
        let root = loader.lock().unwrap().root().clone();
        let out = root.run_command("subagent 写冒泡排序").unwrap();
        assert!(out.contains("排序算法"), "{out}");
        handle.join().unwrap();
    }

    #[test]
    fn subagent_parallel_fanout() {
        let (base, handle) = mock_llm(&["A 的结果", "B 的结果"]);
        let loader = test_loader(base, 1);
        let root = loader.lock().unwrap().root().clone();
        let out = root
            .run_command("subagent --parallel \"任务A\" | \"任务B\"")
            .unwrap();
        assert!(out.contains("[子任务1]") && out.contains("[子任务2]"), "{out}");
        assert!(out.contains("A 的结果") && out.contains("B 的结果"), "{out}");
        handle.join().unwrap();
    }

    #[test]
    fn subagent_tools_runs_commands() {
        // 子任务首轮输出工具命令 → 执行(假 fs 命令)→ 再调 → 最终结果
        let (base, handle) = mock_llm(&["/fs ls .", "目录里有 2 个文件"]);
        let loader = test_loader(base, 1);
        {
            let root = loader.lock().unwrap().root().clone();
            let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let c2 = Arc::clone(&calls);
            root.on_command("fs", move |args: &str| {
                c2.lock().unwrap().push(args.to_string());
                "a.txt b.txt".into()
            });
            let out = root.run_command("subagent --tools 统计目录").unwrap();
            assert!(out.contains("2 个文件"), "{out}");
            assert_eq!(*calls.lock().unwrap(), vec!["ls ."], "工具应被调用一次");
        }
        handle.join().unwrap();
    }

    #[test]
    fn models_command_outputs_table() {
        // /models 是本地命令,不调 LLM → mock 空列表(线程立即退出,不挂 accept)
        let (base, handle) = mock_llm(&[]);
        let loader = test_loader(base, 1);
        let root = loader.lock().unwrap().root().clone();
        let out = root.run_command("models").unwrap();
        assert!(out.contains("Qwen3.8-27B-4bit"));
        handle.join().unwrap();
    }
}
