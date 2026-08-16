//! memory 插件 —— 上下文压缩/记忆服务(设计论证见本地文档 `context-compression.md` 第 5 版)。
//!
//! 按项目插件哲学(内核只负责插拔,能力全部来自插件),把 chat 的"可用上下文管理"
//! 独立为插件:
//! - 提供 [`MemoryService`]:历史条目(score/seq/ts 跟随条目)+ critical/memory 槽位 +
//!   逐条后台评分 + 全量审计 + 驱逐 + 持久化;
//! - chat 插件可选注入:`ctx.inject::<MemoryService>()`,拿不到时回退纯滑动窗口;
//! - 注册 `/new` 命令(新建会话);卸载插件 → 命令消失,chat 降级为滑动窗口。

use crate::cordis::context::Context;
use crate::cordis::plugin::Plugin;
use crate::cordis::service::Service;
use crate::llm::{now_ts, ChatMessage, Compressor, HistoryEntry, LlmService};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub fn factory() -> Box<dyn Plugin> {
    Box::new(MemoryPlugin)
}

pub struct MemoryPlugin;

#[derive(serde::Deserialize)]
#[serde(default)]
pub struct MemoryOptions {
    /// 总开关(false = 不提供服务,chat 回退滑动窗口)
    pub enabled: bool,
    /// 次级记忆预算(字;≈500~750 token)
    pub memory_max_chars: usize,
    /// 关键记忆条数上限(S 级,不计入 memory 预算)
    pub critical_max_items: usize,
    /// critical 重审门槛:条数 ≥ 此值时审计顺带重审淘汰过时约束
    pub critical_reaudit: usize,
    /// 临时窗口:驱逐时最近 N 条豁免(防"刚说完就消失")
    pub grace_turns: usize,
    /// 距上次审计新增多少轮触发审计(触发器 B)
    pub trigger_turns: usize,
    /// 近期原样保留的轮数(滑动裁剪兜底)
    pub history_turns: usize,
    pub persist: PersistOptions,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            memory_max_chars: 500,
            critical_max_items: 8,
            critical_reaudit: 6,
            grace_turns: 3,
            trigger_turns: 6,
            history_turns: 6,
            persist: PersistOptions::default(),
        }
    }
}

#[derive(serde::Deserialize, Default)]
pub struct PersistOptions {
    /// 空/"default" = 可执行文件所在目录;"~"或"~/xxx" = 家目录;绝对路径 = 自定义
    #[serde(default)]
    pub dir: Option<String>,
}

// ---------- 持久化(conversation 档案只写 + context 快照 resume) ----------

/// last 可用上下文快照(context.json,resume 时载入)
#[derive(Serialize, Deserialize, Clone)]
pub struct ChatSnapshot {
    pub seq: u64,
    pub history: Vec<HistoryEntry>,
    pub critical: Vec<String>,
    pub memory: Option<String>,
    pub since_audit: usize,
}

pub struct Persist {
    /// 完整对话记录(只写不读;JSONL 追加,一行一条 HistoryEntry)
    conversation: PathBuf,
    /// last 可用上下文快照(每次交互/审计后重写,resume 载入)
    context: PathBuf,
}

