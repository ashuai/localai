//! tui 插件 —— 交互层插件化(论证见本地文档 `localai-docs/tui-plugin.md`)。
//!
//! 职责(全部走 cordis 通道):
//! - 订阅 `session/reply` `session/status` → mpsc → 渲染;
//! - 注册内置命令 `/help /plugins /load /unload /model /clear /quit`(ctx.on_command,
//!   与 `/micro` 同构;卸载插件命令自动消失);
//! - 提供 [`TuiBackend`] 服务(含主循环),main 装配完成后调用 `run()` 进入交互;
//! - 退出:`/quit` 或 Ctrl-C 置退出标志,主循环每轮检查。

use crate::cordis::context::Context;
use crate::cordis::loader::{Loader, LoaderService};
use crate::cordis::plugin::Plugin;
use crate::cordis::service::Service;
use crate::events::{SessionInput, SessionReply, SessionStatus};
use crate::llm::LlmService;
use crate::tui::app::App;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::DefaultTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn factory() -> Box<dyn Plugin> {
    Box::new(TuiPlugin)
}

pub struct TuiPlugin;

pub enum UiEvent {
    Reply(String),
    Status(String),
}

/// 交互后端服务:main 装配完成后 `inject::<TuiBackend>()?.run()` 进入 TUI。
pub struct TuiBackend {
    root: Context,
    app: Arc<Mutex<App>>,
    /// mpsc::Receiver 不是 Sync,包一层 Mutex 以满足 Service(Send+Sync)
    rx: Mutex<Receiver<UiEvent>>,
    loader: Arc<Mutex<Loader>>,
    quit: Arc<AtomicBool>,
}

impl Service for TuiBackend {
    fn service_name_static() -> &'static str {
        "tui"
    }
}

