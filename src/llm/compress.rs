//! 上下文压缩器(设计论证见本地文档 `context-compression.md` 第 5 版)。
//!
//! - [`HistoryEntry`]:历史条目 = 消息 + 评分元数据(评分跟随条目,驱逐/审计/持久化都读它);
//! - [`Compressor::score_entry`]:单条评分(触发器 A,后台线程调用,零成本);
//! - [`Compressor::audit`]:全量审计(触发器 B,阈值触发):重评分 + critical 合并/重审
//!   + memory 重建 + 驱逐清单。
//!
//! 与项目的评分准则/模板对应,JSON 输出(`response_format: json_object` +
//! `enable_thinking: false`),解析失败由调用侧降级。

use crate::llm::micro::extract_json;
use crate::llm::{ChatMessage, ChatRequest, LlmClient};
use serde::{Deserialize, Serialize};

/// 历史条目 = 消息 + 评分元数据。
/// `score` 0 = 未评(后台评分失败/尚未完成),驱逐时按 B 级(3)缺省;审计会覆盖。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub role: String,
    pub content: String,
    pub score: u8,
    /// 全局递增序号:时间线排序(驱逐同分 seq 升序 = 最老先丢)
    pub seq: u64,
    pub ts: String,
}

impl HistoryEntry {
    pub fn user(content: String, seq: u64, ts: String) -> Self {
        Self { role: "user".into(), content, score: 0, seq, ts }
    }
    pub fn assistant(content: String, seq: u64, ts: String) -> Self {
        Self { role: "assistant".into(), content, score: 0, seq, ts }
    }
    pub fn tool_result(content: String, seq: u64, ts: String) -> Self {
        Self { role: "user".into(), content, score: 3, seq, ts }
    }
    pub fn with_score(mut self, score: u8) -> Self {
        self.score = score.min(5);
        self
    }
}

// ---------- 模板(评分准则共享;逐条与审计口径一致) ----------

/// 五级评分准则(§3.1):拿不准 → 往低打(宁可多丢,临时窗口 + 审计重评兜底)
pub const SCORE_RUBRIC: &str = "评分准则(1~5 分,维度 = 该轮内容后续被复用的可能性与重要性):\n\
5 分(S):约束、决定、用户偏好、任务目标、未完成任务 —— 必须原样记住;\n\
4 分(A):重要事实、结论、关键上下文;\n\
3 分(B):一般但有信息量;\n\
2 分(C):低价值(重复、含糊、可推演);\n\
1 分(D):无意义(问候/寒暄/客套/语气词)。\n\
拿不准 → 往低打。";

fn score_system() -> String {
    format!(
        "你是 localai 的对话评分器。下面是一条对话消息。\n{SCORE_RUBRIC}\n\
         输出 JSON(不要其他文字):\n\
         {{\"score\": 1~5, \"critical\": \"仅当 score=5 时的原样关键记忆条目,否则省略\"}}\n\n\
         要求:\n\
         - critical 必须原样保留该消息中的约束/决定/偏好原文,不要改写;\n\
         - 拿不准时按准则的\"往低打\"处理。"
    )
}

fn audit_system(memory_max_chars: usize, critical_max_items: usize, reaudit: bool) -> String {
    let reaudit_block = if reaudit {
        "\n4. 附带了一份\"待重审的旧关键记忆清单\":逐条判断 —— 已过时/被后续覆盖 → 淘汰;\n\
         仍有效 → 保留(可与新条目合并同类);输出合并后的最终 critical 清单。\n"
    } else {
        ""
    };
    format!(
        "你是 localai 的上下文压缩器。下面是一段历史对话,每条消息标注了序号(seq)。\n\
         任务(一次调用完成):\n\
         1. 给每条消息评分:{SCORE_RUBRIC}\n\
         2. 处置:\n\
            - 5 分 → 写进 critical(原样保留,不压缩);\n\
            - 4/3 分 → 压缩进 memory(信息密度压缩:只留事实/决定/约束/偏好/未完成任务,\n\
              去掉修辞与过程性废话);\n\
            - 2/1 分 → 不写进任何输出(dropped);\n\
         3. 输出 JSON(不要其他文字):\n\
            {{\"scores\": [{{\"seq\": 1, \"score\": 5}}, ...],\n\
              \"critical\": [\"...\", \"...\"],\n\
              \"memory\": \"...\",\n\
              \"dropped_seqs\": [3, 7]}}\n\
         {reaudit_block}\
         要求:\n\
         - memory 不超过 {memory_max_chars} 字,中文,信息密度最大化;\n\
         - critical 每条一句话、可独立引用,不超过 {critical_max_items} 条;\n\
         - 最近 3 条是当前任务上下文,一律保留,不评分不丢弃。"
    )
}

// ---------- Compressor ----------

/// 逐条评分结果(触发器 A)
pub struct EntryScore {
    pub score: u8,
    pub critical: Option<String>,
}

/// 审计结果(触发器 B)
#[derive(Debug, Default)]
pub struct AuditOutcome {
    /// (seq, 新分数):调用侧写回每个条目
    pub scores: Vec<(u64, u8)>,
    /// 合并/重审后的关键记忆
    pub critical: Vec<String>,
    /// 次级记忆(≤ memory_max_chars)
    pub memory: String,
    /// 被判定丢弃的条目 seq(调用侧按驱逐算法 + 临时窗口豁免后移除)
    pub dropped_seqs: Vec<u64>,
}

/// 压缩器:两个入口 —— 逐条评分(后台)+ 全量审计(阈值)。
pub struct Compressor {
    client: LlmClient,
    memory_max_chars: usize,
    critical_max_items: usize,
}

