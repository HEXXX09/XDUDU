//! 默认经典终端界面：历史进入原生滚动区，只重绘底部活动区域。

use std::{
    collections::VecDeque,
    future::Future,
    io::{self, Write},
    sync::Mutex,
};

use async_trait::async_trait;
use crossterm::{
    cursor::{Hide, MoveToColumn, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute, queue,
    style::Print,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xdudu_core::{AgentEvent, AgentRunResult, ApprovalRequest, EventSink, XduduError, redact_text};

use crate::{
    approval_prompt::{ApprovalMenuChoice, format_approval_prompt},
    markdown::{MarkdownLineKind, terminal_markdown},
    ui::{TerminalTheme, tool_display_name, tool_phase_display},
};

pub(crate) struct ApprovalUiRequest {
    pub(crate) request: ApprovalRequest,
    pub(crate) response: oneshot::Sender<ApprovalMenuChoice>,
}

#[derive(Clone)]
pub(crate) struct ChannelEventSink {
    sender: mpsc::UnboundedSender<AgentEvent>,
}

impl ChannelEventSink {
    pub(crate) fn channel() -> (Self, mpsc::UnboundedReceiver<AgentEvent>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender }, receiver)
    }
}

#[async_trait]
impl EventSink for ChannelEventSink {
    async fn emit(&self, event: AgentEvent) {
        let _ = self.sender.send(event);
    }
}

pub(crate) struct ClassicTurnOutcome {
    pub(crate) result: AgentRunResult,
    pub(crate) queued: VecDeque<String>,
}

struct RawGuard;

impl RawGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnableBracketedPaste, Hide)?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, Show);
    }
}

#[derive(Default)]
struct ToolLine {
    call_id: String,
    name: String,
    detail: String,
}

struct PendingApproval {
    prompt: String,
    selected: usize,
    response: Option<oneshot::Sender<ApprovalMenuChoice>>,
}

#[derive(Default)]
struct LiveState {
    status: String,
    assistant: String,
    input: Vec<char>,
    cursor: usize,
    queued: VecDeque<String>,
    tools: Vec<ToolLine>,
    approval: Option<PendingApproval>,
}

struct ClassicScreen {
    active_rows: u16,
    theme: TerminalTheme,
}

impl ClassicScreen {
    fn new(theme: TerminalTheme) -> Self {
        Self {
            active_rows: 0,
            theme,
        }
    }

    fn clear_active(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.active_rows > 0 {
            queue!(
                writer,
                crossterm::cursor::MoveUp(self.active_rows),
                MoveToColumn(0),
                Clear(ClearType::FromCursorDown)
            )?;
            self.active_rows = 0;
        }
        Ok(())
    }

    fn commit_markdown(&mut self, source: &str) -> io::Result<()> {
        if source.trim().is_empty() {
            return Ok(());
        }
        let mut stdout = io::stdout();
        self.clear_active(&mut stdout)?;
        for line in terminal_markdown(source) {
            let text = match line.kind {
                MarkdownLineKind::Heading => self.theme.strong(&line.text),
                MarkdownLineKind::Code => self.theme.muted(&line.text),
                MarkdownLineKind::DiffAdd => self.theme.success(&line.text),
                MarkdownLineKind::DiffRemove => self.theme.danger(&line.text),
                MarkdownLineKind::DiffContext => self.theme.muted(&line.text),
                MarkdownLineKind::Body => line.text,
            };
            queue!(stdout, Print(text), Print("\r\n"))?;
        }
        stdout.flush()
    }

    fn commit_line(&mut self, line: &str) -> io::Result<()> {
        let mut stdout = io::stdout();
        self.clear_active(&mut stdout)?;
        queue!(stdout, Print(line), Print("\r\n"))?;
        stdout.flush()
    }

