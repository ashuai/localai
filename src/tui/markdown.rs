//! 轻量 Markdown → 样式化 Spans(不引入重型依赖,契合"小上下文、零成本"的项目基调)。
//!
//! 覆盖常见语法:标题(`#`~`###`)、粗体(`**x**`)、斜体(`*x*` / `_x_`)、
//! 行内代码(`` `x` ``)、代码块(```` ``` ```` 围栏,内容原样保留)、无序/有序列表、
//! 引用(`>`)、分隔线(`---`)、链接(`[文本](url)`)、反斜杠转义(`\*` `\`` `\\`)。
//!
//! 产物是逐行的 [`Line`],由消息面板的 `Paragraph` 自动换行/滚动。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

// ---- 样式表(深色终端友好) ----

/// 代码(行内 + 代码块):solarized yellow
fn code() -> Style {
    Style::default().fg(Color::Rgb(0xE6, 0xDB, 0x74))
}
/// 围栏 / 标题井号 / 分隔线:灰
fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}
fn italic() -> Style {
    Style::default().add_modifier(Modifier::ITALIC)
}
fn link() -> Style {
    Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)
}
/// 列表标记:亮青加粗
fn bullet() -> Style {
    Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)
}
/// 标题级别 → 颜色
fn header_color(level: usize) -> Color {
    match level {
        1 => Color::LightCyan,
        2 => Color::LightBlue,
        _ => Color::LightMagenta,
    }
}

/// 渲染整段回复 → 带样式的行。
pub fn render(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let line = raw.trim_end();
        let trimmed = line.trim_start();

        // ---- 代码块围栏:``` [lang] ----
        if let Some(rest) = trimmed.strip_prefix("```") {
            let lang = rest.trim();
            let mut spans = vec![Span::styled("```", dim())];
            if !lang.is_empty() {
                spans.push(Span::styled(format!(" {lang}"), dim()));
            }
            out.push(Line::from(spans));
            in_code = !in_code;
            continue;
        }
        // ---- 代码块内容:原样保留,统一代码色 ----
        if in_code {
            out.push(Line::from(vec![Span::styled(line.to_string(), code())]));
            continue;
        }
        // ---- 标题 ----
        if let Some((level, content)) = header_of(line) {
            let mut spans = vec![Span::styled(format!("{} ", "#".repeat(level)), dim())];
            for sp in inline_spans(content) {
                spans.push(sp.patch_style(bold()));
            }
            out.push(Line::from(spans).style(header_color(level)));
            continue;
        }
        // ---- 引用 ----
        if let Some(content) = line.strip_prefix("> ") {
            let mut spans = vec![Span::styled("▍", Style::default().fg(Color::Gray))];
            for sp in inline_spans(content) {
                spans.push(sp.patch_style(italic()));
            }
            out.push(Line::from(spans).style(Style::default().fg(Color::Gray)));
            continue;
        }
        // ---- 列表(- / * / + / 1. ) ----
        if let Some((mark, content)) = list_of(line) {
            let mut spans = vec![Span::styled(mark, bullet())];
            spans.extend(inline_spans(content));
            out.push(Line::from(spans));
            continue;
        }
        // ---- 分隔线 ----
        if is_hr(line) {
            out.push(Line::from(vec![Span::styled("─────", dim())]));
            continue;
        }
        // ---- 普通行(行内格式) ----
        out.push(Line::from(inline_spans(line)));
    }
    out
}

