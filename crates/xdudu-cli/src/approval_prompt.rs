//! 审批菜单的终端键盘交互。

use std::io::{self, Write};

use crossterm::{
    cursor::{Hide, MoveToColumn, MoveUp, Show},
    event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, read},
    execute, queue,
    style::Print,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use serde_json::Value;
use xdudu_core::{ApprovalRequest, redact_value};

use crate::ui::TerminalTheme;

const OPTION_COUNT: usize = 4;

pub(crate) fn format_approval_prompt(theme: TerminalTheme, request: &ApprovalRequest) -> String {
    let title = match request.tool_name.as_str() {
        "web_search" => "联网搜索",
        "web_fetch" => "读取网页",
        "terminal_exec" => "运行终端命令",
        "file_write" => "写入文件",
        "apply_patch" => "应用代码补丁",
        _ => "执行受控操作",
    };
    let detail = approval_detail(&request.tool_name, &redact_value(&request.input));
    let risk = match request.side_effect.as_str() {
        "network-access" => "将访问公开互联网",
        "process-execution" => "将在本机启动进程",
        "workspace-write" => "将修改当前工作区",
        value => value,
    };
    format!(
        "\n  {} {}  {}  {}  {}\n  {}\n\n",
        theme.warning("◆"),
        theme.strong(title),
        detail,
        theme.muted("·"),
        risk,
        theme.muted("↑↓ 选择 · Enter 确认 · Esc 拒绝"),
    )
}

fn approval_detail(tool_name: &str, input: &Value) -> String {
    let preferred = match tool_name {
        "web_search" => value_string(input, "query").map(|value| format!("搜索“{value}”")),
        "web_fetch" => value_string(input, "url").map(|value| format!("访问 {value}")),
        "terminal_exec" => value_string(input, "command").map(|value| format!("执行 {value}")),
        "file_write" => value_string(input, "path").map(|value| format!("写入 {value}")),
        "apply_patch" => value_string(input, "patch")
            .map(|value| format!("修改代码（补丁 {} 字节）", value.len())),
        _ => value_string(input, "path")
            .or_else(|| value_string(input, "url"))
            .or_else(|| value_string(input, "query")),
    };
    truncate_detail(
        preferred.unwrap_or_else(|| "已隐藏冗长参数，可拒绝后检查详情".into()),
        22,
    )
}

fn value_string(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.replace(['\r', '\n'], " "))
        .filter(|value| !value.trim().is_empty())
}

fn truncate_detail(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalMenuChoice {
    Deny,
    Once,
    Session,
    Always,
}

impl ApprovalMenuChoice {
    pub(crate) const ALL: [Self; OPTION_COUNT] =
        [Self::Deny, Self::Once, Self::Session, Self::Always];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Deny => "拒绝（默认）",
            Self::Once => "仅允许本次",
            Self::Session => "本会话允许同类操作",
            Self::Always => "始终允许同类操作",
        }
    }

    pub(crate) const fn result_label(self) -> &'static str {
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
    use chrono::Utc;
    use crossterm::event::KeyEvent;
    use serde_json::json;
    use uuid::Uuid;
    use xdudu_core::{PermissionLevel, SideEffectKind};

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

    #[test]
    fn web_审批只显示清晰摘要而不展开_json() {
        let request = ApprovalRequest {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            tool_name: "web_search".into(),
            permission_level: PermissionLevel::Network,
            side_effect: SideEffectKind::NetworkAccess,
            input: json!({"query":"Rust Agent 最新进展", "maxResults": 8}),
            requested_at: Utc::now(),
        };
        let prompt = format_approval_prompt(TerminalTheme::new(false), &request);
        assert!(prompt.contains("◆ 联网搜索  搜索“Rust Agent 最新进展”  ·  将访问公开互联网"));
        assert!(prompt.contains("搜索“Rust Agent 最新进展”"));
        assert!(prompt.contains("将访问公开互联网"));
        assert!(!prompt.contains("maxResults"));
        assert!(!prompt.contains('{'));
        assert_eq!(
            prompt
                .lines()
                .filter(|line| line.contains("联网搜索"))
                .count(),
            1
        );
    }
}
