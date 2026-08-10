//! 轻量交互行编辑器。
//!
//! 不持久化历史，避免把用户可能输入的秘密写入磁盘。

use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use crossterm::{
    cursor::MoveToColumn,
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, read,
    },
    execute, queue,
    style::Print,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use unicode_width::UnicodeWidthStr;

const MAX_INPUT_CHARS: usize = 262_144;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadResult {
    Line(String),
    Interrupted,
    Eof,
}

#[derive(Debug, Default)]
pub(crate) struct InputEditor {
    history: Vec<String>,
    workspace_paths: Vec<String>,
}

#[derive(Debug, Default)]
struct LineState {
    chars: Vec<char>,
    cursor: usize,
    history_index: Option<usize>,
    draft: Vec<char>,
    kill_buffer: Vec<char>,
    undo: Vec<(Vec<char>, usize)>,
    command_selection: usize,
}

const COMMANDS: [(&str, &str, bool); 21] = [
    ("/help", "显示帮助", false),
    ("/new", "开始新会话", false),
    ("/clear", "开始新会话", false),
    ("/resume", "恢复历史会话", true),
    ("/model", "选择当前模型", true),
    ("/mcp", "查看 MCP Server 与外部工具", false),
    ("/plugins", "查看声明式插件", false),
    ("/instructions", "查看自定义指令加载情况", false),
    ("/skills", "查看可用技能与加载策略", false),
    ("/agent", "查看 Agent 档案与子代理", false),
    ("/plan", "生成或管理计划", true),
    ("/turns", "设置最大循环次数", true),
    ("/approval", "管理审批规则", true),
    ("/transcript", "查看完整会话", false),
    ("/copy", "复制最后回答", false),
    ("/export", "导出会话 Markdown", false),
    ("/rename", "重命名当前会话", true),
    ("/compact", "压缩会话上下文", false),
    ("/exit", "退出 XDUDU", false),
    ("/quit", "退出 XDUDU", false),
    ("/q", "退出 XDUDU", false),
];

impl LineState {
    fn insert_paste(&mut self, value: &str) -> bool {
        self.snapshot();
        let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
        let remaining = MAX_INPUT_CHARS.saturating_sub(self.chars.len());
        let mut inserted: Vec<char> = normalized.chars().take(remaining).collect();
        let inserted_len = inserted.len();
        let truncated = normalized.chars().count() > inserted_len;
        self.chars
            .splice(self.cursor..self.cursor, inserted.drain(..));
        self.cursor += inserted_len;
        self.history_index = None;
        truncated
    }

