//! 面向终端的 Markdown 语义渲染。
//!
//! Markdown 只在这一层解析，TUI、经典终端和非 TTY 输出共享同一语义结果。

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkdownLineKind {
    Body,
    Heading,
    Code,
    DiffAdd,
    DiffRemove,
    DiffContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkdownLine {
    pub(crate) kind: MarkdownLineKind,
    pub(crate) text: String,
}

#[derive(Default)]
struct RenderState {
    output: Vec<MarkdownLine>,
    current: String,
    kind: Option<MarkdownLineKind>,
    quote_depth: usize,
    list_stack: Vec<Option<u64>>,
    pending_link: Option<String>,
    in_table: bool,
    in_code: bool,
    in_diff: bool,
    table_cells: Vec<String>,
}

impl RenderState {
    fn set_kind(&mut self, kind: MarkdownLineKind) {
        self.kind = Some(kind);
    }

    fn push_text(&mut self, value: &str) {
        if self.current.is_empty() && self.quote_depth > 0 {
            self.current.push_str(&"│ ".repeat(self.quote_depth));
        }
        self.current.push_str(value);
    }

    fn flush(&mut self) {
        if self.current.is_empty() {
            return;
        }
        self.output.push(MarkdownLine {
            kind: self.kind.unwrap_or(MarkdownLineKind::Body),
            text: self.current.trim_end().to_owned(),
        });
        self.current.clear();
        self.kind = None;
    }

    fn blank(&mut self) {
        self.flush();
        if self.output.last().is_some_and(|line| !line.text.is_empty()) {
            self.output.push(MarkdownLine {
                kind: MarkdownLineKind::Body,
                text: String::new(),
            });
        }
    }

    fn finish(mut self) -> Vec<MarkdownLine> {
        self.flush();
        while self.output.last().is_some_and(|line| line.text.is_empty()) {
            self.output.pop();
        }
        self.output
    }
}

pub(crate) fn terminal_markdown(source: &str) -> Vec<MarkdownLine> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut state = RenderState::default();

    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                state.blank();
                state.set_kind(MarkdownLineKind::Heading);
            }
            Event::End(TagEnd::Heading(_)) => state.blank(),
            Event::Start(Tag::Paragraph) => state.flush(),
            Event::End(TagEnd::Paragraph) => state.blank(),
            Event::Start(Tag::BlockQuote(_)) => state.quote_depth += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                state.flush();
                state.quote_depth = state.quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::List(start)) => state.list_stack.push(start),
            Event::End(TagEnd::List(_)) => {
                state.flush();
                state.list_stack.pop();
                if state.list_stack.is_empty() {
                    state.blank();
                }
            }
            Event::Start(Tag::Item) => {
                state.flush();
                let indent = "  ".repeat(state.list_stack.len().saturating_sub(1));
                let marker = match state.list_stack.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".into(),
                };
                state.push_text(&format!("{indent}{marker}"));
            }
            Event::End(TagEnd::Item) => state.flush(),
            Event::Start(Tag::CodeBlock(kind)) => {
                state.blank();
                state.in_code = true;
                state.in_diff = matches!(&kind, CodeBlockKind::Fenced(language) if language.eq_ignore_ascii_case("diff") || language.eq_ignore_ascii_case("patch"));
                state.set_kind(MarkdownLineKind::Code);
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    state.push_text(&format!("  [{language}]"));
                    state.flush();
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                state.blank();
                state.in_code = false;
                state.in_diff = false;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                state.pending_link = Some(dest_url.into_string());
            }
            Event::End(TagEnd::Link) => {
                if let Some(url) = state.pending_link.take()
                    && !url.is_empty()
                    && !state.current.ends_with(&url)
                {
                    state.push_text(&format!(" ‹{url}›"));
                }
            }
            Event::Start(Tag::Table(_)) => {
                state.blank();
                state.in_table = true;
            }
            Event::End(TagEnd::Table) => {
                state.in_table = false;
                state.blank();
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => state.table_cells.clear(),
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                state.push_text(&format!("│ {} │", state.table_cells.join(" │ ")));
                state.flush();
            }
            Event::Start(Tag::TableCell) => state.current.clear(),
            Event::End(TagEnd::TableCell) => {
                state.table_cells.push(state.current.trim().to_owned());
                state.current.clear();
            }
            Event::Text(text) => {
                if state.in_code {
                    for (index, line) in text.lines().enumerate() {
                        if index > 0 {
                            state.flush();
                        }
                        state.set_kind(if state.in_diff {
                            if line.starts_with('+') && !line.starts_with("+++") {
                                MarkdownLineKind::DiffAdd
                            } else if line.starts_with('-') && !line.starts_with("---") {
                                MarkdownLineKind::DiffRemove
                            } else {
                                MarkdownLineKind::DiffContext
                            }
                        } else {
                            MarkdownLineKind::Code
                        });
                        state.push_text("  ");
                        state.push_text(line);
                    }
                } else {
                    state.push_text(&text);
                }
            }
            Event::Code(code) => state.push_text(&code),
            Event::SoftBreak => state.push_text(" "),
            Event::HardBreak => state.flush(),
            Event::Rule => {
                state.blank();
                state.push_text("────────────────────────");
                state.blank();
            }
            Event::TaskListMarker(checked) => {
                state.push_text(if checked { "[✓] " } else { "[ ] " });
            }
            Event::FootnoteReference(reference) => state.push_text(&format!("[{reference}]")),
            Event::Html(_) | Event::InlineHtml(_) => {}
            Event::InlineMath(value) | Event::DisplayMath(value) => state.push_text(&value),
            _ => {}
        }
    }

    state.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 标题和强调符不会原样显示() {
        let lines = terminal_markdown("## 总结\n\n这是 **重要** 的 `结果`");
        assert_eq!(lines[0].kind, MarkdownLineKind::Heading);
        assert_eq!(lines[0].text, "总结");
        assert_eq!(lines[2].text, "这是 重要 的 结果");
    }

    #[test]
    fn 列表链接和代码块适合终端阅读() {
        let lines =
            terminal_markdown("- [来源](https://example.com)\n```rust\nlet value = 1;\n```");
        assert_eq!(lines[0].text, "• 来源 ‹https://example.com›");
        assert!(
            lines.iter().any(|line| {
                line.kind == MarkdownLineKind::Code && line.text.contains("let value = 1;")
            }),
            "{lines:?}"
        );
    }

    #[test]
    fn 表格转换为终端行() {
        let lines = terminal_markdown("| 名称 | 状态 |\n| --- | --- |\n| 搜索 | 完成 |");
        assert!(
            lines.iter().any(|line| line.text == "│ 名称 │ 状态 │"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.text == "│ 搜索 │ 完成 │"),
            "{lines:?}"
        );
    }

    #[test]
    fn diff_代码块区分增删行() {
        let lines = terminal_markdown("```diff\n-old\n+new\n same\n```");
        assert!(
            lines
                .iter()
                .any(|line| line.kind == MarkdownLineKind::DiffRemove)
        );
        assert!(
            lines
                .iter()
                .any(|line| line.kind == MarkdownLineKind::DiffAdd)
        );
    }

    #[test]
    fn 普通乘号和标识符下划线保持不变() {
        let lines = terminal_markdown("价格 2 * 3，读取 max_bytes");
        assert_eq!(lines[0].text, "价格 2 * 3，读取 max_bytes");
    }
}