impl Persist {
    /// 目录解析:空/"default" = 可执行文件目录;`~` = 家目录;绝对路径 = 自定义。
    pub fn resolve_dir(cfg: &Option<String>) -> PathBuf {
        match cfg.as_deref() {
            None | Some("") | Some("default") => std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
            Some(dir) if dir.starts_with('~') => {
                let home = std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_else(|_| ".".into());
                let rest = dir.trim_start_matches('~').trim_start_matches(['/', '\\']);
                if rest.is_empty() {
                    PathBuf::from(home)
                } else {
                    PathBuf::from(home).join(rest)
                }
            }
            Some(dir) => PathBuf::from(dir),
        }
    }

    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self {
            conversation: dir.join("conversation.jsonl"),
            context: dir.join("context.json"),
        }
    }

    /// 追加新条目到对话档案(只写不读)。
    pub fn append_conversation(&self, new_entries: &[HistoryEntry]) -> anyhow::Result<()> {
        if new_entries.is_empty() {
            return Ok(());
        }
        let mut content = String::new();
        for e in new_entries {
            content.push_str(&serde_json::to_string(e)?);
            content.push('\n');
        }
        append_file(&self.conversation, content.as_bytes())
    }

    /// 会话分隔标记(/new 时写入档案)
    pub fn append_session_marker(&self, ts: &str) -> anyhow::Result<()> {
        append_file(&self.conversation, format!("{{\"session\": \"{ts}\"}}\n").as_bytes())
    }

    /// 保存 last 可用上下文快照(原子写,0600)。
    pub fn save_context(&self, snap: &ChatSnapshot) -> anyhow::Result<()> {
        atomic_write_json(&self.context, snap)
    }

    /// 载入 last 可用上下文(失败 → None,调用侧全新会话)。
    pub fn load_context(&self) -> Option<ChatSnapshot> {
        let text = std::fs::read_to_string(&self.context).ok()?;
        serde_json::from_str(&text).ok()
    }

    #[cfg(test)]
    fn conversation_path(&self) -> PathBuf {
        self.conversation.clone()
    }
    #[cfg(test)]
    fn context_path(&self) -> PathBuf {
        self.context.clone()
    }
}

fn append_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "ctx".into());
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, json.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------- MemoryService ----------

/// 可用上下文管理:chat 插件可选注入;无它时 chat 回退纯滑动窗口。
pub struct MemoryService {
    history: Arc<Mutex<Vec<HistoryEntry>>>,
    critical: Arc<Mutex<Vec<String>>>,
    memory: Arc<Mutex<Option<String>>>,
    since_audit: Mutex<usize>,
    seq: AtomicU64,
    /// conversation.jsonl 档案已写水位(append 去重)
    saved: AtomicU64,
    compressor: Option<Arc<Compressor>>,
    persist: Option<Persist>,
    history_turns: usize,
    grace_turns: usize,
    critical_max_items: usize,
    critical_reaudit: usize,
    trigger_turns: usize,
}

impl Service for MemoryService {
    fn service_name_static() -> &'static str {
        "memory"
    }
}

impl MemoryService {
    pub fn new(opts: MemoryOptions, client: crate::llm::LlmClient) -> Self {
        let compressor = if opts.enabled {
            Some(Arc::new(Compressor::new(
                client.clone(),
                opts.memory_max_chars,
                opts.critical_max_items,
            )))
        } else {
            None
        };
        let persist = if opts.enabled {
            Some(Persist::new(Persist::resolve_dir(&opts.persist.dir)))
        } else {
            None
        };
        let svc = Self {
            history: Arc::new(Mutex::new(Vec::new())),
            critical: Arc::new(Mutex::new(Vec::new())),
            memory: Arc::new(Mutex::new(None)),
            since_audit: Mutex::new(0),
            seq: AtomicU64::new(1),
            saved: AtomicU64::new(0),
            compressor,
            persist,
            history_turns: opts.history_turns,
            grace_turns: opts.grace_turns,
            critical_max_items: opts.critical_max_items,
            critical_reaudit: opts.critical_reaudit,
            trigger_turns: opts.trigger_turns,
        };
        svc.resume();
        svc
    }