/// `^#{1,6}\s+` → (级别, 内容);否则 `None`。
fn header_of(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start();
    let level = t.bytes().take_while(|&b| b == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &t[level..];
    if rest.is_empty() {
        return Some((level, ""));
    }
    if !rest.starts_with(' ') {
        return None;
    }
    Some((level, rest.trim_start()))
}

/// 列表前缀 → (标记文本, 内容)。支持 `- ` `* ` `+ ` 与 `1. `。
fn list_of(line: &str) -> Option<(String, &str)> {
    let t = line.trim_start();
    for c in ['-', '*', '+'] {
        if let Some(rest) = t.strip_prefix(c) {
            if let Some(content) = rest.strip_prefix(' ') {
                return Some((format!("{c} "), content.trim_start()));
            }
        }
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        if let Some(rest) = t[digits..].strip_prefix(". ") {
            return Some((format!("{}. ", &t[..digits]), rest.trim_start()));
        }
    }
    None
}

/// `---` / `***` / `___`(3 个及以上,可夹空格)视为分隔线。
fn is_hr(line: &str) -> bool {
    let t = line.trim();
    let b = t.as_bytes();
    if b.len() < 3 {
        return false;
    }
    let c = b[0];
    if c != b'-' && c != b'*' && c != b'_' {
        return false;
    }
    b.iter().all(|&x| x == c || x == b' ' || x == b'\t')
}

/// 行内格式解析:`**粗体**`、`*斜体*`、`_斜体_`、`` `代码` ``、`[文本](链接)`、`\` 转义。
fn inline_spans(s: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        // 转义:下一个字符按字面输出
        if let Some(st) = rest.strip_prefix('\\') {
            let (c, tail) = split_first_char(st);
            plain.push(c);
            rest = tail;
            continue;
        }
        // **粗体**(优先于单 * 斜体)
        if rest.starts_with("**") {
            if let Some(end) = find_unmatched(&rest[2..], "**") {
                flush(&mut plain, &mut spans);
                spans.push(Span::styled(rest[2..2 + end].to_string(), bold()));
                rest = &rest[2 + end + 2..];
                continue;
            }
        }
        // *斜体* / _斜体_
        if rest.starts_with('*') || rest.starts_with('_') {
            let d = if rest.starts_with('*') { "*" } else { "_" };
            if let Some(end) = find_unmatched(&rest[1..], d) {
                if end > 0 {
                    flush(&mut plain, &mut spans);
                    spans.push(Span::styled(rest[1..1 + end].to_string(), italic()));
                    rest = &rest[1 + end + 1..];
                    continue;
                }
            }
        }
        // `行内代码`
        if rest.starts_with('`') {
            if let Some(end) = find_unmatched(&rest[1..], "`") {
                flush(&mut plain, &mut spans);
                spans.push(Span::styled(rest[1..1 + end].to_string(), code()));
                rest = &rest[1 + end + 1..];
                continue;
            }
        }
        // [文本](链接):文本下划线青色,链接不显示
        if rest.starts_with('[') {
            if let Some(close) = find_unmatched(&rest[1..], "]") {
                let label = &rest[1..1 + close];
                if let Some(open) = rest[1 + close + 1..].strip_prefix('(') {
                    if let Some(close2) = find_unmatched(open, ")") {
                        flush(&mut plain, &mut spans);
                        spans.push(Span::styled(label.to_string(), link()));
                        rest = &open[close2 + 1..];
                        continue;
                    }
                }
            }
        }
        // 普通字符累积
        let (c, tail) = split_first_char(rest);
        plain.push(c);
        rest = tail;
    }
    flush(&mut plain, &mut spans);
    spans
}

/// 把累积的普通文本刷成 raw span。
fn flush(plain: &mut String, spans: &mut Vec<Span<'static>>) {
    if !plain.is_empty() {
        spans.push(Span::raw(std::mem::take(plain)));
    }
}

/// 切出第一个字符(按 UTF-8 边界)。
fn split_first_char(s: &str) -> (char, &str) {
    let mut it = s.chars();
    (it.next().unwrap(), it.as_str())
}

/// 在 `s` 中找下一个**未转义**的 `delim` 的字节偏移。
fn find_unmatched(s: &str, delim: &str) -> Option<usize> {
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if c == '\\' {
            it.next(); // 跳过被转义字符
            continue;
        }
        if s[i..].starts_with(delim) {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_bold_italic_code_and_escape() {
        let lines = render("**粗体** 与 `代码` 与 *斜体* 与 \\*字面\\*");
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[0].content, "粗体");
        assert!(spans[1].content.contains("与"));
        assert_eq!(spans[2].content, "代码");
        assert!(spans[2].style.fg.is_some(), "行内代码应有前景色");
        assert!(spans[3].content.contains("与"));
        assert!(spans[4].style.add_modifier.contains(Modifier::ITALIC));
        assert!(spans[5].content.contains("*字面*"), "转义星号应按字面输出");
    }

    #[test]
    fn headers_and_lists_and_quote() {
        let lines = render("# 标题\n## 小节\n- 项一\n1. 有序\n> 引用");
        assert_eq!(lines.len(), 5);
        assert!(lines[0].spans[0].content.contains('#'));
        assert!(lines[0].spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert!(lines[1].spans[0].content.contains("##"));
        assert_eq!(lines[2].spans[0].content, "- ", "列表标记");
        assert_eq!(lines[3].spans[0].content, "1. ", "有序列表标记");
        assert_eq!(lines[4].spans[0].content, "▍", "引用标记");
    }

    #[test]
    fn code_block_keeps_content_and_restores() {
        let lines = render("```rust\nfn main() { let s = \"x\"; }\n```\n后文");
        assert_eq!(lines.len(), 4);
        assert!(lines[0].spans[0].content.contains("```"));
        assert!(lines[1].spans[0].content.contains("fn main()"));
        assert!(lines[1].spans[0].style.fg.is_some(), "代码块内容应有颜色");
        assert!(!lines[2].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(lines[3].spans[0].content.contains("后文"), "围栏结束后恢复普通解析");
    }

    #[test]
    fn hr_and_empty_lines() {
        let lines = render("---\n\n正文");
        assert_eq!(lines.len(), 3);
        assert!(lines[0].spans[0].content.contains('─'));
        assert!(lines[1].spans.is_empty());
        assert!(lines[2].spans[0].content.contains("正文"));
    }
}
