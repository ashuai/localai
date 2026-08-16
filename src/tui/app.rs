//! 纯渲染层:App 只持有渲染状态(lines/input/scroll/status)并绘制。
//!
//! 交互编排(键盘 → 事件/命令、事件 → 渲染)由 `tui` 插件(TuiBackend)负责,
//! 见 `src/plugins/tui.rs` 与本地论证文档 `localai-docs/tui-plugin.md`。

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub struct App {
    pub lines: Vec<Line<'static>>,
    pub input: String,
    pub scroll: u16,
    /// 底部状态行(TuiBackend 每轮刷新)
    pub status: String,
}

impl App {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            input: String::new(),
            scroll: 0,
            status: String::new(),
        }
    }

    pub fn push_line(&mut self, spans: Vec<Span<'static>>) {
        self.lines.push(Line::from(spans));
        // 有界渲染:防止长时间运行内存膨胀
        if self.lines.len() > 4000 {
            let drop_n = self.lines.len() - 2000;
            self.lines.drain(0..drop_n);
        }
    }

    pub fn push_status(&mut self, text: String) {
        self.push_line(vec![Span::styled(text, Style::default().fg(Color::DarkGray))]);
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll = 0;
    }

    pub fn draw(&mut self, f: &mut Frame) {
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
        let input = Paragraph::new(self.input.as_str()).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" 输入(Enter 发送;/ 命令;↑↓ 历史;Esc 清空/中断;PgUp/Dn 翻页) "),
        );
        f.render_widget(input, chunks[1]);

        // 状态行
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                self.status.clone(),
                Style::default().fg(Color::DarkGray),
            )])),
            chunks[2],
        );
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