    /// 触发器 A(每轮):推入 user 条目并 spawn 后台评分线程(不阻塞主调用)。
    pub fn record_user(&self, text: &str) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.push(HistoryEntry::user(text.to_string(), seq, now_ts()));
        let compressor = match &self.compressor {
            Some(c) => Arc::clone(c),
            None => return,
        };
        let history = Arc::clone(&self.history);
        let critical = Arc::clone(&self.critical);
        let content = text.to_string();
        std::thread::spawn(move || {
            let probe = HistoryEntry::user(content, seq, String::new());
            if let Ok(s) = compressor.score_entry(&probe) {
                // 评分写回条目(评估跟着条目走)
                if let Some(e) = history.lock().unwrap().iter_mut().find(|e| e.seq == seq) {
                    e.score = s.score.min(5);
                }
                // S 级 → 实时入 critical(精确去重)
                if let Some(c) = s.critical {
                    let c = c.trim().to_string();
                    if !c.is_empty() {
                        let mut crit = critical.lock().unwrap();
                        if !crit.contains(&c) {
                            crit.push(c);
                        }
                    }
                }
            }
            // 评分失败 → 保持 score=0(驱逐时按 B 级缺省),审计补评
        });
    }

    /// 助手回复入历史(评分继承其 user 条目的分数,同轮同分)。
    pub fn record_assistant(&self, text: &str) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let score = {
            let h = self.history.lock().unwrap();
            h.iter()
                .rev()
                .find(|e| e.role == "user")
                .map(|e| if e.score == 0 { 3 } else { e.score })
                .unwrap_or(3)
        };
        self.push(HistoryEntry::assistant(text.to_string(), seq, now_ts()).with_score(score));
    }

    /// 工具结果入历史(recent 段,默认 B 级,不参与评分)。
    pub fn record_tool_result(&self, text: &str) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.push(HistoryEntry::tool_result(text.to_string(), seq, now_ts()));
    }

    /// 触发器 B(阈值):全量审计 → 重评分覆盖 + critical 合并/重审 + memory 重建 + 驱逐。
    pub fn audit_if_needed(&self) {
        let compressor = match &self.compressor {
            Some(c) => Arc::clone(c),
            None => return,
        };
        let trigger = {
            let mut since = self.since_audit.lock().unwrap();
            *since += 1;
            let over = self.history.lock().unwrap().len() > self.history_turns.saturating_mul(2);
            if *since >= self.trigger_turns || over {
                *since = 0;
                true
            } else {
                false
            }
        };
        if !trigger {
            return;
        }
        let entries = self.history.lock().unwrap().clone();
        let old_critical = self.critical.lock().unwrap().clone();
        let reaudit = old_critical.len() >= self.critical_reaudit;
        let outcome = match compressor.audit(&entries, if reaudit { &old_critical } else { &[] }) {
            Ok(o) => o,
            Err(_) => return, // 静默跳过,下次触发再试(不阻塞对话)
        };
        // 分数写回每个条目(覆盖后台逐条的旧分)
        {
            let mut h = self.history.lock().unwrap();
            for (seq, score) in &outcome.scores {
                if let Some(e) = h.iter_mut().find(|e| e.seq == *seq) {
                    e.score = (*score).min(5);
                }
            }
            // 驱逐:dropped 条目移除,但临时窗口(最后 grace 条)内豁免
            let kept = evict(&h, &outcome.dropped_seqs, self.grace_turns);
            *h = kept;
        }
        // critical:重审(模型全量重建)或合并去重
        {
            let mut crit = self.critical.lock().unwrap();
            *crit = merge_critical(&crit, &outcome.critical, self.critical_max_items, reaudit);
        }
        *self.memory.lock().unwrap() = Some(outcome.memory);
    }

    /// 组装上下文消息:[base, 关键记忆, 次级记忆, ...历史条目]
    pub fn build_context(&self, base: &str) -> Vec<ChatMessage> {
        let mut msgs = vec![ChatMessage { role: "system".into(), content: base.to_string() }];
        let critical = self.critical.lock().unwrap();
        if !critical.is_empty() {
            msgs.push(ChatMessage {
                role: "system".into(),
                content: format!("[关键记忆]\n{}", critical.join("\n")),
            });
        }
        let memory = self.memory.lock().unwrap();
        if let Some(m) = &*memory {
            msgs.push(ChatMessage {
                role: "system".into(),
                content: format!("[次级记忆]\n{m}"),
            });
        }
        for e in self.history.lock().unwrap().iter() {
            msgs.push(ChatMessage { role: e.role.clone(), content: e.content.clone() });
        }
        msgs
    }

    /// 落盘:conversation.jsonl 追加本轮新条目(只写档案)+ context.json 快照(resume 载入)。
    pub fn persist(&self) {
        let persist = match &self.persist {
            Some(p) => p,
            None => return,
        };
        let saved = self.saved.load(Ordering::SeqCst);
        let new_entries: Vec<HistoryEntry> = {
            let h = self.history.lock().unwrap();
            h.iter().filter(|e| e.seq > saved).cloned().collect()
        };
        let max_seq = new_entries.iter().map(|e| e.seq).max().unwrap_or(saved);
        let _ = persist.append_conversation(&new_entries);
        self.saved.store(max_seq, Ordering::SeqCst);
        let _ = persist.save_context(&self.snapshot());
    }

    /// `/new` 新建会话:清空可用上下文;档案追加会话分隔;seq 继续递增。
    pub fn reset_session(&self) {
        self.history.lock().unwrap().clear();
        self.critical.lock().unwrap().clear();
        *self.memory.lock().unwrap() = None;
        *self.since_audit.lock().unwrap() = 0;
        if let Some(p) = &self.persist {
            let _ = p.append_session_marker(&now_ts());
            let _ = p.save_context(&self.snapshot());
        }
    }

    fn push(&self, entry: HistoryEntry) {
        let mut h = self.history.lock().unwrap();
        h.push(entry);
        trim(&mut h, self.history_turns);
    }

    fn snapshot(&self) -> ChatSnapshot {
        ChatSnapshot {
            seq: self.seq.load(Ordering::SeqCst),
            history: self.history.lock().unwrap().clone(),
            critical: self.critical.lock().unwrap().clone(),
            memory: self.memory.lock().unwrap().clone(),
            since_audit: *self.since_audit.lock().unwrap(),
        }
    }

    /// 恢复上次可用上下文(resume;加载失败 = 全新会话,不阻塞)。
    fn resume(&self) {
        let persist = match &self.persist {
            Some(p) => p,
            None => return,
        };
        if let Some(snap) = persist.load_context() {
            *self.history.lock().unwrap() = snap.history;
            *self.critical.lock().unwrap() = snap.critical;
            *self.memory.lock().unwrap() = snap.memory;
            *self.since_audit.lock().unwrap() = snap.since_audit;
            self.seq.store(snap.seq.max(1), Ordering::SeqCst);
            // 档案水位:恢复的条目在上一轮运行已写过档案
            self.saved.store(snap.seq, Ordering::SeqCst);
        }
    }

    /// 历史条目(测试/观测用)
    #[cfg(test)]
    pub fn history(&self) -> &Arc<Mutex<Vec<HistoryEntry>>> {
        &self.history
    }
    #[cfg(test)]
    pub fn critical_items(&self) -> Vec<String> {
        self.critical.lock().unwrap().clone()
    }
    #[cfg(test)]
    pub fn memory_text(&self) -> Option<String> {
        self.memory.lock().unwrap().clone()
    }
}

