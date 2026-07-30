//! 审批菜单的终端键盘交互。

use std::io::{self, Write};

use crossterm::{
    cursor::{Hide, MoveToColumn, MoveUp, Show},
    event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, read},
    execute, queue,
    style::Print,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};

use crate::ui::TerminalTheme;

const OPTION_COUNT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalMenuChoice {
    Deny,
    Once,
    Session,
    Always,
}

impl ApprovalMenuChoice {
    const ALL: [Self; OPTION_COUNT] = [Self::Deny, Self::Once, Self::Session, Self::Always];

    const fn label(self) -> &'static str {
        match self {
            Self::Deny => "拒绝（默认）",
            Self::Once => "仅允许本次",
            Self::Session => "本会话允许同类操作",
            Self::Always => "始终允许同类操作",
        }
    }

    const fn result_label(self) -> &'static str {
        match self {
            Self::Deny => "已拒绝",
            Self::Once => "已允许 · 仅本次",
            Self::Session => "已允许 · 当前会话",
            Self::Always => "已允许 · 永久规则",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ApprovalMenuState {
    selected: usize,
}

impl ApprovalMenuState {
    fn handle_key(&mut self, key: KeyEvent) -> Option<ApprovalMenuChoice> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.checked_sub(1).unwrap_or(OPTION_COUNT - 1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % OPTION_COUNT;
                None
            }
            KeyCode::Home => {
                self.selected = 0;
                None
            }
            KeyCode::End => {
                self.selected = OPTION_COUNT - 1;
                None
            }
            KeyCode::Enter => Some(ApprovalMenuChoice::ALL[self.selected]),
            KeyCode::Esc => Some(ApprovalMenuChoice::Deny),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ApprovalMenuChoice::Deny)
            }
            _ => None,
        }
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stderr(), Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), Show);
        let _ = disable_raw_mode();
    }
}

fn render_menu(
    writer: &mut impl Write,
    state: ApprovalMenuState,
    redraw: bool,
    theme: TerminalTheme,
) -> io::Result<()> {
    if redraw {
        queue!(writer, MoveUp(OPTION_COUNT as u16))?;
    }
    for (index, choice) in ApprovalMenuChoice::ALL.iter().enumerate() {
        let marker = if index == state.selected {
            format!("  {} ", theme.accent("●"))
        } else {
            format!("  {} ", theme.muted("○"))
        };
        queue!(
            writer,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Print(marker),
            Print(choice.label()),
            Print("\r\n")
        )?;
    }
    writer.flush()
}

fn finish_menu(
    writer: &mut impl Write,
    choice: ApprovalMenuChoice,
    theme: TerminalTheme,
) -> io::Result<()> {
    queue!(writer, MoveUp(OPTION_COUNT as u16))?;
    for index in 0..OPTION_COUNT {
        queue!(writer, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        if index + 1 < OPTION_COUNT {
            queue!(writer, crossterm::cursor::MoveDown(1))?;
        }
    }
    queue!(
        writer,
        MoveUp((OPTION_COUNT - 1) as u16),
        MoveToColumn(0),
        Print("  "),
        Print(match choice {
            ApprovalMenuChoice::Deny => theme.danger("✗"),
            _ => theme.success("✓"),
        }),
        Print(" "),
        Print(choice.result_label()),
        Print("\r\n")
    )?;
    writer.flush()
}

pub(crate) fn read_approval_menu(
    theme: TerminalTheme,
    prompt: &str,
    fullscreen: bool,
) -> io::Result<ApprovalMenuChoice> {
    let _guard = TerminalGuard::enter()?;
    let mut writer: Box<dyn Write> = if fullscreen {
        Box::new(io::stdout())
    } else {
        Box::new(io::stderr())
    };
    if fullscreen {
        queue!(
            writer,
            MoveToColumn(0),
            Clear(ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        )?;
    }
    queue!(writer, Print(prompt))?;
    let mut state = ApprovalMenuState::default();
    render_menu(&mut writer, state, false, theme)?;
    loop {
        let Event::Key(key) = read()? else {
            continue;
        };
        let previous = state.selected;
        if let Some(choice) = state.handle_key(key) {
            finish_menu(&mut writer, choice, theme)?;
            return Ok(choice);
        }
        if state.selected != previous {
            render_menu(&mut writer, state, true, theme)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEvent;

    use super::*;

    #[test]
    fn 默认选择拒绝并由_enter_确认() {
        let mut state = ApprovalMenuState::default();

        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(ApprovalMenuChoice::Deny)
        );
    }

    #[test]
    fn 上下方向键移动并循环选择() {
        let mut state = ApprovalMenuState::default();

        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(ApprovalMenuChoice::Once)
        );

        state.selected = 0;
        state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(ApprovalMenuChoice::Always)
        );
    }

    #[test]
    fn esc_和_ctrl_c_安全拒绝() {
        let mut state = ApprovalMenuState { selected: 2 };

        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(ApprovalMenuChoice::Deny)
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(ApprovalMenuChoice::Deny)
        );
    }
}