impl TuiBackend {
    pub fn run(&self) -> anyhow::Result<()> {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            anyhow::bail!("stdin 不是终端(如需脚本模式,请用 --once / --micro / --list-plugins)");
        }
        let mut terminal = ratatui::init();
        let res = self.run_loop(&mut terminal);
        ratatui::restore();
        res
    }

    fn run_loop(&self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        loop {
            if self.quit.load(Ordering::SeqCst) {
                break;
            }
            self.refresh_status();
            terminal.draw(|f| {
                let mut app = self.app.lock().unwrap();
                app.draw(f);
            })?;
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        if !self.on_key(k) {
                            break;
                        }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            while let Ok(ev) = self.rx.lock().unwrap().try_recv() {
                self.apply(ev);
            }
        }
        Ok(())
    }

    fn refresh_status(&self) {
        let (loaded, model) = {
            let l = self.loader.lock().unwrap();
            let loaded = l.list().iter().filter(|p| p.loaded).count();
            let model = self
                .root
                .inject::<LlmService>()
                .map(|s| s.client.model())
                .unwrap_or_else(|| "?".into());
            (loaded, model)
        };
        self.app.lock().unwrap().status =
            format!(" 插件×{loaded} | 模型 {model} | 192.168.0.5:9870 (oMLX) | Ctrl-C 退出 ");
    }

    /// 键盘 → 事件/命令;返回 false 表示退出主循环。
    fn on_key(&self, k: crossterm::event::KeyEvent) -> bool {
        match k.code {
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => false,
            KeyCode::Esc => false,
            KeyCode::Enter => {
                let text = {
                    let mut app = self.app.lock().unwrap();
                    let text = std::mem::take(&mut app.input);
                    app.scroll = 0;
                    text.trim().to_string()
                };
                if text.is_empty() {
                    return true;
                }
                if let Some(cmd) = text.strip_prefix('/') {
                    self.handle_command(cmd);
                } else {
                    self.app.lock().unwrap().push_line(vec![
                        Span::styled(
                            "你 ",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(text.clone()),
                    ]);
                    self.root.emit(SessionInput { text });
                }
                true
            }
            KeyCode::Char(ch) => {
                self.app.lock().unwrap().input.push(ch);
                true
            }
            KeyCode::Backspace => {
                self.app.lock().unwrap().input.pop();
                true
            }
            KeyCode::PageUp => {
                let mut app = self.app.lock().unwrap();
                app.scroll = app.scroll.saturating_add(5);
                true
            }
            KeyCode::PageDown => {
                let mut app = self.app.lock().unwrap();
                app.scroll = app.scroll.saturating_sub(5);
                true
            }
            _ => true,
        }
    }

    /// 命令统一走 `ctx.run_command`(所有命令都是插件注册的)。
    fn handle_command(&self, cmd: &str) {
        match self.root.run_command(cmd) {
            Some(out) => {
                for l in out.lines() {
                    self.app.lock().unwrap().push_status(l.to_string());
                }
            }
            None => {
                let name = cmd.split_whitespace().next().unwrap_or(cmd);
                self.app
                    .lock()
                    .unwrap()
                    .push_status(format!("未知命令: /{name}(/help 查看)"));
            }
        }
    }

    fn apply(&self, ev: UiEvent) {
        match ev {
            UiEvent::Reply(text) => self.app.lock().unwrap().push_line(vec![
                Span::styled(
                    "AI ",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::raw(text),
            ]),
            UiEvent::Status(text) => self.app.lock().unwrap().push_status(text),
        }
    }
}

impl Plugin for TuiPlugin {
    fn name(&self) -> &'static str {
        "tui"
    }

    fn inject(&self) -> &'static [&'static str] {
        &["loader", "llm"]
    }

    fn apply(&self, ctx: &Context) -> anyhow::Result<()> {
        let loader_svc = ctx
            .inject::<LoaderService>()
            .ok_or_else(|| anyhow::anyhow!("缺少 loader 服务"))?;
        let loader = Arc::clone(&loader_svc.loader);
        let llm_client = {
            let llm = ctx
                .inject::<LlmService>()
                .ok_or_else(|| anyhow::anyhow!("缺少 llm 服务"))?;
            llm.client.clone()
        };

        let app = Arc::new(Mutex::new(App::new()));
        let (tx, rx) = mpsc::channel();
        let quit = Arc::new(AtomicBool::new(false));

        // 订阅渲染事件(卸载时自动移除)
        let tx1 = tx.clone();
        ctx.on(move |ev: &SessionReply| {
            let _ = tx1.send(UiEvent::Reply(ev.text.clone()));
        });
        let tx2 = tx;
        ctx.on(move |ev: &SessionStatus| {
            let _ = tx2.send(UiEvent::Status(ev.text.clone()));
        });

        // ---- 内置命令(全部走 ctx.on_command,与 /micro 同构) ----
        let root2 = ctx.clone();
        ctx.on_command("help", move |_: &str| {
            let mut s = String::from(
                "内置命令: /help /plugins /load <插件> /unload <插件> /model [模型] /clear /quit",
            );
            let names = root2.command_names();
            if !names.is_empty() {
                s.push_str(&format!("\n插件命令: /{}", names.join(" /")));
            }
            s
        });

        let loader2 = Arc::clone(&loader);
        ctx.on_command("plugins", move |_: &str| {
            let l = loader2.lock().unwrap();
            let mut s = String::from("插件: ");
            for p in l.list() {
                s.push_str(&format!("{}{} ", p.name, if p.loaded { "✓" } else { "✗" }));
            }
            s
        });

        let loader3 = Arc::clone(&loader);
        ctx.on_command("load", move |args: &str| {
            let args = args.trim();
            if args.is_empty() {
                return "用法: /load <插件名>(/plugins 查看)".into();
            }
            let mut l = loader3.lock().unwrap();
            match l.load(args, serde_yaml::Value::Null) {
                Ok(()) => format!("/load {args} OK"),
                Err(e) => format!("/load {args} 失败: {e:#}"),
            }
        });

        let loader4 = Arc::clone(&loader);
        ctx.on_command("unload", move |args: &str| {
            let args = args.trim();
            if args.is_empty() {
                return "用法: /unload <插件名>".into();
            }
            let mut l = loader4.lock().unwrap();
            match l.unload(args) {
                Ok(()) => format!("/unload {args} OK"),
                Err(e) => format!("/unload {args} 失败: {e:#}"),
            }
        });

        let client = llm_client;
        ctx.on_command("model", move |args: &str| {
            let args = args.trim();
            if args.is_empty() {
                format!("当前模型: {}", client.model())
            } else {
                client.set_model(args.to_string());
                format!("模型已切换: {args}")
            }
        });

        let app_clear = Arc::clone(&app);
        ctx.on_command("clear", move |_: &str| {
            app_clear.lock().unwrap().clear();
            "已清屏".to_string()
        });

        let quit_cmd = Arc::clone(&quit);
        ctx.on_command("quit", move |_: &str| {
            quit_cmd.store(true, Ordering::SeqCst);
            "退出".to_string()
        });

        // ---- 提供交互后端服务(main 注入后 run()) ----
        ctx.provide(Arc::new(TuiBackend {
            root: ctx.clone(),
            app,
            rx: Mutex::new(rx),
            loader,
            quit,
        }));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmClient, LlmConfig};

    /// 装配一个只含 tui 插件的 loader(提供 LoaderService/LlmService,与 main 一致)。
    fn test_loader() -> Arc<Mutex<Loader>> {
        let client = LlmClient::new(LlmConfig {
            base_url: "http://127.0.0.1:9".into(),
            api_key: "test".into(),
            model: "m".into(),
            timeout_secs: 1,
            max_concurrent: 2,
        });
        let loader = Arc::new(Mutex::new(Loader::new(client, vec![factory])));
        loader
            .lock()
            .unwrap()
            .root()
            .provide(Arc::new(LoaderService { loader: Arc::clone(&loader) }));
        loader
            .lock()
            .unwrap()
            .load("tui", serde_yaml::Value::Null)
            .unwrap();
        loader
    }

    #[test]
    fn tui_plugin_registers_commands() {
        let loader = test_loader();
        let root = loader.lock().unwrap().root().clone();
        let plugins_out = root.run_command("plugins").unwrap();
        assert!(plugins_out.contains("tui"), "plugins 输出应含 tui: {plugins_out}");
        let help = root.run_command("help").unwrap();
        assert!(help.contains("/quit"), "help 应列内置命令: {help}");
        assert!(
            help.contains("插件命令: /clear") && help.contains("/quit"),
            "help 应列出插件注册的命令: {help}"
        );
        assert_eq!(root.run_command("quit").as_deref(), Some("退出"));
    }

    #[test]
    fn tui_unload_reverts_commands_and_service() {
        let loader = test_loader();
        loader.lock().unwrap().unload("tui").unwrap();
        let root = loader.lock().unwrap().root().clone();
        assert!(root.run_command("quit").is_none(), "卸载后 quit 命令应消失");
        assert!(root.run_command("plugins").is_none(), "卸载后 plugins 命令应消失");
        assert!(root.inject::<TuiBackend>().is_none(), "TuiBackend 服务应随卸载回收");
    }

    #[test]
    fn tui_run_rejects_non_tty() {
        // cargo test 环境 stdin 非终端 → run() 应报错而非让 ratatui panic
        let loader = test_loader();
        let root = loader.lock().unwrap().root().clone();
        let backend = root.inject::<TuiBackend>().unwrap();
        assert!(backend.run().is_err());
    }
}
