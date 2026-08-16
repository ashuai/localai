//! TUI 应用:消息面板 + 输入行 + 状态行。
//!
//! 与 cordis 的接缝:
//! - 普通输入 → `session/input` 事件(chat 插件订阅);
//! - `session/reply` / `session/status` → 通过 mpsc 回灌渲染;
//! - `/xxx` 命令:先走插件注册的命令表(ctx.run_command),未命中再走内置命令。

use crate::cordis::context::Context;
use crate::cordis::loader::Loader;
use crate::events::{SessionReply, SessionStatus};
use crate::llm::LlmService;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub enum UiEvent {
    Reply(String),
    Status(String),
}

pub fn run(loader: Arc<Mutex<Loader>>) -> anyhow::Result<()> {
    let root = {
        let l = loader.lock().unwrap();
        l.root().clone()
    };
    let (tx, rx) = mpsc::channel::<UiEvent>();
    {
        let tx1 = tx.clone();
        root.on(move |ev: &SessionReply| {
            let _ = tx1.send(UiEvent::Reply(ev.text.clone()));
        });
    }
    {
        let tx2 = tx;
        root.on(move |ev: &SessionStatus| {
            let _ = tx2.send(UiEvent::Status(ev.text.clone()));
        });
    }
    let mut app = App::new(root, loader, rx);
    app.run_loop()
}

pub struct App {
    root: Context,
    loader: Arc<Mutex<Loader>>,
    rx: Receiver<UiEvent>,
    lines: Vec<Line<'static>>,
    input: String,
    scroll: u16,
}

impl App {
    fn new(root: Context, loader: Arc<Mutex<Loader>>, rx: Receiver<UiEvent>) -> Self {
        Self {
            root,
            loader,
            rx,
            lines: Vec::new(),
            input: String::new(),
            scroll: 0,
        }
    }

    fn run_loop(&mut self) -> anyhow::Result<()> {
        let mut terminal = ratatui::init();
        let res = self.loop_(&mut terminal);
        ratatui::restore();
        res
    }

    fn loop_(&mut self, terminal: &mut ratatui::DefaultTerminal) -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        if !self.on_key(k) {
                            return Ok(());
                        }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            while let Ok(ev) = self.rx.try_recv() {
                self.apply(ev);
            }
        }
    }

    fn on_key(&mut self, k: crossterm::event::KeyEvent) -> bool {
        match k.code {
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => false,
            KeyCode::Esc => false,
            KeyCode::Enter => {
                self.submit();
                true
            }
            KeyCode::Char(ch) => {
                self.input.push(ch);
                true
            }
            KeyCode::Backspace => {
                self.input.pop();
                true
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(5);
                true
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(5);
                true
            }
            _ => true,
        }
    }

    fn submit(&mut self) {
        let text = std::mem::take(&mut self.input);
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.scroll = 0;
        if let Some(cmd) = text.strip_prefix('/') {
            self.handle_command(cmd);
            return;
        }
        self.push_line(
            vec![
                Span::styled("你 ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(text.clone()),
            ],
            None,
        );
        self.root.emit(crate::events::SessionInput { text });
    }

    fn handle_command(&mut self, cmd: &str) {
        let (name, args) = cmd
            .split_once(' ')
            .map(|(n, r)| (n.to_string(), r.trim().to_string()))
            .unwrap_or((cmd.to_string(), String::new()));
        match name.as_str() {
            "help" => {
                let mut lines: Vec<String> = vec![
                    "内置命令: /help /plugins /load <插件> /unload <插件> /model [模型] /clear /quit"
                        .to_string(),
                ];
                let names = self.root.command_names();
                if !names.is_empty() {
                    lines.push(format!("插件命令: /{}", names.join(" /")));
                }
                for l in lines {
                    self.push_status(l);
                }
            }
            "plugins" => {
                let l = self.loader.lock().unwrap();
                let mut s = String::from("插件: ");
                for p in l.list() {
                    s.push_str(&format!("{}{} ", p.name, if p.loaded { "✓" } else { "✗" }));
                }
                drop(l);
                self.push_status(s);
            }
            "load" | "unload" => {
                if args.is_empty() {
                    self.push_status(format!("用法: /{name} <插件名>(/plugins 查看)"));
                    return;
                }
                let res = {
                    let mut l = self.loader.lock().unwrap();
                    if name == "load" {
                        l.load(&args, serde_yaml::Value::Null)
                    } else {
                        l.unload(&args)
                    }
                };
                match res {
                    Ok(()) => self.push_status(format!("/{name} {args} OK")),
                    Err(e) => self.push_status(format!("/{name} {args} 失败: {e:#}")),
                }
            }
            "model" => {
                if args.is_empty() {
                    if let Some(llm) = self.root.inject::<LlmService>() {
                        self.push_status(format!("当前模型: {}", llm.client.model()));
                    }
                } else if let Some(llm) = self.root.inject::<LlmService>() {
                    llm.client.set_model(args.clone());
                    self.push_status(format!("模型已切换: {args}"));
                }
            }
            "clear" => {
                self.lines.clear();
                self.scroll = 0;
            }
            "quit" => std::process::exit(0),
            _ => {
                // 先问插件命令表,再报未知
                match self.root.run_command(cmd) {
                    Some(out) => {
                        for l in out.lines() {
                            self.push_status(l.to_string());
                        }
                    }
                    None => self.push_status(format!("未知命令: /{name}(/help 查看)")),
                }
            }
        }
    }

    fn apply(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Reply(text) => {
                self.push_line(
                    vec![
                        Span::styled("AI ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                        Span::raw(text),
                    ],
                    Some(Color::Green),
                );
            }
            UiEvent::Status(text) => self.push_status(text),
        }
    }

    fn push_status(&mut self, text: String) {
        self.push_line(
            vec![Span::styled(text, Style::default().fg(Color::DarkGray))],
            None,
        );
    }

    fn push_line(&mut self, spans: Vec<Span<'static>>, _fg: Option<Color>) {
        self.lines.push(Line::from(spans));
        // 有界渲染:防止长时间运行内存膨胀
        if self.lines.len() > 4000 {
            let drop_n = self.lines.len() - 2000;
            self.lines.drain(0..drop_n);
        }
    }

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        // 消息面板:从底部向上翻页(PageUp/Down 微调)
        let max_lines = (chunks[0].height as usize).saturating_sub(2).max(1);
        let total = self.lines.len();
        let end = total.saturating_sub(self.scroll as usize);
        let start = end.saturating_sub(max_lines);
        let content: Vec<Line> = self.lines.iter().skip(start).cloned().collect();
        let msg = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" localai · cordis 模式 · 小上下文·频繁微调用 "),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(msg, chunks[0]);

        // 输入行
        let input = Paragraph::new(self.input.as_str())
            .block(Block::default().borders(Borders::ALL).title(" 输入(Enter 发送,/ 命令;PageUp/Down 翻页) "));
        f.render_widget(input, chunks[1]);

        // 状态行:插件数 + 模型
        let (plugin_count, model) = {
            let l = self.loader.lock().unwrap();
            let loaded = l.list().iter().filter(|p| p.loaded).count();
            let model = self
                .root
                .inject::<LlmService>()
                .map(|s| s.client.model())
                .unwrap_or_else(|| "?".into());
            (loaded, model)
        };
        let status = Line::from(vec![
            Span::styled(
                format!(" 插件×{plugin_count} | 模型 {model} | 192.168.0.5:9870 (oMLX) | Ctrl-C 退出 "),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        f.render_widget(Paragraph::new(status), chunks[2]);
    }
}