impl Compressor {
    pub fn new(client: LlmClient, memory_max_chars: usize, critical_max_items: usize) -> Self {
        Self { client, memory_max_chars, critical_max_items }
    }

    /// 触发器 A:单条评分(输入 = 准则 + 单条消息,输出 = 分数 + 可选 S 级记忆)。
    pub fn score_entry(&self, entry: &HistoryEntry) -> anyhow::Result<EntryScore> {
        let req = ChatRequest {
            model: self.client.model(),
            messages: vec![
                ChatMessage { role: "system".into(), content: score_system() },
                ChatMessage { role: entry.role.clone(), content: entry.content.clone() },
            ],
            max_tokens: 200,
            json: true,
            enable_thinking: false,
        };
        let resp = self.client.chat(&req)?;
        parse_score(&resp.content)
    }

    /// 触发器 B:全量审计(重评分 + critical + memory + 驱逐清单)。
    /// `old_critical` 仅在达到重审门槛时传入(调用侧判断),让模型一并重审淘汰过时约束。
    pub fn audit(
        &self,
        entries: &[HistoryEntry],
        old_critical: &[String],
    ) -> anyhow::Result<AuditOutcome> {
        let reaudit = !old_critical.is_empty();
        let mut msgs = vec![ChatMessage {
            role: "system".into(),
            content: audit_system(self.memory_max_chars, self.critical_max_items, reaudit),
        }];
        for e in entries {
            msgs.push(ChatMessage {
                role: "user".into(),
                content: format!("<seq={}> [{}]\n{}", e.seq, e.role, e.content),
            });
        }
        if reaudit {
            msgs.push(ChatMessage {
                role: "user".into(),
                content: format!(
                    "待重审的旧关键记忆清单:\n{}",
                    old_critical.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n")
                ),
            });
        }
        let req = ChatRequest {
            model: self.client.model(),
            messages: msgs,
            max_tokens: 500,
            json: true,
            enable_thinking: false,
        };
        let resp = self.client.chat(&req)?;
        parse_audit(&resp.content)
    }
}

// ---------- 解析(JSON 输出,失败由调用侧降级) ----------

fn parse_score(content: &str) -> anyhow::Result<EntryScore> {
    let v = extract_json(content).ok_or_else(|| anyhow::anyhow!("评分输出无 JSON: {content}"))?;
    let score = v["score"].as_u64().unwrap_or(3).clamp(1, 5) as u8;
    let critical = v["critical"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    Ok(EntryScore { score, critical })
}

fn parse_audit(content: &str) -> anyhow::Result<AuditOutcome> {
    let v = extract_json(content).ok_or_else(|| anyhow::anyhow!("审计输出无 JSON: {content}"))?;
    let mut outcome = AuditOutcome::default();
    if let Some(scores) = v["scores"].as_array() {
        for s in scores {
            let seq = s["seq"].as_u64().unwrap_or(0);
            let score = s["score"].as_u64().unwrap_or(3).clamp(1, 5) as u8;
            outcome.scores.push((seq, score));
        }
    }
    if let Some(critical) = v["critical"].as_array() {
        outcome.critical = critical
            .iter()
            .filter_map(|c| c.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    outcome.memory = v["memory"].as_str().unwrap_or("").trim().to_string();
    if let Some(dropped) = v["dropped_seqs"].as_array() {
        outcome.dropped_seqs = dropped
            .iter()
            .filter_map(|d| d.as_u64())
            .collect();
    }
    Ok(outcome)
}

// ---------- 时间戳 ----------

/// HH:MM:SS 时间戳(与 fs 审计同款)
pub fn now_ts() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:02}:{:02}:{:02}", (d.as_secs() / 3600) % 24, (d.as_secs() / 60) % 60, d.as_secs() % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_score_handles_json_and_garbage() {
        let ok = parse_score(r#"{"score": 5, "critical": "约束:始终用中文"}"#).unwrap();
        assert_eq!(ok.score, 5);
        assert_eq!(ok.critical.as_deref(), Some("约束:始终用中文"));
        // 带前后缀噪声(模型偶尔多写)
        let noisy = parse_score("好的\n```json\n{\"score\": 1}\n```").unwrap();
        assert_eq!(noisy.score, 1);
        assert!(noisy.critical.is_none());
        // 非 JSON → Err(调用侧降级)
        assert!(parse_score("完全没有 JSON").is_err());
    }

    #[test]
    fn parse_audit_handles_lists_and_clamps() {
        let a = parse_audit(
            r#"{"scores":[{"seq":1,"score":5},{"seq":2,"score":9}],
               "critical":["约束A"],
               "memory":"摘要",
               "dropped_seqs":[3,7]}"#,
        )
        .unwrap();
        assert_eq!(a.scores, vec![(1, 5), (2, 5)], "分数应 clamp 到 1~5");
        assert_eq!(a.critical, vec!["约束A"]);
        assert_eq!(a.memory, "摘要");
        assert_eq!(a.dropped_seqs, vec![3, 7]);
        // 缺字段 → 默认空
        let empty = parse_audit(r#"{"scores":[]}"#).unwrap();
        assert!(empty.critical.is_empty() && empty.memory.is_empty());
    }

    #[test]
    fn templates_embed_rubric_and_params() {
        assert!(score_system().contains("往低打"));
        let a = audit_system(500, 8, false);
        assert!(a.contains("500 字") && a.contains("8 条"));
        assert!(!a.contains("重审"));
        let b = audit_system(500, 8, true);
        assert!(b.contains("待重审的旧关键记忆清单"));
    }
}
