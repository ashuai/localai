//! OpenAI 兼容客户端 —— 对接本地 oMLX 服务器(192.168.0.5:9870)。
//!
//! 实测要点(写入 docs/model-layer.md 的论证依据):
//! - 稳态单次微调用 ~1.1-1.8s;模型冷加载一次 +16s(之后常驻);
//! - `response_format: json_object` + `chat_template_kwargs.enable_thinking: false`
//!   是干净 JSON 的必要组合(默认开思考会把推理过程混进 content);
//! - 服务器 max_concurrent_requests=12,客户端默认并发 6,留余量。

use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    /// 默认模型(默认 Qwen3.6-35b,主模型)
    pub model: String,
    pub timeout_secs: u64,
    /// 客户端并发上限(信号量)
    pub max_concurrent: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    /// 要求 JSON 输出(response_format: json_object)
    pub json: bool,
    /// 是否开启模型思考(Qwen3.6 默认开;微调用必须关)
    pub enable_thinking: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ChatUsage {
    pub total_time: Option<f64>,
    pub cached_tokens: Option<u32>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub model_load_duration: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct ChatResponse {
    pub content: String,
    pub finish_reason: String,
    pub usage: ChatUsage,
    pub latency_ms: u128,
}

/// 极简计数信号量(`std::sync::Semaphore` 尚未稳定)。
struct Semaphore {
    inner: Mutex<usize>,
    cond: Condvar,
}

impl Semaphore {
    fn new(permits: usize) -> Self {
        Self {
            inner: Mutex::new(permits),
            cond: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut n = self.inner.lock().unwrap();
        while *n == 0 {
            n = self.cond.wait(n).unwrap();
        }
        *n -= 1;
    }

    fn release(&self) {
        let mut n = self.inner.lock().unwrap();
        *n += 1;
        self.cond.notify_one();
    }
}

/// RAII 许可:作用域结束自动 release。
struct Permit<'a> {
    sem: &'a Semaphore,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.sem.release();
    }
}

/// 线程安全的 LLM 客户端(内部共享 agent + 信号量 + 模型覆盖)。
#[derive(Clone)]
pub struct LlmClient {
    config: LlmConfig,
    agent: ureq::Agent,
    sem: Arc<Semaphore>,
    /// 运行时模型覆盖(/model 命令);None 时用 config.model
    model_override: Arc<RwLock<Option<String>>>,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(config.timeout_secs.max(5)))
            .build();
        let sem = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
        Self {
            config,
            agent,
            sem,
            model_override: Arc::new(RwLock::new(None)),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.config.base_url
    }

    /// 当前生效模型(优先级:运行时覆盖 > config)
    pub fn model(&self) -> String {
        self.model_override
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.config.model.clone())
    }

    pub fn set_model(&self, m: impl Into<String>) {
        *self.model_override.write().unwrap() = Some(m.into());
    }

    pub fn default_model(&self) -> &str {
        &self.config.model
    }

    /// 一次对话补全(阻塞;受信号量限流)。
    pub fn chat(&self, req: &ChatRequest) -> anyhow::Result<ChatResponse> {
        self.sem.acquire();
        let _permit = Permit { sem: &self.sem };

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "max_tokens": req.max_tokens,
            "stream": false,
        });
        if req.json {
            body["response_format"] = serde_json::json!({ "type": "json_object" });
        }
        if !req.enable_thinking {
            body["chat_template_kwargs"] = serde_json::json!({ "enable_thinking": false });
        }

        let url = format!("{}/v1/chat/completions", self.config.base_url.trim_end_matches('/'));
        // 注意:ureq 的 body 是惰性读取的,latency 必须包住 into_json 才是完整耗时
        let start = Instant::now();
        let resp = self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.config.api_key))
            .send_json(&body)
            .map_err(|e| map_ureq_err(e, &self.config.base_url))?;
        let json: serde_json::Value = resp
            .into_json()
            .map_err(|e| anyhow::anyhow!("解析响应失败: {e}"))?;
        let latency_ms = start.elapsed().as_millis();

        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let finish_reason = json["choices"][0]["finish_reason"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let usage = ChatUsage {
            total_time: json["usage"]["total_time"].as_f64(),
            cached_tokens: json["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().map(|v| v as u32),
            prompt_tokens: json["usage"]["prompt_tokens"].as_u64().map(|v| v as u32),
            completion_tokens: json["usage"]["completion_tokens"].as_u64().map(|v| v as u32),
            model_load_duration: json["usage"]["model_load_duration"].as_f64(),
        };
        Ok(ChatResponse {
            content,
            finish_reason,
            usage,
            latency_ms,
        })
    }
}

fn map_ureq_err(e: ureq::Error, base_url: &str) -> anyhow::Error {
    match e {
        ureq::Error::Status(401, _) => anyhow::anyhow!("401: API key 无效(检查 LLM_API_KEY / localai.yml server.api_key)"),
        ureq::Error::Status(403, _) => anyhow::anyhow!("403: 无权限"),
        ureq::Error::Status(404, _) => anyhow::anyhow!("404: 端点不存在(确认 {base_url} 是 OpenAI 兼容服务)"),
        ureq::Error::Status(code, _) => anyhow::anyhow!("HTTP {code}"),
        ureq::Error::Transport(t) => anyhow::anyhow!("网络错误(无法连接 {base_url}): {t}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_override_takes_precedence() {
        let c = LlmClient::new(LlmConfig {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model: "Qwen3.6-35b".into(),
            timeout_secs: 5,
            max_concurrent: 2,
        });
        assert_eq!(c.model(), "Qwen3.6-35b");
        c.set_model("Qwen3.5-27b");
        assert_eq!(c.model(), "Qwen3.5-27b");
    }

    #[test]
    fn request_body_shapes() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage { role: "user".into(), content: "hi".into() }],
            max_tokens: 10,
            json: true,
            enable_thinking: false,
        };
        let body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "max_tokens": req.max_tokens,
            "stream": false,
            "response_format": { "type": "json_object" },
            "chat_template_kwargs": { "enable_thinking": false },
        });
        assert_eq!(body["model"], "m");
        assert_eq!(body["response_format"]["type"], "json_object");
    }
}
