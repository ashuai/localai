//! tui 插件 —— 交互层插件化(论证见本地文档 `localai-docs/tui-plugin.md`)。
//!
//! 职责(全部走 cordis 通道):
//! - 订阅 `session/reply` `session/status` → mpsc → 渲染;
//! - 注册内置命令 `/help /plugins /load /unload /model /clear /quit`(ctx.on_command,
//!   与 `/micro` 同构;卸载插件命令自动消失);
//! - 提供 [`TuiBackend`] 服务(含主循环),main 装配完成后调用 `run()` 进入交互;
//! - 退出:双击 Ctrl+C 或 `/quit` 置退出标志,主循环每轮检查;Esc 不再退出
//!   (优先中断 chat 调用,否则清空输入框);↑↓ 浏览发送历史。

use crate::cordis::context::Context;
use crate::cordis::loader::{Loader, LoaderService};
use crate::cordis::plugin::Plugin;
use crate::cordis::service::Service;
use crate::events::{SessionInput, SessionReply, SessionStatus};
use crate::llm::LlmService;
use crate::plugins::chat::ChatService;
use crate::tui::app::App;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::DefaultTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// 双击 Ctrl+C 退出:第一次按下记录时间,窗口内再按才真正退出(防误按)
    exit_arm: Arc<Mutex<Option<Instant>>>,
    /// 已发送的输入历史(旧→新;仅 Enter 发送的非命令输入)
    sent: Arc<Mutex<Vec<String>>>,
    /// 输入历史浏览状态(主循环线程访问;Arc 共享故用 Mutex 包)
    hist: Mutex<HistState>,
}

/// 输入历史浏览状态:`pos=0` 显示当前草稿;`pos>0` 显示倒数第 pos 条已发送。
#[derive(Default)]
struct HistState {
    pos: usize,
    draft: String,
}

/// 双击 Ctrl+C 的确认窗口:第一次按下后 2 秒内再按才退出
const EXIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

/// RAII 守卫:作用域结束(含 panic 展开)时关闭 bracketed paste,避免终端残留粘贴模式。
struct DisableBracketedPasteOnDrop;