    fn draw(&mut self, state: &LiveState) -> io::Result<()> {
        let mut stdout = io::stdout();
        self.clear_active(&mut stdout)?;
        let mut lines = Vec::new();
        if !state.status.is_empty() {
            lines.push(self.theme.muted(&format!("─ {}", state.status)));
        }
        for tool in &state.tools {
            lines.push(format!(
                "{} {} · {}",
                self.theme.accent("⏺"),
                tool_display_name(&tool.name),
                self.theme.muted(&tool.detail)
            ));
        }
        if !state.assistant.trim().is_empty() {
            for line in terminal_markdown(&state.assistant) {
                lines.push(match line.kind {
                    MarkdownLineKind::Heading => self.theme.strong(&line.text),
                    MarkdownLineKind::Code => self.theme.muted(&line.text),
                    MarkdownLineKind::DiffAdd => self.theme.success(&line.text),
                    MarkdownLineKind::DiffRemove => self.theme.danger(&line.text),
                    MarkdownLineKind::DiffContext => self.theme.muted(&line.text),
                    MarkdownLineKind::Body => line.text,
                });
            }
        }
        if let Some(approval) = &state.approval {
            lines.extend(approval.prompt.lines().map(str::to_owned));
            for (index, choice) in ApprovalMenuChoice::ALL.iter().enumerate() {
                let marker = if index == approval.selected {
                    "●"
                } else {
                    "○"
                };
                lines.push(format!("  {marker} {}", choice.label()));
            }
            lines.push(self.theme.muted("  ↑↓/j/k 选择 · Enter 确认 · Esc 拒绝"));
        } else {
            if !state.queued.is_empty() {
                lines.push(
                    self.theme
                        .muted(&format!("  已排队 {} 条消息", state.queued.len())),
                );
            }
            let input = state.input.iter().collect::<String>().replace('\n', " ↵ ");
            lines.push(format!("{} {input}", self.theme.accent("❯")));
            lines.push(
                self.theme
                    .muted("  Enter 排队 · Shift+Enter 换行 · Esc/Ctrl+C 中断"),
            );
        }
        for line in &lines {
            queue!(stdout, Print(line), Print("\r\n"))?;
        }
        self.active_rows = lines.len().min(u16::MAX as usize) as u16;
        stdout.flush()
    }

    fn finish(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        self.clear_active(&mut stdout)?;
        queue!(stdout, Show)?;
        stdout.flush()
    }
}

pub(crate) async fn drive_turn<F>(
    run: F,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    mut approvals: mpsc::UnboundedReceiver<ApprovalUiRequest>,
    cancellation: CancellationToken,
    color: bool,
) -> Result<ClassicTurnOutcome, XduduError>
where
    F: Future<Output = Result<AgentRunResult, XduduError>>,
{
    let _raw = RawGuard::enter().map_err(XduduError::from)?;
    let mut terminal = EventStream::new();
    let mut run = std::pin::pin!(run);
    let mut state = LiveState {
        status: "正在理解任务".into(),
        ..Default::default()
    };
    let mut screen = ClassicScreen::new(TerminalTheme::new(color));
    screen.draw(&state).map_err(XduduError::from)?;

    let result = loop {
        tokio::select! {
            result = &mut run => break result?,
            Some(event) = events.recv() => {
                apply_agent_event(&mut state, &mut screen, event).map_err(XduduError::from)?;
                screen.draw(&state).map_err(XduduError::from)?;
            }
            Some(request) = approvals.recv() => {
                state.approval = Some(PendingApproval {
                    prompt: format_approval_prompt(screen.theme, &request.request),
                    selected: 0,
                    response: Some(request.response),
                });
                state.status = "等待批准".into();
                screen.draw(&state).map_err(XduduError::from)?;
            }
            input = terminal.next() => {
                match input {
                    Some(Ok(event)) => handle_terminal_event(&mut state, &cancellation, event),
                    Some(Err(error)) => return Err(XduduError::from(error)),
                    None => cancellation.cancel(),
                }
                screen.draw(&state).map_err(XduduError::from)?;
            }
        }
    };

    while let Ok(event) = events.try_recv() {
        apply_agent_event(&mut state, &mut screen, event).map_err(XduduError::from)?;
    }
    if !state.assistant.trim().is_empty() {
        screen
            .commit_markdown(&state.assistant)
            .map_err(XduduError::from)?;
        state.assistant.clear();
    } else if !result.final_message.trim().is_empty() {
        screen
            .commit_markdown(&redact_text(&result.final_message))
            .map_err(XduduError::from)?;
    }
    screen.finish().map_err(XduduError::from)?;
    Ok(ClassicTurnOutcome {
        result,
        queued: state.queued,
    })
}