// ---------- 历史工具函数 ----------

/// 有界裁剪(滑动窗口降级为 recent 段上限;驱逐由审计/驱逐算法负责)。
fn trim(h: &mut Vec<HistoryEntry>, turns: usize) {
    let keep = turns.saturating_mul(2);
    if h.len() > keep {
        let drop_n = h.len() - keep;
        h.drain(0..drop_n);
    }
}

/// 驱逐:dropped 中的条目移除,但临时窗口(最后 grace 条)内豁免。
/// 驱逐优先级由条目上的 score/seq 决定(模型已给出 dropped 清单)。
fn evict(entries: &[HistoryEntry], dropped: &[u64], grace: usize) -> Vec<HistoryEntry> {
    let grace_start = entries.len().saturating_sub(grace);
    let mut idx = 0usize;
    entries
        .iter()
        .filter(|e| {
            let keep = !dropped.contains(&e.seq) || idx >= grace_start;
            idx += 1;
            keep
        })
        .cloned()
        .collect()
}

/// critical 合并:重审(模型全量重建)或取并集去重,截断到上限。
fn merge_critical(old: &[String], new: &[String], max: usize, reaudit: bool) -> Vec<String> {
    let mut out: Vec<String> = if reaudit { new.to_vec() } else { old.to_vec() };
    if !reaudit {
        for c in new {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
    }
    out.truncate(max);
    out
}

impl Plugin for MemoryPlugin {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn inject(&self) -> &'static [&'static str] {
        &["llm"]
    }

    fn apply(&self, ctx: &Context) -> anyhow::Result<()> {
        let opts: MemoryOptions = ctx.options()?;
        if !opts.enabled {
            // 禁用 = 不提供服务,chat 回退滑动窗口
            return Ok(());
        }
        let llm = ctx.inject::<LlmService>().ok_or_else(|| anyhow::anyhow!("缺少 llm 服务"))?;
        let svc = Arc::new(MemoryService::new(opts, llm.client.clone()));
        ctx.provide(Arc::clone(&svc));

        // `/new` 新建会话:清空可用上下文(critical/memory/recent),档案追加分隔
        let svc2 = Arc::clone(&svc);
        ctx.on_command("new", move |_: &str| {
            svc2.reset_session();
            "已新建会话(可用上下文已清空)".to_string()
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmClient, LlmConfig};
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    fn test_memory() -> MemoryService {
        MemoryService::new(
            MemoryOptions {
                enabled: true,
                history_turns: 10,
                trigger_turns: 100,
                ..Default::default()
            },
            LlmClient::new(LlmConfig {
                base_url: "http://127.0.0.1:9".into(),
                api_key: "t".into(),
                model: "m".into(),
                timeout_secs: 1,
                max_concurrent: 1,
            }),
        )
    }

    /// 按请求内容分发的 mock LLM:
    /// - 含"对话评分器" → score 回复;含"上下文压缩器" → audit 回复;
    /// - 否则 → 从 main 队列取一条主回复。
    fn mock_llm_dispatch(
        score: Option<&'static str>,
        audit: Option<&'static str>,
        main: Vec<&'static str>,
        expected_requests: usize,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let main_q: Arc<Mutex<VecDeque<String>>> =
            Arc::new(Mutex::new(main.iter().map(|s| s.to_string()).collect()));
        let handle = std::thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut sock, _) = listener.accept().unwrap();
                let body = read_request_body(&mut sock);
                let reply = if body.contains("对话评分器") {
                    score.expect("未提供 score 回复").to_string()
                } else if body.contains("上下文压缩器") {
                    audit.expect("未提供 audit 回复").to_string()
                } else {
                    main_q.lock().unwrap().pop_front().unwrap_or("(mock 主回复耗尽)".into())
                };
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

    /// 读完整请求并返回 body(用于按内容分发)。
    fn read_request_body(sock: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = |b: &[u8]| b.windows(4).any(|w| w == b"\r\n\r\n");
        while !header_end(&buf) {
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => return String::from_utf8_lossy(&buf).to_string(),
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
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    #[test]
    fn background_scoring_writes_entry_score_and_critical() {
        let (base, handle) = mock_llm_dispatch(
            Some(r#"{"score": 5, "critical": "约束:始终用中文"}"#),
            None,
            vec![],
            1,
        );
        let client = LlmClient::new(LlmConfig {
            base_url: base,
            api_key: "t".into(),
            model: "m".into(),
            timeout_secs: 5,
            max_concurrent: 2,
        });
        let mut svc = test_memory();
        svc.compressor = Some(Arc::new(Compressor::new(client, 500, 8)));
        svc.record_user("以后都用中文回答");
        // 轮询等待后台评分写入(不阻塞主调用)
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let scored = {
                let h = svc.history().lock().unwrap();
                h.iter().find(|e| e.role == "user").map(|e| e.score).unwrap_or(0)
            };
            if scored == 5 {
                break;
            }
            assert!(Instant::now() < deadline, "后台评分未在超时内写入(score={scored})");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            svc.critical_items().iter().any(|c| c.contains("用中文")),
            "S 级关键记忆应入库"
        );
        handle.join().unwrap();
    }

    #[test]
    fn audit_applies_scores_memory_and_evicts() {
        let (base, handle) = mock_llm_dispatch(
            Some(r#"{"score": 1}"#),
            Some(
                r#"{"scores":[{"seq":1,"score":1},{"seq":2,"score":4}],
                   "critical":["约束X"],
                   "memory":"早期事实摘要",
                   "dropped_seqs":[1]}"#,
            ),
            vec![],
            2, // 1 后台评分 + 1 审计(无主调用)
        );
        let client = LlmClient::new(LlmConfig {
            base_url: base,
            api_key: "t".into(),
            model: "m".into(),
            timeout_secs: 5,
            max_concurrent: 2,
        });
        let mut svc = test_memory();
        svc.compressor = Some(Arc::new(Compressor::new(client, 500, 8)));
        svc.trigger_turns = 1; // 每次都审计
        svc.grace_turns = 0; // 关闭临时窗口,验证驱逐
        svc.record_user("你好");
        svc.record_assistant("回复");
        svc.audit_if_needed();
        let h = svc.history().lock().unwrap();
        assert!(!h.is_empty());
        assert!(!h.iter().any(|e| e.seq == 1), "seq=1 应被驱逐");
        assert_eq!(svc.memory_text().as_deref(), Some("早期事实摘要"));
        assert!(svc.critical_items().iter().any(|c| c.contains("约束X")));
        drop(h);
        handle.join().unwrap();
    }

    #[test]
    fn evict_honors_grace_window_and_dropped() {
        let mk = |seq: u64| HistoryEntry::user(format!("m{seq}"), seq, "t".into());
        let entries = vec![mk(1), mk(2), mk(3), mk(4)];
        let kept = evict(&entries, &[1, 3], 2);
        let seqs: Vec<u64> = kept.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4], "seq1 被驱逐,seq3 受临时窗口保护");
        let kept0 = evict(&entries, &[1, 3], 0);
        let seqs0: Vec<u64> = kept0.iter().map(|e| e.seq).collect();
        assert_eq!(seqs0, vec![2, 4]);
    }

    #[test]
    fn merge_critical_dedups_and_reaudits() {
        let old = vec!["约束A".to_string(), "约束B".to_string()];
        let new = vec!["约束B".to_string(), "约束C".to_string()];
        let merged = merge_critical(&old, &new, 8, false);
        assert_eq!(merged, vec!["约束A", "约束B", "约束C"]);
        let re = merge_critical(&old, &new, 8, true);
        assert_eq!(re, vec!["约束B", "约束C"]);
        let cap = merge_critical(&old, &new, 2, false);
        assert_eq!(cap.len(), 2);
    }

    #[test]
    fn persist_roundtrip_and_resume() {
        let dir = tempfile::TempDir::new().unwrap();
        let persist = Persist::new(dir.path().to_path_buf());
        let entry = HistoryEntry::user("你好".into(), 1, "12:00:00".into());
        persist.append_conversation(std::slice::from_ref(&entry)).unwrap();
        persist.append_session_marker("12:00:01").unwrap();
        let snap = ChatSnapshot {
            seq: 5,
            history: vec![entry.clone()],
            critical: vec!["约束X".into()],
            memory: Some("记忆".into()),
            since_audit: 2,
        };
        persist.save_context(&snap).unwrap();
        let archive = std::fs::read_to_string(persist.conversation_path()).unwrap();
        assert_eq!(archive.lines().count(), 2, "档案应为 JSONL 追加");
        assert!(archive.contains("你好") && archive.contains("session"));
        let loaded = persist.load_context().unwrap();
        assert_eq!(loaded.seq, 5);
        assert_eq!(loaded.history.len(), 1);
        assert_eq!(loaded.critical, vec!["约束X"]);
        assert_eq!(loaded.memory.as_deref(), Some("记忆"));
        std::fs::write(persist.context_path(), "{broken").unwrap();
        assert!(persist.load_context().is_none());
    }

    #[test]
    fn resolve_dir_defaults_to_exe_home_and_absolute() {
        let d = Persist::resolve_dir(&None);
        assert!(d.is_absolute(), "{d:?}");
        assert_eq!(Persist::resolve_dir(&Some("default".into())), d);
        let home = Persist::resolve_dir(&Some("~".into()));
        assert!(home.is_absolute(), "{home:?}");
        let abs = Persist::resolve_dir(&Some("/tmp/localai-ctx".into()));
        assert_eq!(abs, PathBuf::from("/tmp/localai-ctx"));
    }

    #[test]
    fn reset_session_clears_context_and_keeps_seq() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut svc = test_memory();
        svc.persist = Some(Persist::new(dir.path().to_path_buf()));
        svc.record_user("第一轮");
        svc.record_assistant("回复");
        svc.audit_if_needed(); // 无 compressor,跳过
        svc.persist();
        let seq_before = svc.seq.load(Ordering::SeqCst);
        assert!(svc.seq.load(Ordering::SeqCst) > 1);
        svc.reset_session();
        assert!(svc.history().lock().unwrap().is_empty());
        assert!(svc.critical_items().is_empty());
        assert!(svc.seq.load(Ordering::SeqCst) >= seq_before, "seq 继续递增");
        let archive = std::fs::read_to_string(dir.path().join("conversation.jsonl")).unwrap();
        assert!(archive.contains("session"), "reset 应写入会话分隔");
    }
}
