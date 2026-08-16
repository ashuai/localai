//! localai 入口:读配置 → 装配 Loader(注入 llm 核心服务)→ 加载插件 → 进入交互。
//!
//! 用法:
//! - `localai`                启动 TUI
//! - `localai --once <文本>`  非交互跑一轮(chat 插件),打印回复后退出
//! - `localai --list-plugins` 列出插件与加载状态
//! - `localai --self-test`    装配自检

use anyhow::Context as _;
use localai::cordis::loader::Loader;
use localai::events::{SessionReply, SessionStatus};
use localai::llm::{LlmClient, LlmConfig};
use localai::plugins;
use serde::Deserialize;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Deserialize)]
struct AppConfig {
    server: ServerCfg,
    #[serde(default)]
    plugins: Vec<PluginCfg>,
}

#[derive(Deserialize)]
struct ServerCfg {
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default = "default_model")]
    default_model: String,
    #[serde(default = "default_concurrent")]
    max_concurrent: usize,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_model() -> String {
    "Qwen3.6-35b".into()
}
fn default_concurrent() -> usize {
    6
}
fn default_timeout() -> u64 {
    120
}

#[derive(Deserialize)]
struct PluginCfg {
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    options: serde_yaml::Value,
}

fn default_true() -> bool {
    true
}

fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().collect();

    let cfg_text = std::fs::read_to_string("localai.yml")
        .context("读取 localai.yml 失败(请在项目根目录运行)")?;
    let cfg: AppConfig = serde_yaml::from_str(&cfg_text).context("解析 localai.yml 失败")?;

    let api_key = resolve_key(&cfg.server)?;
    let client = LlmClient::new(LlmConfig {
        base_url: cfg.server.base_url.clone(),
        api_key,
        model: cfg.server.default_model.clone(),
        timeout_secs: cfg.server.timeout_secs,
        max_concurrent: cfg.server.max_concurrent,
    });

    let mut loader = Loader::new(client, plugins::builtin());
    for p in &cfg.plugins {
        if p.enabled {
            loader
                .load(&p.name, p.options.clone())
                .with_context(|| format!("加载插件 {}", p.name))?;
        }
    }

    match args.get(1).map(|s| s.as_str()) {
        Some("--list-plugins") => {
            for p in loader.list() {
                println!("{:<12} {}", p.name, if p.loaded { "已加载" } else { "未加载" });
            }
            println!("核心服务: {}", loader.root().service_names().join(", "));
            Ok(())
        }
        Some("--once") => {
            let text = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "你好".to_string());
            run_once(&loader, &text)
        }
        Some("--micro") => {
            let text = args.get(2).cloned().unwrap_or_default();
            run_micro(&loader, &text)
        }
        Some("--self-test") => {
            println!("核心服务: {}", loader.root().service_names().join(", "));
            for p in loader.list() {
                println!(
                    "插件 {:<12} {}",
                    p.name,
                    if p.loaded { "已加载 ✓" } else { "未加载" }
                );
            }
            println!("自检完成");
            Ok(())
        }
        _ => localai::tui::run(Arc::new(Mutex::new(loader))),
    }
}

/// API key 解析优先级:api_key_env 指定环境变量 > api_key 字段。
fn resolve_key(server: &ServerCfg) -> anyhow::Result<String> {
    if let Some(env) = &server.api_key_env {
        if let Ok(v) = std::env::var(env) {
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    if let Some(k) = &server.api_key {
        if !k.is_empty() {
            return Ok(k.clone());
        }
    }
    anyhow::bail!(
        "未配置 API key:请设置环境变量 {} 或在 localai.yml server.api_key 中配置",
        server.api_key_env.as_deref().unwrap_or("LLM_API_KEY")
    )
}

/// `--once` 模式:走与 TUI 相同的事件路径(session/input → chat → session/reply)。
fn run_once(loader: &Loader, text: &str) -> anyhow::Result<()> {
    let root = loader.root().clone();
    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    root.on(move |ev: &SessionReply| {
        let _ = tx2.send(ev.text.clone());
    });
    root.on(move |ev: &SessionStatus| {
        eprintln!("  [status] {}", ev.text);
    });
    println!("user> {text}");
    root.emit(localai::events::SessionInput { text: text.to_string() });
    match rx.recv_timeout(Duration::from_secs(180)) {
        Ok(reply) => {
            println!("assistant> {reply}");
            Ok(())
        }
        Err(e) => anyhow::bail!("等待回复超时: {e}"),
    }
}

/// `--micro` 模式:直接跑 microtask 插件的 3 阶段微调用流水线并打印状态行。
fn run_micro(loader: &Loader, text: &str) -> anyhow::Result<()> {
    let root = loader.root().clone();
    let (tx, rx) = mpsc::channel::<String>();
    let tx2 = tx.clone();
    root.on(move |ev: &SessionStatus| {
        let _ = tx2.send(ev.text.clone());
    });
    let out = root
        .run_command(&format!("micro {text}"))
        .unwrap_or_else(|| "micro 命令未注册(插件未加载?)".to_string());
    println!("{out}");
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(line) => println!("{line}"),
            Err(_) => break,
        }
    }
    Ok(())
}