impl Drop for DisableBracketedPasteOnDrop {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    }
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
        // 开启 bracketed paste:crossterm 不感知输入法(IME),中文/日文等 IME 提交的文本
        // 以及用户粘贴的内容,终端(Ghostty/iTerm2 等)会包装成 `\x1B[200~…\x1B[201~`
        // 以 Paste 事件送达;不开启时这些文本被 crossterm 当作按键流丢弃 → 无法输入中文。
        let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
        // RAII:退出(含 panic)时关掉 bracketed paste,避免终端残留粘贴模式
        let _guard = DisableBracketedPasteOnDrop;
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
                    // 中文输入法(IME)提交 / 用户粘贴的文本:bracketed paste 模式下的 Paste 事件
                    Event::Paste(text) => self.on_paste(&text),
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
        let (loaded, model, base_url, busy) = {
            let l = self.loader.lock().unwrap();
            let loaded = l.list().iter().filter(|p| p.loaded).count();
            // base_url/model 都来自 localai.yml(server 段),不要硬编码
            let (model, base_url) = self
                .root
                .inject::<LlmService>()
                .map(|s| (s.client.model(), s.client.base_url().to_string()))
                .unwrap_or_else(|| ("?".into(), "?".into()));
            // chat 插件可选:未加载(卸载后)不影响状态栏
            let busy = self
                .root
                .inject::<ChatService>()
                .map(|c| c.is_busy())
                .unwrap_or(false);
            (loaded, model, base_url, busy)
        };
        let mut app = self.app.lock().unwrap();
        app.status = if self.exit_armed() {
            " 再按一次 Ctrl+C 退出 ".to_string()
        } else if busy {
            format!(" 插件×{loaded} | 模型 {model} | {base_url} | ● 调用中(Esc 中断) ")
        } else {
            format!(" 插件×{loaded} | 模型 {model} | {base_url} | Ctrl-C ×2 退出 ")
        };
    }

    /// 键盘 → 事件/命令;返回 false 表示退出主循环。
    fn on_key(&self, k: crossterm::event::KeyEvent) -> bool {
        // 双击 Ctrl+C 退出:除 Ctrl+C 外的任意按键都会取消"再按一次"的确认状态
        let is_ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
        if !is_ctrl_c {
            self.disarm_exit();
        }
        match k.code {
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                // 第一次只提示并进入确认状态;窗口内第二次才退出(防误按)
                !self.arm_exit()
            }
            // Esc 不再退出:优先中断进行中的调用;否则清空输入框
            KeyCode::Esc => {
                self.on_esc();
                true
            }
            KeyCode::Up => {
                self.history_up();
                true
            }
            KeyCode::Down => {
                self.history_down();
                true
            }
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
                    // 记入发送历史(有界,只留最近 200 条)
                    {
                        let mut sent = self.sent.lock().unwrap();
                        sent.push(text.clone());
                        if sent.len() > 200 {
                            sent.remove(0);
                        }
                    }
                    {
                        let mut hist = self.hist.lock().unwrap();
                        hist.pos = 0;
                        hist.draft.clear();
                    }
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
                let mut app = self.app.lock().unwrap();
                self.reset_hist(&mut app); // 编辑即退出历史浏览,恢复草稿
                app.input.push(ch);
                true
            }
            KeyCode::Backspace => {
                let mut app = self.app.lock().unwrap();
                self.reset_hist(&mut app);
                app.input.pop();
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

    /// Esc:优先中断进行中的调用;否则清空输入框。
    fn on_esc(&self) {
        let cancelled = self
            .root
            .inject::<ChatService>()
            .map(|c| c.cancel_current())
            .unwrap_or(false);
        let mut app = self.app.lock().unwrap();
        if cancelled {
            app.push_status("已中断当前调用(再按一次 Esc 清空输入框)".into());
        } else {
            app.input.clear();
        }
    }

    /// ↑:浏览已发送历史(首次进入时暂存当前未发送的草稿)。
    fn history_up(&self) {
        let mut app = self.app.lock().unwrap();
        let sent = self.sent.lock().unwrap();
        if sent.is_empty() {
            return;
        }
        let mut hist = self.hist.lock().unwrap();
        if hist.pos == 0 {
            hist.draft = app.input.clone();
        }
        if hist.pos < sent.len() {
            hist.pos += 1;
            app.input = sent[sent.len() - hist.pos].clone();
        }
    }

    /// ↓:向新方向回退(回到当前草稿)。
    fn history_down(&self) {
        let mut app = self.app.lock().unwrap();
        let sent = self.sent.lock().unwrap();
        let mut hist = self.hist.lock().unwrap();
        if hist.pos > 0 {
            hist.pos -= 1;
            app.input = if hist.pos == 0 {
                hist.draft.clone()
            } else {
                sent[sent.len() - hist.pos].clone()
            };
        }
    }

    /// 编辑输入框时退出历史浏览:恢复草稿并清空暂存。
    fn reset_hist(&self, app: &mut std::sync::MutexGuard<'_, App>) {
        let mut hist = self.hist.lock().unwrap();
        if hist.pos != 0 {
            hist.pos = 0;
            app.input = std::mem::take(&mut hist.draft);
        }
    }

    /// 双击 Ctrl+C 退出:第一次按下进入确认状态并提示;确认窗口内第二次返回 true(退出)。
    fn arm_exit(&self) -> bool {
        let mut arm = self.exit_arm.lock().unwrap();
        let now = Instant::now();
        let already_armed = matches!(
            arm.as_ref(),
            Some(t) if now.duration_since(*t) <= EXIT_CONFIRM_WINDOW
        );
        if already_armed {
            true
        } else {
            *arm = Some(now);
            false
        }
    }

    /// 取消"再按一次退出"的确认状态(任意其他按键触发)。
    fn disarm_exit(&self) {
        self.exit_arm.lock().unwrap().take();
    }

    /// 是否处于退出确认状态(窗口内第一次 Ctrl+C 之后)。
    fn exit_armed(&self) -> bool {
        let arm = self.exit_arm.lock().unwrap();
        matches!(arm.as_ref(), Some(t) if t.elapsed() <= EXIT_CONFIRM_WINDOW)
    }

    /// 粘贴 / 输入法(IME)提交的文本 → 追加到输入框。
    fn on_paste(&self, text: &str) {
        self.app.lock().unwrap().input.push_str(text);
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
            exit_arm: Arc::new(Mutex::new(None)),
            sent: Arc::new(Mutex::new(Vec::new())),
            hist: Mutex::new(HistState::default()),
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

    #[test]
    fn tui_paste_appends_ime_text_to_input() {
        // 中文输入法(IME)提交的文本走 Paste 事件 → on_paste 追加到输入框
        let loader = test_loader();
        let root = loader.lock().unwrap().root().clone();
        let backend = root.inject::<TuiBackend>().unwrap();
        backend.on_paste("你好，世界");
        assert_eq!(backend.app.lock().unwrap().input, "你好，世界");
    }

    #[test]
    fn tui_ctrl_c_requires_double_press() {
        // 双击 Ctrl+C 才退出:第一次只进入确认状态,第二次(窗口内)才返回退出
        let loader = test_loader();
        let root = loader.lock().unwrap().root().clone();
        let backend = root.inject::<TuiBackend>().unwrap();
        let ctrl_c = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        // 第一次 Ctrl+C:不退出
        assert!(backend.on_key(ctrl_c), "第一次 Ctrl+C 不应退出");
        assert!(backend.exit_armed(), "第一次按下后应进入确认状态");
        // 确认窗口内第二次:退出
        assert!(!backend.on_key(ctrl_c), "窗口内第二次 Ctrl+C 应退出");
        // 任意其他键取消确认状态 → 之后单次 Ctrl+C 又只是重新进入确认
        backend.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!backend.exit_armed(), "其他按键应取消确认状态");
        assert!(backend.on_key(ctrl_c), "取消后再按一次 Ctrl+C 不应退出");
        assert!(backend.exit_armed());
    }

    #[test]
    fn tui_esc_clears_input_and_never_quits() {
        // Esc:不退出;空闲时清空输入框
        let loader = test_loader();
        let root = loader.lock().unwrap().root().clone();
        let backend = root.inject::<TuiBackend>().unwrap();
        let esc = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        backend.app.lock().unwrap().input = "abc".into();
        assert!(backend.on_key(esc), "Esc 不应退出主循环");
        assert!(backend.app.lock().unwrap().input.is_empty(), "空闲时 Esc 应清空输入框");
        assert!(!backend.quit.load(Ordering::SeqCst), "退出标志不应被设置");
    }

    #[test]
    fn tui_history_up_down_cycles_with_draft() {
        // ↑↓ 历史:↑ 回到已发送,↓ 回到未发送草稿;浏览中编辑恢复草稿
        let loader = test_loader();
        let root = loader.lock().unwrap().root().clone();
        let backend = root.inject::<TuiBackend>().unwrap();
        let key = |code| crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        // 发送两条消息
        for msg in ["第一条", "第二条"] {
            backend.app.lock().unwrap().input = msg.into();
            assert!(backend.on_key(key(crossterm::event::KeyCode::Enter)));
        }
        assert_eq!(backend.sent.lock().unwrap().len(), 2, "应记录 2 条发送历史");
        // 模拟未发送草稿
        backend.app.lock().unwrap().input = "草稿".into();
        // ↑:第二条 → 第一条 → 到顶不动
        backend.on_key(key(crossterm::event::KeyCode::Up));
        assert_eq!(backend.app.lock().unwrap().input, "第二条");
        backend.on_key(key(crossterm::event::KeyCode::Up));
        assert_eq!(backend.app.lock().unwrap().input, "第一条");
        backend.on_key(key(crossterm::event::KeyCode::Up));
        assert_eq!(backend.app.lock().unwrap().input, "第一条", "到顶后不再变化");
        // ↓:第二条 → 草稿
        backend.on_key(key(crossterm::event::KeyCode::Down));
        assert_eq!(backend.app.lock().unwrap().input, "第二条");
        backend.on_key(key(crossterm::event::KeyCode::Down));
        assert_eq!(backend.app.lock().unwrap().input, "草稿", "↓ 回到底应恢复草稿");
        // 浏览中开始编辑 → 恢复草稿并继续输入
        backend.on_key(key(crossterm::event::KeyCode::Up)); // 显示第二条,草稿暂存
        backend.on_key(key(crossterm::event::KeyCode::Char('!')));
        assert_eq!(backend.app.lock().unwrap().input, "草稿!", "编辑应回到草稿并追加字符");
    }
}