    fn handle_key(&mut self, key: KeyEvent, history: &[String]) -> Option<ReadResult> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        match key.code {
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert('\n');
                None
            }
            KeyCode::Enter if self.chars.get(self.cursor.wrapping_sub(1)) == Some(&'\\') => {
                self.snapshot();
                self.cursor -= 1;
                self.chars[self.cursor] = '\n';
                self.cursor += 1;
                None
            }
            KeyCode::Enter => {
                let value = self.chars.iter().collect::<String>();
                if let Some((command, _, needs_argument)) = self.selected_command() {
                    if value != command && !value.contains(char::is_whitespace) {
                        self.chars = command.chars().collect();
                        if needs_argument {
                            self.chars.push(' ');
                            self.cursor = self.chars.len();
                            return None;
                        }
                        return Some(ReadResult::Line(command.into()));
                    }
                }
                Some(ReadResult::Line(value))
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ReadResult::Interrupted)
            }
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.chars.is_empty() =>
            {
                Some(ReadResult::Eof)
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.line_start();
                None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.line_end();
                None
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert('\n');
                None
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.snapshot();
                let end = self.line_end();
                self.kill_buffer = self.chars.drain(self.cursor..end).collect();
                None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.snapshot();
                let start = self.line_start();
                self.kill_buffer = self.chars.drain(start..self.cursor).collect();
                self.cursor = start;
                None
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.snapshot();
                let original = self.cursor;
                while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
                    self.cursor -= 1;
                }
                while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
                    self.cursor -= 1;
                }
                self.kill_buffer = self.chars.drain(self.cursor..original).collect();
                None
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.kill_buffer.is_empty() {
                    self.snapshot();
                    for character in self.kill_buffer.clone() {
                        self.chars.insert(self.cursor, character);
                        self.cursor += 1;
                    }
                }
                None
            }
            KeyCode::Char('_') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.restore_undo();
                None
            }
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.restore_undo();
                None
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let query = self.chars.iter().collect::<String>();
                if let Some(value) = history.iter().rev().find(|value| {
                    query.is_empty()
                        || value
                            .to_ascii_lowercase()
                            .contains(&query.to_ascii_lowercase())
                }) {
                    self.chars = value.chars().collect();
                    self.cursor = self.chars.len();
                }
                None
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.chars.is_empty() {
                    Some(ReadResult::Line("/transcript".into()))
                } else {
                    None
                }
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
                    self.cursor -= 1;
                }
                while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
                    self.cursor -= 1;
                }
                None
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                while self.cursor < self.chars.len() && !self.chars[self.cursor].is_whitespace() {
                    self.cursor += 1;
                }
                while self.cursor < self.chars.len() && self.chars[self.cursor].is_whitespace() {
                    self.cursor += 1;
                }
                None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert(character);
                self.history_index = None;
                self.command_selection = 0;
                None
            }
            KeyCode::Tab => {
                if let Some((command, _, needs_argument)) = self.selected_command() {
                    self.chars = command.chars().collect();
                    if needs_argument {
                        self.chars.push(' ');
                    }
                    self.cursor = self.chars.len();
                }
                None
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.snapshot();
                self.cursor -= 1;
                self.chars.remove(self.cursor);
                self.history_index = None;
                None
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
                self.snapshot();
                self.chars.remove(self.cursor);
                self.history_index = None;
                None
            }
            KeyCode::Left if self.cursor > 0 => {
                self.cursor -= 1;
                None
            }
            KeyCode::Right if self.cursor < self.chars.len() => {
                self.cursor += 1;
                None
            }
            KeyCode::Home => {
                self.cursor = self.line_start();
                None
            }
            KeyCode::End => {
                self.cursor = self.line_end();
                None
            }
            KeyCode::Up if !self.command_matches().is_empty() => {
                let count = self.command_matches().len();
                self.command_selection = (self.command_selection + count - 1) % count;
                None
            }
            KeyCode::Down if !self.command_matches().is_empty() => {
                let count = self.command_matches().len();
                self.command_selection = (self.command_selection + 1) % count;
                None
            }
            KeyCode::Up if !history.is_empty() => {
                let index = match self.history_index {
                    None => {
                        self.draft = self.chars.clone();
                        history.len() - 1
                    }
                    Some(index) => index.saturating_sub(1),
                };
                self.load_history(index, history);
                None
            }
            KeyCode::Down => {
                match self.history_index {
                    Some(index) if index + 1 < history.len() => {
                        self.load_history(index + 1, history);
                    }
                    Some(_) => {
                        self.chars.clone_from(&self.draft);
                        self.cursor = self.chars.len();
                        self.history_index = None;
                    }
                    None => {}
                }
                None
            }
            _ => None,
        }
    }

    fn insert(&mut self, character: char) {
        self.snapshot();
        self.chars.insert(self.cursor, character);
        self.cursor += 1;
        self.history_index = None;
    }

    fn snapshot(&mut self) {
        if self
            .undo
            .last()
            .is_none_or(|(chars, cursor)| chars != &self.chars || *cursor != self.cursor)
        {
            self.undo.push((self.chars.clone(), self.cursor));
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
        }
    }

    fn restore_undo(&mut self) {
        if let Some((chars, cursor)) = self.undo.pop() {
            self.chars = chars;
            self.cursor = cursor;
            self.history_index = None;
        }
    }

    fn line_start(&self) -> usize {
        self.chars[..self.cursor]
            .iter()
            .rposition(|character| *character == '\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.chars[self.cursor..]
            .iter()
            .position(|character| *character == '\n')
            .map_or(self.chars.len(), |index| self.cursor + index)
    }

    fn command_matches(&self) -> Vec<(&'static str, &'static str, bool)> {
        let value = self.chars.iter().collect::<String>();
        if !value.starts_with('/') || value.contains(char::is_whitespace) {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .copied()
            .filter(|(command, _, _)| command.starts_with(&value))
            .take(6)
            .collect()
    }

    fn selected_command(&self) -> Option<(&'static str, &'static str, bool)> {
        let matches = self.command_matches();
        matches
            .get(self.command_selection.min(matches.len().saturating_sub(1)))
            .copied()
    }

    fn complete_path(&mut self, paths: &[String]) -> bool {
        let value = self.chars.iter().collect::<String>();
        let start = value[..value
            .char_indices()
            .nth(self.cursor)
            .map_or(value.len(), |(index, _)| index)]
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + 1);
        let token = &value[start..];
        if !token.starts_with('@') {
            return false;
        }
        let query = token.trim_start_matches('@').to_ascii_lowercase();
        let Some(path) = paths
            .iter()
            .find(|path| path.to_ascii_lowercase().contains(&query))
        else {
            return false;
        };
        self.snapshot();
        let prefix = value[..start].to_owned();
        let completed = format!("{prefix}@{path} ");
        self.chars = completed.chars().collect();
        self.cursor = self.chars.len();
        true
    }

    fn load_history(&mut self, index: usize, history: &[String]) {
        self.chars = history[index].chars().collect();
        self.cursor = self.chars.len();
        self.history_index = Some(index);
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// 启用 bracketed paste：粘贴内容以单个 `Event::Paste` 到达，不会把
/// 其中的换行当作 Enter 提交。
struct BracketedPasteGuard;

impl BracketedPasteGuard {
    fn enter() -> io::Result<Self> {
        execute!(io::stdout(), EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for BracketedPasteGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
    }
}

impl InputEditor {
    pub(crate) fn with_workspace(cwd: &Path) -> Self {
        let mut workspace_paths = Vec::new();
        collect_workspace_paths(cwd, cwd, &mut workspace_paths, 2_000);
        workspace_paths.sort();
        Self {
            history: Vec::new(),
            workspace_paths,
        }
    }

    pub(crate) fn read_line(&mut self, prompt: &str) -> io::Result<ReadResult> {
        if !io::stdin().is_terminal() {
            return self.read_plain_line(prompt);
        }

        let _guard = RawModeGuard::enter()?;
        let _paste = BracketedPasteGuard::enter()?;
        let mut stdout = io::stdout();
        let mut state = LineState::default();
        let mut rendered_rows = 1;
        redraw(
            &mut stdout,
            prompt,
            &state,
            &self.workspace_paths,
            &mut rendered_rows,
        )?;

        loop {
            match read()? {
                Event::Key(key) => {
                    if key.code == KeyCode::Tab
                        && state.command_matches().is_empty()
                        && state.complete_path(&self.workspace_paths)
                    {
                        redraw(
                            &mut stdout,
                            prompt,
                            &state,
                            &self.workspace_paths,
                            &mut rendered_rows,
                        )?;
                        continue;
                    }
                    if key.code == KeyCode::Char('g')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        disable_raw_mode()?;
                        let edited =
                            edit_in_external_editor(&state.chars.iter().collect::<String>());
                        enable_raw_mode()?;
                        if let Ok(value) = edited {
                            state.snapshot();
                            state.chars = value.chars().collect();
                            state.cursor = state.chars.len();
                        }
                        redraw(
                            &mut stdout,
                            prompt,
                            &state,
                            &self.workspace_paths,
                            &mut rendered_rows,
                        )?;
                        continue;
                    }
                    if let Some(result) = state.handle_key(key, &self.history) {
                        let (columns, _) = crossterm::terminal::size().unwrap_or((80, 24));
                        let width = usize::from(columns.max(10));
                        let cursor = screen_position(
                            &state.chars,
                            state.cursor,
                            UnicodeWidthStr::width(prompt),
                            width,
                        );
                        queue!(
                            stdout,
                            crossterm::cursor::MoveDown(rendered_rows.saturating_sub(cursor.1)),
                            MoveToColumn(0)
                        )?;
                        stdout.flush()?;
                        if let ReadResult::Line(line) = &result
                            && !line.trim().is_empty()
                            && self.history.last() != Some(line)
                        {
                            self.history.push(line.clone());
                        }
                        return Ok(result);
                    }
                    redraw(
                        &mut stdout,
                        prompt,
                        &state,
                        &self.workspace_paths,
                        &mut rendered_rows,
                    )?;
                }
                Event::Paste(value) => {
                    state.insert_paste(&value);
                    redraw(
                        &mut stdout,
                        prompt,
                        &state,
                        &self.workspace_paths,
                        &mut rendered_rows,
                    )?;
                }
                Event::Resize(_, _) => redraw(
                    &mut stdout,
                    prompt,
                    &state,
                    &self.workspace_paths,
                    &mut rendered_rows,
                )?,
                _ => {}
            }
        }
    }

    fn read_plain_line(&mut self, prompt: &str) -> io::Result<ReadResult> {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(ReadResult::Eof);
        }
        let line = line.trim_end_matches(['\r', '\n']).to_owned();
        if !line.trim().is_empty() && self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        Ok(ReadResult::Line(line))
    }
}

fn edit_in_external_editor(initial: &str) -> io::Result<String> {
    let path = std::env::temp_dir().join(format!("xdudu-prompt-{}.txt", std::process::id()));
    fs::write(&path, initial)?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    let status = std::process::Command::new(editor).arg(&path).status()?;
    let result = if status.success() {
        fs::read_to_string(&path)
    } else {
        Err(io::Error::other("外部编辑器未正常退出"))
    };
    let _ = fs::remove_file(path);
    result
}

fn redraw(
    writer: &mut impl Write,
    prompt: &str,
    state: &LineState,
    workspace_paths: &[String],
    rendered_rows: &mut u16,
) -> io::Result<()> {
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let width = usize::from(columns.max(10));
    let max_visible = width
        .saturating_mul(usize::from(rows.saturating_sub(8)).max(3))
        .max(256);
    let (visible, visible_cursor) = visible_input(&state.chars, state.cursor, max_visible);
    let line: String = visible.iter().collect();
    let prompt_width = UnicodeWidthStr::width(prompt);
    let cursor = screen_position(&visible, visible_cursor, prompt_width, width);
    let end = screen_position(&visible, visible.len(), prompt_width, width);
    let suggestions = state.command_matches();
    let path_suggestions = matching_paths(&state.chars, workspace_paths);
    queue!(
        writer,
        crossterm::cursor::MoveUp(rendered_rows.saturating_sub(1)),
        MoveToColumn(0),
        Clear(ClearType::FromCursorDown),
        Print(prompt),
        Print(line.replace('\n', "\r\n"))
    )?;
    for (index, (command, description, _)) in suggestions.iter().enumerate() {
        let marker = if index == state.command_selection {
            "●"
        } else {
            "○"
        };
        queue!(
            writer,
            Print("\r\n"),
            Print(format!("  {marker} {command:<14} {description}"))
        )?;
    }
    for path in &path_suggestions {
        queue!(writer, Print("\r\n"), Print(format!("  ○ @{path}")))?;
    }
    let suggestion_rows = suggestions.len().saturating_add(path_suggestions.len()) as u16;
    queue!(
        writer,
        crossterm::cursor::MoveUp(
            end.1
                .saturating_sub(cursor.1)
                .saturating_add(suggestion_rows)
        ),
        MoveToColumn(cursor.0)
    )?;
    *rendered_rows = end.1.saturating_add(1).saturating_add(suggestion_rows);
    writer.flush()
}

/// 长输入只渲染光标周围的窗口，内部仍保留并发送全部内容。
fn visible_input(chars: &[char], cursor: usize, limit: usize) -> (Vec<char>, usize) {
    if chars.len() <= limit {
        return (chars.to_vec(), cursor);
    }
    let half = limit / 2;
    let start = cursor
        .saturating_sub(half)
        .min(chars.len().saturating_sub(limit));
    let end = start.saturating_add(limit).min(chars.len());
    let mut visible = Vec::with_capacity(limit + 64);
    let prefix = if start > 0 {
        format!("… 已折叠 {start} 个字符 …\n")
    } else {
        String::new()
    };
    visible.extend(prefix.chars());
    let prefix_len = visible.len();
    visible.extend_from_slice(&chars[start..end]);
    if end < chars.len() {
        visible.extend("\n… 后续内容已折叠 …".chars());
    }
    (visible, prefix_len + cursor.saturating_sub(start))
}

fn matching_paths(chars: &[char], paths: &[String]) -> Vec<String> {
    let value = chars.iter().collect::<String>();
    let Some(token) = value
        .split_whitespace()
        .last()
        .filter(|token| token.starts_with('@'))
    else {
        return Vec::new();
    };
    let query = token.trim_start_matches('@').to_ascii_lowercase();
    paths
        .iter()
        .filter(|path| path.to_ascii_lowercase().contains(&query))
        .take(5)
        .cloned()
        .collect()
}

fn collect_workspace_paths(root: &Path, current: &Path, output: &mut Vec<String>, limit: usize) {
    if output.len() >= limit {
        return;
    }
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        if output.len() >= limit {
            break;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(name.as_ref(), ".git" | ".xdudu" | ".xycli" | "target") {
            continue;
        }
        let path: PathBuf = entry.path();
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            collect_workspace_paths(root, &path, output, limit);
        } else if let Ok(relative) = path.strip_prefix(root) {
            output.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

fn screen_position(chars: &[char], until: usize, prompt_width: usize, width: usize) -> (u16, u16) {
    let mut row = 0usize;
    let mut column = prompt_width.min(width.saturating_sub(1));
    for character in chars.iter().take(until) {
        if *character == '\n' {
            row += 1;
            column = 0;
            continue;
        }
        let character_width = unicode_width::UnicodeWidthChar::width(*character).unwrap_or(0);
        if column + character_width >= width {
            row += 1;
            column = 0;
        }
        column += character_width;
    }
    (
        column.min(u16::MAX as usize) as u16,
        row.min(u16::MAX as usize) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn 支持光标移动和中间插入() {
        let mut state = LineState::default();
        state.handle_key(key(KeyCode::Char('a')), &[]);
        state.handle_key(key(KeyCode::Char('c')), &[]);
        state.handle_key(key(KeyCode::Left), &[]);
        state.handle_key(key(KeyCode::Char('b')), &[]);

        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &[]),
            Some(ReadResult::Line("abc".into()))
        );
    }

    #[test]
    fn 上下键浏览历史并恢复草稿() {
        let history = vec!["first".to_owned(), "second".to_owned()];
        let mut state = LineState::default();
        state.handle_key(key(KeyCode::Char('x')), &history);

        state.handle_key(key(KeyCode::Up), &history);
        assert_eq!(state.chars.iter().collect::<String>(), "second");
        state.handle_key(key(KeyCode::Up), &history);
        assert_eq!(state.chars.iter().collect::<String>(), "first");
        state.handle_key(key(KeyCode::Down), &history);
        state.handle_key(key(KeyCode::Down), &history);
        assert_eq!(state.chars.iter().collect::<String>(), "x");
    }

    #[test]
    fn ctrl_c_中断且空行_ctrl_d_结束() {
        let mut state = LineState::default();
        assert_eq!(
            state.handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &[]
            ),
            Some(ReadResult::Interrupted)
        );
        assert_eq!(
            state.handle_key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                &[]
            ),
            Some(ReadResult::Eof)
        );
    }

    #[test]
    fn 支持多行输入和斜杠候选() {
        let mut state = LineState::default();
        state.handle_key(key(KeyCode::Char('/')), &[]);
        assert!(!state.command_matches().is_empty());
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &[]);
        assert!(state.chars.contains(&'\n'));
    }

    #[test]
    fn at_候选可以补全工作区路径() {
        let mut state = LineState::default();
        state.handle_key(key(KeyCode::Char('@')), &[]);
        state.handle_key(key(KeyCode::Char('m')), &[]);
        assert!(state.complete_path(&["src/main.rs".into()]));
        assert_eq!(state.chars.iter().collect::<String>(), "@src/main.rs ");
    }

    #[test]
    fn 粘贴不提交且超长内容会截断() {
        let mut state = LineState::default();
        assert!(!state.insert_paste("第一行\r\n第二行"));
        assert_eq!(state.chars.iter().collect::<String>(), "第一行\n第二行");
        assert!(state.handle_key(key(KeyCode::Left), &[]).is_none());

        let mut large = LineState::default();
        assert!(large.insert_paste(&"x".repeat(MAX_INPUT_CHARS + 1)));
        assert_eq!(large.chars.len(), MAX_INPUT_CHARS);
    }

    #[test]
    fn 长输入只渲染光标附近但保留全部内容() {
        let chars = "x".repeat(1_000).chars().collect::<Vec<_>>();
        let (visible, cursor) = visible_input(&chars, chars.len(), 100);
        assert!(visible.len() < chars.len());
        assert!(visible.iter().collect::<String>().contains("已折叠"));
        assert!(cursor <= visible.len());
        assert_eq!(chars.len(), 1_000);
    }
}
