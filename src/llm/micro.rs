//! 微调用协议 —— 本项目的模型层核心论证落地。
//!
//! 前提(本地 LLM,实测):单次调用 ~1.1-1.8s、零成本、小上下文够用。
//! 结论:**智能的最小单位是一次小上下文的有界调用,而不是一条长上下文**。
//!
//! 协议:`MicroTask { name, system, input, max_tokens }` → `MicroOutcome { json, text, latency, retries }`
//! - 固定、稳定的 system 模板(吃 oMLX 的 prompt 缓存);
//! - 可变载荷只装"这一件事"的输入,绝不累积历史;
//! - 强制 JSON 输出 + 关思考,输出可直接被 Rust 确定性消费;
//! - 失败重试:调用零成本,JSON 解析失败直接重试(默认 2 次)。

use crate::llm::client::{ChatMessage, ChatRequest, LlmClient};
use std::time::Instant;

/// 一次微调用:名字 + 固定任务模板 + 最小载荷。
pub struct MicroTask {
    pub name: &'static str,
    /// 固定系统提示词(保持稳定 → 命中缓存)
    pub system: String,
    /// 本次调用的最小输入
    pub input: String,
    pub max_tokens: u32,
}

/// 微调用结果:结构化 JSON 优先,退化时保留原文。
#[derive(Debug)]
pub struct MicroOutcome {
    pub task: &'static str,
    pub json: Option<serde_json::Value>,
    pub text: String,
    pub latency_ms: u128,
    pub retries: u32,
}

/// 微调用引擎:并发有界 + 失败重试。
pub struct MicroEngine {
    client: LlmClient,
    max_retries: u32,
}

impl MicroEngine {
    pub fn new(client: LlmClient, max_retries: u32) -> Self {
        Self { client, max_retries }
    }

    pub fn client(&self) -> &LlmClient {
        &self.client
    }

    /// 单次微调用(JSON + 关思考 + 重试)。
    pub fn call(&self, task: &MicroTask) -> anyhow::Result<MicroOutcome> {
        let mut last_err: Option<anyhow::Error> = None;
        let start = Instant::now();
        for attempt in 0..=self.max_retries {
            let req = ChatRequest {
                model: self.client.model(),
                messages: vec![
                    ChatMessage { role: "system".into(), content: task.system.clone() },
                    ChatMessage { role: "user".into(), content: task.input.clone() },
                ],
                max_tokens: task.max_tokens,
                json: true,
                enable_thinking: false,
            };
            match self.client.chat(&req) {
                Ok(resp) => {
                    if let Some(json) = extract_json(&resp.content) {
                        return Ok(MicroOutcome {
                            task: task.name,
                            json: Some(json),
                            text: resp.content,
                            latency_ms: resp.latency_ms,
                            retries: attempt,
                        });
                    }
                    last_err = Some(anyhow::anyhow!(
                        "JSON 解析失败(尝试 {}/{}): {:.120}",
                        attempt + 1,
                        self.max_retries + 1,
                        resp.content
                    ));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("微调用失败")).context(format!(
            "task={} 耗时={:?}",
            task.name,
            start.elapsed()
        )))
    }

    /// 并行 fan-out:每任务一线程,受客户端信号量限流。
    pub fn call_parallel(&self, tasks: &[MicroTask]) -> Vec<anyhow::Result<MicroOutcome>> {
        std::thread::scope(|s| {
            let handles: Vec<_> = tasks.iter().map(|t| s.spawn(move || self.call(t))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })
    }
}

/// 从模型输出里提取 JSON(容忍围栏/前后缀/裁剪)。
pub fn extract_json(text: &str) -> Option<serde_json::Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        return Some(v);
    }
    // ```json ... ``` 围栏
    if let Some(rest) = t.strip_prefix("```json") {
        if let Some(inner) = rest.strip_suffix("```") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(inner.trim()) {
                return Some(v);
            }
        }
    }
    // 兜底:第一个 { 到最后一个 } 之间的内容
    if let (Some(s), Some(e)) = (t.find('{'), t.rfind('}')) {
        if e > s {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t[s..=e]) {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_plain() {
        let v = extract_json(r#"{"intent": "question"}"#).unwrap();
        assert_eq!(v["intent"], "question");
    }

    #[test]
    fn extract_json_fenced() {
        let v = extract_json("```json\n{\"intent\": \"command\"}\n```").unwrap();
        assert_eq!(v["intent"], "command");
    }

    #[test]
    fn extract_json_noisy() {
        let v = extract_json("好的,结果如下:{\"tags\": [\"a\", \"b\"]} 完毕").unwrap();
        assert_eq!(v["tags"][0], "a");
    }

    #[test]
    fn extract_json_garbage() {
        assert!(extract_json("完全没有 json").is_none());
    }
}