fn apply_agent_event(
    state: &mut LiveState,
    screen: &mut ClassicScreen,
    event: AgentEvent,
) -> io::Result<()> {
    match event {
        AgentEvent::StateChanged { state: agent_state } => {
            state.status = format!("{agent_state:?}")
        }
        AgentEvent::AssistantDelta { text } => {
            state.assistant.push_str(&redact_text(&text));
            if let Some(boundary) = ready_markdown_boundary(&state.assistant) {
                let remaining = state.assistant.split_off(boundary);
                let ready = std::mem::replace(&mut state.assistant, remaining);
                screen.commit_markdown(&ready)?;
            }
        }
        AgentEvent::ToolStarted { call_id, name } => state.tools.push(ToolLine {
            call_id,
            name,
            detail: "启动".into(),
        }),
        AgentEvent::ToolProgress {
            call_id,
            phase,
            completed,
            total,
            unit,
            message,
            ..
        } => {
            if let Some(tool) = state.tools.iter_mut().find(|tool| tool.call_id == call_id) {
                let count = match (completed, total, unit) {
                    (Some(done), Some(total), Some(unit)) => format!(" · {done}/{total} {unit}"),
                    (Some(done), None, Some(unit)) => format!(" · {done} {unit}"),
                    _ => String::new(),
                };
                tool.detail = format!(
                    "{}{}",
                    message
                        .map(|value| redact_text(&value))
                        .unwrap_or_else(|| tool_phase_display(&phase).into()),
                    count
                );
            }
        }
        AgentEvent::ToolFinished {
            call_id,
            name,
            result,
        } => {
            state.tools.retain(|tool| tool.call_id != call_id);
            let marker = if result.success { "✓" } else { "✗" };
            let detail = if name == "web_search" {
                let query = result
                    .output
                    .as_ref()
                    .and_then(|value| value.get("query"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("未提供查询词");
                let count = result
                    .output
                    .as_ref()
                    .and_then(|value| value.get("resultCount"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                format!(
                    "{marker} 联网搜索（“{}”） · {count} 条结果 · {} ms",
                    redact_text(query),
                    result.duration_ms
                )
            } else {
                format!(
                    "{marker} {} · {} ms",
                    tool_display_name(&name),
                    result.duration_ms
                )
            };
            screen.commit_line(&detail)?;
        }
        AgentEvent::Warning { message, .. } => {
            screen.commit_line(&format!("! {}", redact_text(&message)))?
        }
        AgentEvent::PlanStepCompleted { summary, .. } => {
            screen.commit_line(&format!("✓ {}", redact_text(&summary)))?
        }
        AgentEvent::PlanStepFailed { error, .. } => {
            screen.commit_line(&format!("✗ {}", redact_text(&error)))?
        }
        AgentEvent::PlanPaused { reason, .. } => {
            screen.commit_line(&format!("Ⅱ {}", redact_text(&reason)))?
        }
        _ => {}
    }
    Ok(())
}

fn handle_terminal_event(state: &mut LiveState, cancellation: &CancellationToken, event: Event) {
    match event {
        Event::Paste(value) if state.approval.is_none() => {
            for character in value.chars().filter(|character| *character != '\r') {
                state.input.insert(state.cursor, character);
                state.cursor += 1;
            }
        }
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            if let Some(approval) = &mut state.approval {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        approval.selected = (approval.selected + 3) % 4
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        approval.selected = (approval.selected + 1) % 4
                    }
                    KeyCode::Enter => resolve_approval(state, None),
                    KeyCode::Esc => resolve_approval(state, Some(ApprovalMenuChoice::Deny)),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        resolve_approval(state, Some(ApprovalMenuChoice::Deny))
                    }
                    _ => {}
                }
                return;
            }
            match key.code {
                KeyCode::Esc => cancellation.cancel(),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    cancellation.cancel()
                }
                KeyCode::Enter
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    state.input.insert(state.cursor, '\n');
                    state.cursor += 1;
                }
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.input.insert(state.cursor, '\n');
                    state.cursor += 1;
                }
                KeyCode::Enter => {
                    let value = state.input.iter().collect::<String>();
                    if !value.trim().is_empty() {
                        state.queued.push_back(value);
                        state.input.clear();
                        state.cursor = 0;
                    }
                }
                KeyCode::Left if state.cursor > 0 => state.cursor -= 1,
                KeyCode::Right if state.cursor < state.input.len() => state.cursor += 1,
                KeyCode::Backspace if state.cursor > 0 => {
                    state.cursor -= 1;
                    state.input.remove(state.cursor);
                }
                KeyCode::Delete if state.cursor < state.input.len() => {
                    state.input.remove(state.cursor);
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    state.input.insert(state.cursor, character);
                    state.cursor += 1;
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn resolve_approval(state: &mut LiveState, forced: Option<ApprovalMenuChoice>) {
    if let Some(mut approval) = state.approval.take() {
        let choice = forced.unwrap_or(ApprovalMenuChoice::ALL[approval.selected]);
        if let Some(response) = approval.response.take() {
            let _ = response.send(choice);
        }
        state.status = choice.result_label().into();
    }
}

fn ready_markdown_boundary(source: &str) -> Option<usize> {
    let mut offset = 0;
    let mut boundary = None;
    let mut fence: Option<&str> = None;
    for line in source.split_inclusive('\n') {
        offset += line.len();
        let trimmed = line.trim();
        for marker in ["```", "~~~"] {
            if trimmed.starts_with(marker) {
                fence = if fence == Some(marker) {
                    None
                } else if fence.is_none() {
                    Some(marker)
                } else {
                    fence
                };
                if fence.is_none() {
                    boundary = Some(offset);
                }
            }
        }
        if trimmed.is_empty() && fence.is_none() {
            boundary = Some(offset);
        }
    }
    boundary
}

#[derive(Debug, Default)]
pub(crate) struct ApprovalBroker {
    sender: Mutex<Option<mpsc::UnboundedSender<ApprovalUiRequest>>>,
}

impl ApprovalBroker {
    pub(crate) fn attach(&self) -> mpsc::UnboundedReceiver<ApprovalUiRequest> {
        let (sender, receiver) = mpsc::unbounded_channel();
        *self.sender.lock().unwrap() = Some(sender);
        receiver
    }

    pub(crate) async fn request(&self, request: ApprovalRequest) -> Option<ApprovalMenuChoice> {
        let sender = self.sender.lock().unwrap().clone()?;
        let (response, receiver) = oneshot::channel();
        sender.send(ApprovalUiRequest { request, response }).ok()?;
        receiver.await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 流式_markdown_只提交完整段落和代码围栏() {
        assert_eq!(ready_markdown_boundary("标题还没结束"), None);
        assert_eq!(
            ready_markdown_boundary("第一段\n\n第二段"),
            Some("第一段\n\n".len())
        );
        assert_eq!(ready_markdown_boundary("```rust\nlet x = 1;\n"), None);
        assert_eq!(
            ready_markdown_boundary("```rust\nlet x = 1;\n```\n"),
            Some("```rust\nlet x = 1;\n```\n".len())
        );
    }
}
