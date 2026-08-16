//! microtask 插件:模型层"小上下文、零成本、频繁微调用"的活体演示。
//!
//! 1. **环境微调用**:每次助手回复后,并行 2 个微调用(意图分类 + 关键词提取),
//!    发一行状态 —— 展示"什么小事都能调"的零成本用法;
//! 2. **`/micro` 流水线**:串行决策 → 并行 fan-out(标题 + 关键词 + 摘要),
//!    带每阶段耗时 —— 展示确定性编排 + 有界并发的编排模式。

use crate::cordis::context::Context;
use crate::cordis::plugin::Plugin;
use crate::cordis::service::Service;
use crate::events::{SessionReply, SessionStatus};
use crate::llm::{LlmService, MicroEngine, MicroTask};
use std::sync::Arc;

pub fn factory() -> Box<dyn Plugin> {
    Box::new(MicroTaskPlugin)
}

pub struct MicroTaskPlugin;

#[derive(serde::Deserialize, Default)]
pub struct MicroOptions {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
}

fn default_retries() -> u32 {
    2
}

/// 从 JSON 里取"第一个可见值"做展示:字符串直出,数组转逗号串,对象取首值。
fn first_value_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_str())
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Object(map) => map
            .values()
            .next()
            .map(first_value_string)
            .unwrap_or_else(|| "?".to_string()),
        _ => v.to_string(),
    }
}

pub struct MicroService {
    pub engine: MicroEngine,
}

impl Service for MicroService {
    fn service_name_static() -> &'static str {
        "micro"
    }
}

// 固定任务模板:保持稳定 → 命中 oMLX prompt 缓存;输出强制 JSON
const CLASSIFY_SYSTEM: &str = "你是意图分类器。把用户消息分类为:greeting(问候)、question(提问)、command(指令)、chitchat(闲聊)。只输出 JSON:{\"intent\": \"...\", \"confidence\": 0.0-1.0 的小数}";
const TAGS_SYSTEM: &str = "你是关键词提取器。从文本提取 1-3 个最核心的关键词/短语。只输出 JSON:{\"tags\": [\"...\"]}";
const TITLE_SYSTEM: &str = "你是标题生成器。为给定内容生成不超过 12 个字的简短标题。只输出 JSON:{\"title\": \"...\"}";
const SUMMARY_SYSTEM: &str = "你是摘要器。把用户输入压缩成不超过 40 字的一句话摘要。只输出 JSON:{\"summary\": \"...\"}";

impl Plugin for MicroTaskPlugin {
    fn name(&self) -> &'static str {
        "microtask"
    }

    fn inject(&self) -> &'static [&'static str] {
        &["llm"]
    }

    fn apply(&self, ctx: &Context) -> anyhow::Result<()> {
        let llm = ctx.inject::<LlmService>().ok_or_else(|| anyhow::anyhow!("缺少 llm 服务"))?;
        let opts: MicroOptions = ctx.options()?;
        let client = llm.client.clone();
        if let Some(m) = &opts.model {
            client.set_model(m.clone());
        }
        let engine = MicroEngine::new(client, opts.max_retries);
        let svc = Arc::new(MicroService { engine });
        ctx.provide(Arc::clone(&svc));

        // ---- 环境微调用:每次回复后并行 2 个小调用,零成本增值 ----
        let svc2 = Arc::clone(&svc);
        let ctx2 = ctx.clone();
        ctx.on(move |ev: &SessionReply| {
            let svc = Arc::clone(&svc2);
            let ctx = ctx2.clone();
            let user = ev.user_text.clone();
            let reply = ev.text.clone();
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let results = svc.engine.call_parallel(&[
                    MicroTask {
                        name: "classify",
                        system: CLASSIFY_SYSTEM.into(),
                        input: user.clone(),
                        max_tokens: 60,
                    },
                    MicroTask {
                        name: "tags",
                        system: TAGS_SYSTEM.into(),
                        input: reply.clone(),
                        max_tokens: 80,
                    },
                ]);
                let mut parts = Vec::new();
                if let Ok(o) = &results[0] {
                    if let Some(v) = &o.json {
                        if let Some(intent) = v["intent"].as_str() {
                            parts.push(format!("意图={intent}"));
                        }
                    }
                }
                if let Ok(o) = &results[1] {
                    if let Some(v) = &o.json {
                        if let Some(tags) = v["tags"].as_array() {
                            let joined: Vec<String> = tags
                                .iter()
                                .filter_map(|t| t.as_str().map(String::from))
                                .collect();
                            if !joined.is_empty() {
                                parts.push(format!("关键词={}", joined.join(",")));
                            }
                        }
                    }
                }
                ctx.emit(SessionStatus {
                    text: if parts.is_empty() {
                        format!("[micro] 环境微调用失败 ({:?})", start.elapsed())
                    } else {
                        format!("[micro] {} ({:?})", parts.join(" | "), start.elapsed())
                    },
                });
            });
        });

        // ---- /micro 命令:3 阶段流水线演示(串行决策 → 并行 fan-out) ----
        let ctx3 = ctx.clone();
        let svc3 = Arc::clone(&svc);
        ctx.on_command("micro", move |rest: &str| {
            let svc = Arc::clone(&svc3);
            let ctx = ctx3.clone();
            let input = if rest.is_empty() {
                "帮我写一个 rust 的 http server".to_string()
            } else {
                rest.to_string()
            };
            ctx.emit(SessionStatus { text: format!("[micro] 流水线开始: {input}") });
            std::thread::spawn(move || {
                let t0 = std::time::Instant::now();
                // 阶段 1:意图分类(串行决策)
                let classify = svc.engine.call(&MicroTask {
                    name: "classify",
                    system: CLASSIFY_SYSTEM.into(),
                    input: input.clone(),
                    max_tokens: 60,
                });
                let t1 = std::time::Instant::now();
                let intent = classify
                    .as_ref()
                    .ok()
                    .and_then(|o| o.json.as_ref())
                    .and_then(|v| v["intent"].as_str().map(String::from))
                    .unwrap_or_else(|| "解析失败".to_string());
                // 阶段 2:并行 fan-out(标题 + 关键词 + 摘要)
                let outs = svc.engine.call_parallel(&[
                    MicroTask { name: "title", system: TITLE_SYSTEM.into(), input: input.clone(), max_tokens: 60 },
                    MicroTask { name: "tags", system: TAGS_SYSTEM.into(), input: input.clone(), max_tokens: 80 },
                    MicroTask { name: "summary", system: SUMMARY_SYSTEM.into(), input: input.clone(), max_tokens: 120 },
                ]);
                let t2 = std::time::Instant::now();
                let mut lines = vec![format!("[micro] 意图: {intent} (阶段1 {:?})", t1 - t0)];
                for o in &outs {
                    match o {
                        Ok(o) => {
                            let value = o
                                .json
                                .as_ref()
                                .map(first_value_string)
                                .unwrap_or_else(|| "?".to_string());
                            lines.push(format!("[micro] {:<8} {value} ({:?})", o.task, o.latency_ms));
                        }
                        Err(e) => lines.push(format!("[micro] {e:#}")),
                    }
                }
                lines.push(format!(
                    "[micro] 阶段1={:?} 阶段2(3 并行)={:?} 总计={:?}",
                    t1 - t0,
                    t2 - t1,
                    t2 - t0
                ));
                for l in lines {
                    ctx.emit(SessionStatus { text: l });
                }
            });
            "micro 流水线已启动(4 次微调用,约 2-4 秒)".to_string()
        });

        Ok(())
    }
}
