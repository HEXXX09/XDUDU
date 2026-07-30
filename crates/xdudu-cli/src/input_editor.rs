//! 轻量交互行编辑器。
//!
//! 不持久化历史，避免把用户可能输入的秘密写入磁盘。

use std::io::{self, IsTerminal, Write};

use crossterm::{
    cursor::{MoveToColumn, MoveToNextLine},
    event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, read},
    queue,
    style::Print,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadResult {
    Line(String),
    Interrupted,
    Eof,
}

#[derive(Debug, Default)]
pub(crate) struct InputEditor {
    history: Vec<String>,
}

#[derive(Debug, Default)]
struct LineState {
    chars: Vec<char>,
    cursor: usize,
    history_index: Option<usize>,
    draft: Vec<char>,
}

impl LineState {
    fn handle_key(&mut self, key: KeyEvent, history: &[String]) -> Option<ReadResult> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        match key.code {
            KeyCode::Enter => Some(ReadResult::Line(self.chars.iter().collect())),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ReadResult::Interrupted)
            }
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.chars.is_empty() =>
            {
                Some(ReadResult::Eof)
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = 0;
                None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.chars.len();
                None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.chars.insert(self.cursor, character);
                self.cursor += 1;
                self.history_index = None;
                None
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
                self.history_index = None;
                None
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
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
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.chars.len();
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

impl InputEditor {
    pub(crate) fn read_line(&mut self, prompt: &str) -> io::Result<ReadResult> {
        if !io::stdin().is_terminal() {
            return self.read_plain_line(prompt);
        }

        let _guard = RawModeGuard::enter()?;
        let mut stdout = io::stdout();
        let mut state = LineState::default();
        queue!(stdout, Print(prompt))?;
        stdout.flush()?;

        loop {
            match read()? {
                Event::Key(key) => {
                    if let Some(result) = state.handle_key(key, &self.history) {
                        queue!(stdout, MoveToNextLine(1), MoveToColumn(0))?;
                        stdout.flush()?;
                        if let ReadResult::Line(line) = &result
                            && !line.trim().is_empty()
                            && self.history.last() != Some(line)
                        {
                            self.history.push(line.clone());
                        }
                        return Ok(result);
                    }
                    redraw(&mut stdout, prompt, &state)?;
                }
                Event::Paste(value) => {
                    for character in value.chars().filter(|character| *character != '\r') {
                        if character == '\n' {
                            continue;
                        }
                        state.chars.insert(state.cursor, character);
                        state.cursor += 1;
                    }
                    redraw(&mut stdout, prompt, &state)?;
                }
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

fn redraw(writer: &mut impl Write, prompt: &str, state: &LineState) -> io::Result<()> {
    let line: String = state.chars.iter().collect();
    let suffix: String = state.chars[state.cursor..].iter().collect();
    queue!(
        writer,
        MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        Print(prompt),
        Print(line)
    )?;
    let suffix_width = UnicodeWidthStr::width(suffix.as_str()).min(u16::MAX as usize) as u16;
    if suffix_width > 0 {
        queue!(writer, crossterm::cursor::MoveLeft(suffix_width))?;
    }
    writer.flush()
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
}
