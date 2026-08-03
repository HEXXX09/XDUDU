//! XDUDU 全屏终端界面。
//!
//! 界面采用“静态对话记录 + 实时活动区 + 底部 Composer”的布局，核心 Agent
//! 仍只通过 `AgentEvent` 发布状态，不依赖任何终端实现。

use std::{
    collections::VecDeque,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEventKind, read,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size,
    },
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use xdudu_core::{
    AgentEvent, AgentLoopState, AgentRunResult, EventSink, Plan, Session, provider::MessageRole,
    redact_text, redact_value,
};

use crate::{
    markdown::{MarkdownLineKind, terminal_markdown},
    ui::{
        ModelOption, model_display_name, model_matches, model_options, supports_true_color,
        tool_display_name, tool_phase_display,
    },
};

const CHROME_ROWS: u16 = 5;
const MAX_COMMAND_SUGGESTIONS: usize = 5;
const PRIMARY: Color = Color::Rgb {
    r: 252,
    g: 244,
    b: 163,
};
const TEXT: Color = Color::Rgb {
    r: 220,
    g: 218,
    b: 211,
};
const MUTED: Color = Color::Rgb {
    r: 145,
    g: 139,
    b: 120,
};
const BORDER: Color = Color::Rgb {
    r: 98,
    g: 92,
    b: 73,
};
const WARNING: Color = Color::Rgb {
    r: 200,
    g: 158,
    b: 87,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    AssistantHeading,
    AssistantCode,
    AssistantDiffAdd,
    AssistantDiffRemove,
    Tool,
    System,
    Warning,
}

#[derive(Debug, Clone)]
struct TranscriptBlock {
    role: Role,
    text: String,
}

#[derive(Debug, Clone)]
struct ToolActivity {
    call_id: String,
    name: String,
    detail: String,
    finished: Option<bool>,
    duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct SlashCommand {
    name: &'static str,
    usage: &'static str,
    description: &'static str,
    requires_argument: bool,
}

const SLASH_COMMANDS: [SlashCommand; 13] = [
    SlashCommand {
        name: "/help",
        usage: "/help",
        description: "显示交互命令",
        requires_argument: false,
    },
    SlashCommand {
        name: "/new",
        usage: "/new",
        description: "开始新会话",
        requires_argument: false,
    },
    SlashCommand {
        name: "/resume",
        usage: "/resume [id]",
        description: "浏览并恢复历史会话",
        requires_argument: false,
    },
    SlashCommand {
        name: "/plan",
        usage: "/plan <目标>",
        description: "生成或审阅执行计划",
        requires_argument: false,
    },
    SlashCommand {
        name: "/plan new",
        usage: "/plan new <目标>",
        description: "创建结构化计划",
        requires_argument: true,
    },
    SlashCommand {
        name: "/plan status",
        usage: "/plan status",
        description: "查看当前计划状态",
        requires_argument: false,
    },
    SlashCommand {
        name: "/plan run",
        usage: "/plan run",
        description: "执行已批准计划",
        requires_argument: false,
    },
    SlashCommand {
        name: "/plan retry",
        usage: "/plan retry",
        description: "重试暂停步骤",
        requires_argument: false,
    },
    SlashCommand {
        name: "/plan cancel",
        usage: "/plan cancel",
        description: "取消当前计划",
        requires_argument: false,
    },
    SlashCommand {
        name: "/plan revisions",
        usage: "/plan revisions",
        description: "查看修订版本",
        requires_argument: false,
    },
    SlashCommand {
        name: "/model",
        usage: "/model [name]",
        description: "选择或切换当前模型",
        requires_argument: false,
    },
    SlashCommand {
        name: "/turns",
        usage: "/turns <n>",
        description: "设置最大循环次数",
        requires_argument: true,
    },
    SlashCommand {
        name: "/exit",
        usage: "/exit",
        description: "退出 XDUDU",
        requires_argument: false,
    },
];

#[derive(Debug)]
struct TuiState {
    version: &'static str,
    provider: String,
    model: String,
    cwd: PathBuf,
    permission: String,
    approval: String,
    status: String,
    transcript: VecDeque<TranscriptBlock>,
    streaming: String,
    tools: Vec<ToolActivity>,
    input: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: Vec<char>,
    command_selection: usize,
    input_hint: String,
    input_active: bool,
    input_interrupted: bool,
    usage: Option<(u64, u64)>,
    transcript_scroll: usize,
    available_tools: Vec<String>,
    skills: Vec<String>,
    show_intro: bool,
    session_picker: Option<SessionPicker>,
    plan_review: Option<PlanReviewView>,
    model_picker: Option<ModelPicker>,
    debug_trace: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionChoice {
    pub(crate) id: uuid::Uuid,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) updated_at: String,
}

#[derive(Debug)]
struct SessionPicker {
    choices: Vec<SessionChoice>,
    selected: usize,
}

#[derive(Debug)]
struct ModelPicker {
    choices: Vec<ModelOption>,
    selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanReviewChoice {
    Approve,
    RequestChanges,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanRecoveryChoice {
    Continue,
    Retry,
    ViewDetails,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanDialogMode {
    Review,
    Recovery,
}

#[derive(Debug)]
struct PlanReviewView {
    plan: Plan,
    selected: usize,
    scroll: usize,
    mode: PlanDialogMode,
}

#[derive(Clone)]
pub(crate) struct TuiRenderer {
    state: Arc<Mutex<TuiState>>,
    color: bool,
}

pub(crate) struct TuiApp {
    renderer: TuiRenderer,
}

pub(crate) struct TuiContext {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) cwd: PathBuf,
    pub(crate) permission: String,
    pub(crate) approval: String,
    pub(crate) available_tools: Vec<String>,
    pub(crate) skills: Vec<String>,
    pub(crate) color: bool,
    pub(crate) debug_trace: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputOutcome {
    Submit(String),
    Command(String),
    Interrupted,
    Exit,
}

pub(crate) struct ScreenGuard;

impl ScreenGuard {
    fn enter() -> io::Result<Self> {
        execute!(
            io::stdout(),
            ResetColor,
            SetAttribute(Attribute::Reset),
            EnterAlternateScreen,
            EnableMouseCapture,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Hide
        )?;
        Ok(Self)
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

struct RawGuard;

impl RawGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

impl TuiApp {
    pub(crate) fn enter(context: TuiContext) -> io::Result<(Self, ScreenGuard)> {
        let guard = ScreenGuard::enter()?;
        let state = TuiState {
            version: env!("CARGO_PKG_VERSION"),
            provider: context.provider,
            model: context.model,
            cwd: context.cwd,
            permission: context.permission,
            approval: context.approval,
            status: "就绪".into(),
            transcript: VecDeque::new(),
            streaming: String::new(),
            tools: Vec::new(),
            input: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            draft: Vec::new(),
            command_selection: 0,
            input_hint: "Enter 发送 · / 命令 · ↑↓ 历史 · 滚轮/PgUp 对话 · Ctrl+D 退出".into(),
            input_active: true,
            input_interrupted: false,
            usage: None,
            transcript_scroll: 0,
            available_tools: context.available_tools,
            skills: context.skills,
            show_intro: true,
            session_picker: None,
            plan_review: None,
            model_picker: None,
            debug_trace: context.debug_trace,
        };
        let app = Self {
            renderer: TuiRenderer {
                state: Arc::new(Mutex::new(state)),
                color: context.color,
            },
        };
        app.renderer.draw()?;
        Ok((app, guard))
    }

    pub(crate) fn renderer(&self) -> TuiRenderer {
        self.renderer.clone()
    }

    pub(crate) fn set_model(&self, model: &str) -> io::Result<()> {
        let mut state = self.renderer.state.lock().unwrap();
        state.model = model.to_owned();
        state.status = "模型已切换".into();
        drop(state);
        self.renderer.draw()
    }

    pub(crate) fn notice(&self, message: impl Into<String>) -> io::Result<()> {
        let mut state = self.renderer.state.lock().unwrap();
        state.show_intro = false;
        state.transcript_scroll = 0;
        push_block(&mut state, Role::System, message.into());
        drop(state);
        self.renderer.draw()
    }

    pub(crate) fn load_session(&self, session: &Session) -> io::Result<()> {
        let mut state = self.renderer.state.lock().unwrap();
        load_session_state(&mut state, session);
        drop(state);
        self.renderer.draw()
    }

    pub(crate) fn select_session(
        &self,
        choices: Vec<SessionChoice>,
    ) -> io::Result<Option<uuid::Uuid>> {
        if choices.is_empty() {
            return Ok(None);
        }
        let _raw = RawGuard::enter()?;
        {
            let mut state = self.renderer.state.lock().unwrap();
            state.input_active = false;
            state.session_picker = Some(SessionPicker {
                choices,
                selected: 0,
            });
        }
        self.renderer.draw()?;

        let selected = loop {
            let Event::Key(key) = read()? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            let mut state = self.renderer.state.lock().unwrap();
            let Some(picker) = state.session_picker.as_mut() else {
                break None;
            };
            match key.code {
                KeyCode::Up => {
                    picker.selected = if picker.selected == 0 {
                        picker.choices.len() - 1
                    } else {
                        picker.selected - 1
                    };
                }
                KeyCode::Down => {
                    picker.selected = (picker.selected + 1) % picker.choices.len();
                }
                KeyCode::Enter => break Some(picker.choices[picker.selected].id),
                KeyCode::Esc => break None,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
                _ => {}
            }
            drop(state);
            self.renderer.draw()?;
        };

        {
            let mut state = self.renderer.state.lock().unwrap();
            state.session_picker = None;
            state.input_active = true;
        }
        self.renderer.draw()?;
        Ok(selected)
    }

    pub(crate) fn select_model(&self) -> io::Result<Option<String>> {
        let _raw = RawGuard::enter()?;
        {
            let mut state = self.renderer.state.lock().unwrap();
            let choices = model_options(&state.provider, &state.model);
            let selected = choices
                .iter()
                .position(|choice| model_matches(&choice.id, &state.model))
                .unwrap_or(0);
            state.input_active = false;
            state.model_picker = Some(ModelPicker { choices, selected });
        }
        self.renderer.draw()?;

        let selected = loop {
            let Event::Key(key) = read()? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            let mut state = self.renderer.state.lock().unwrap();
            let Some(picker) = state.model_picker.as_mut() else {
                break None;
            };
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    picker.selected = picker
                        .selected
                        .checked_sub(1)
                        .unwrap_or(picker.choices.len() - 1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    picker.selected = (picker.selected + 1) % picker.choices.len();
                }
                KeyCode::Enter => break Some(picker.choices[picker.selected].id.clone()),
                KeyCode::Esc => break None,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
                _ => {}
            }
            drop(state);
            self.renderer.draw()?;
        };

        {
            let mut state = self.renderer.state.lock().unwrap();
            state.model_picker = None;
            state.input_active = true;
        }
        self.renderer.draw()?;
        Ok(selected)
    }

    pub(crate) fn review_plan(&self, plan: &Plan) -> io::Result<Option<PlanReviewChoice>> {
        let _raw = RawGuard::enter()?;
        {
            let mut state = self.renderer.state.lock().unwrap();
            state.input_active = false;
            state.plan_review = Some(PlanReviewView {
                plan: plan.clone(),
                selected: 2,
                scroll: 0,
                mode: PlanDialogMode::Review,
            });
        }
        self.renderer.draw()?;

        let decision = loop {
            let Event::Key(key) = read()? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            let mut state = self.renderer.state.lock().unwrap();
            let Some(review) = state.plan_review.as_mut() else {
                break None;
            };
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    review.selected = if review.selected == 0 {
                        2
                    } else {
                        review.selected - 1
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    review.selected = (review.selected + 1) % 3;
                }
                KeyCode::PageUp => review.scroll = review.scroll.saturating_sub(5),
                KeyCode::PageDown => review.scroll = review.scroll.saturating_add(5),
                KeyCode::Enter => {
                    break Some(match review.selected {
                        0 => PlanReviewChoice::Approve,
                        1 => PlanReviewChoice::RequestChanges,
                        _ => PlanReviewChoice::Reject,
                    });
                }
                KeyCode::Esc => break None,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
                _ => {}
            }
            drop(state);
            self.renderer.draw()?;
        };

        {
            let mut state = self.renderer.state.lock().unwrap();
            state.plan_review = None;
            state.input_active = true;
        }
        self.renderer.draw()?;
        Ok(decision)
    }

    pub(crate) fn recover_plan(&self, plan: &Plan) -> io::Result<Option<PlanRecoveryChoice>> {
        let _raw = RawGuard::enter()?;
        {
            let mut state = self.renderer.state.lock().unwrap();
            state.input_active = false;
            state.plan_review = Some(PlanReviewView {
                plan: plan.clone(),
                selected: 2,
                scroll: 0,
                mode: PlanDialogMode::Recovery,
            });
        }
        self.renderer.draw()?;

        let decision = loop {
            let Event::Key(key) = read()? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            let mut state = self.renderer.state.lock().unwrap();
            let Some(view) = state.plan_review.as_mut() else {
                break None;
            };
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    view.selected = if view.selected == 0 {
                        3
                    } else {
                        view.selected - 1
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    view.selected = (view.selected + 1) % 4;
                }
                KeyCode::PageUp => view.scroll = view.scroll.saturating_sub(5),
                KeyCode::PageDown => view.scroll = view.scroll.saturating_add(5),
                KeyCode::Enter => {
                    break Some(match view.selected {
                        0 => PlanRecoveryChoice::Continue,
                        1 => PlanRecoveryChoice::Retry,
                        2 => PlanRecoveryChoice::ViewDetails,
                        _ => PlanRecoveryChoice::Cancel,
                    });
                }
                KeyCode::Esc => break None,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
                _ => {}
            }
            drop(state);
            self.renderer.draw()?;
        };

        {
            let mut state = self.renderer.state.lock().unwrap();
            state.plan_review = None;
            state.input_active = true;
        }
        self.renderer.draw()?;
        Ok(decision)
    }

    pub(crate) fn begin_prompt(&self, prompt: &str) -> io::Result<()> {
        let mut state = self.renderer.state.lock().unwrap();
        state.input_active = false;
        state.status = "正在处理".into();
        state.show_intro = false;
        push_block(&mut state, Role::User, prompt.to_owned());
        state.streaming.clear();
        state.tools.clear();
        state.input.clear();
        state.cursor = 0;
        state.history_index = None;
        state.transcript_scroll = 0;
        drop(state);
        self.renderer.draw()
    }

    pub(crate) fn finish_prompt(&self, result: &AgentRunResult) -> io::Result<()> {
        let mut state = self.renderer.state.lock().unwrap();
        let mut assistant = std::mem::take(&mut state.streaming);
        if assistant.trim().is_empty() {
            assistant.clone_from(&result.final_message);
        }
        if !assistant.trim().is_empty() {
            push_block(&mut state, Role::Assistant, redact_text(&assistant));
        }
        state.status = if result.exit_code == 0 {
            "就绪".into()
        } else {
            "未完成".into()
        };
        state.tools.clear();
        state.transcript_scroll = 0;
        state.input_active = true;
        drop(state);
        self.renderer.draw()
    }

    pub(crate) fn read_input(&self) -> io::Result<InputOutcome> {
        let _raw = RawGuard::enter()?;
        {
            let mut state = self.renderer.state.lock().unwrap();
            state.input_active = true;
            state.input_interrupted = false;
        }
        self.renderer.draw()?;

        loop {
            let event = read()?;
            if matches!(event, Event::Resize(_, _)) {
                self.renderer.draw()?;
                continue;
            }
            if let Event::Mouse(mouse) = event {
                let mut state = self.renderer.state.lock().unwrap();
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        state.transcript_scroll = state.transcript_scroll.saturating_add(3)
                    }
                    MouseEventKind::ScrollDown => {
                        state.transcript_scroll = state.transcript_scroll.saturating_sub(3)
                    }
                    _ => continue,
                }
                drop(state);
                self.renderer.draw()?;
                continue;
            }
            let Event::Key(key) = event else {
                continue;
            };
            let outcome = {
                let mut state = self.renderer.state.lock().unwrap();
                handle_input_key(&mut state, key)
            };
            self.renderer.draw()?;
            if let Some(outcome) = outcome {
                return Ok(outcome);
            }
        }
    }
}

impl TuiRenderer {
    fn draw(&self) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        let (columns, rows) = size().unwrap_or((80, 24));
        let mut stdout = io::stdout();
        queue!(stdout, Hide)?;
        for row in 0..rows {
            queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }

        if state.show_intro {
            draw_intro(&mut stdout, &state, columns, rows, self.color)?;
        }
        draw_transcript(&mut stdout, &state, columns, rows, self.color)?;
        draw_chrome(&mut stdout, &state, columns, rows, self.color)?;
        if state.session_picker.is_some() {
            draw_session_picker(&mut stdout, &state, columns, rows, self.color)?;
        }
        if state.plan_review.is_some() {
            draw_plan_review(&mut stdout, &state, columns, rows, self.color)?;
        }
        if state.model_picker.is_some() {
            draw_model_picker(&mut stdout, &state, columns, rows, self.color)?;
        }
        stdout.flush()
    }
}

#[async_trait]
impl EventSink for TuiRenderer {
    async fn emit(&self, event: AgentEvent) {
        {
            let mut state = self.state.lock().unwrap();
            if state.debug_trace
                && let Some(trace) = event.structured_trace()
                && let Ok(line) = serde_json::to_string(&redact_value(&trace))
            {
                push_block(&mut state, Role::System, format!("trace {line}"));
            }
            match event {
                AgentEvent::StateChanged { state: agent_state } => {
                    state.status = state_label(agent_state).into();
                }
                AgentEvent::AssistantDelta { text } => {
                    state.streaming.push_str(&redact_text(&text));
                }
                AgentEvent::ToolStarted { call_id, name } => {
                    state.tools.push(ToolActivity {
                        call_id,
                        name,
                        detail: "启动".into(),
                        finished: None,
                        duration_ms: None,
                    });
                }
                AgentEvent::ToolProgress {
                    call_id,
                    phase,
                    completed,
                    total,
                    unit,
                    message,
                    ..
                } => {
                    if let Some(tool) = state.tools.iter_mut().find(|tool| tool.call_id == call_id)
                    {
                        let count = match (completed, total, unit) {
                            (Some(done), Some(total), Some(unit)) => {
                                format!(" · {done}/{total} {unit}")
                            }
                            (Some(done), None, Some(unit)) => format!(" · {done} {unit}"),
                            _ => String::new(),
                        };
                        let detail = message
                            .map(|value| redact_text(&value))
                            .filter(|value| !value.trim().is_empty())
                            .unwrap_or_else(|| tool_phase_display(&phase).into());
                        tool.detail = format!("{detail}{count}");
                    }
                }
                AgentEvent::ToolFinished {
                    call_id, result, ..
                } => {
                    if let Some(index) = state.tools.iter().position(|tool| tool.call_id == call_id)
                    {
                        let tool = state.tools.remove(index);
                        let line = completed_tool_line(&tool, &result);
                        push_block(
                            &mut state,
                            if result.success {
                                Role::Tool
                            } else {
                                Role::Warning
                            },
                            line,
                        );
                    }
                }
                AgentEvent::UsageUpdated { usage } => {
                    state.usage = Some((usage.input_tokens, usage.output_tokens));
                }
                AgentEvent::Warning { message, .. } => {
                    push_block(&mut state, Role::Warning, redact_text(&message));
                }
                AgentEvent::DebugTrace { .. } => {}
                AgentEvent::PlanStarted { .. } => state.status = "执行计划".into(),
                AgentEvent::PlanStepStarted { title, attempt, .. } => {
                    state.status = format!("步骤 {title} · 第 {attempt} 次");
                }
                AgentEvent::PlanStepCompleted {
                    summary, evidence, ..
                } => {
                    push_block(
                        &mut state,
                        Role::System,
                        format!("步骤完成：{}", redact_text(&summary)),
                    );
                    for item in evidence {
                        push_block(
                            &mut state,
                            Role::System,
                            format!(
                                "证据 {}：{}",
                                item.criterion_index,
                                redact_text(&item.evidence)
                            ),
                        );
                    }
                }
                AgentEvent::PlanStepFailed { error, .. } => {
                    push_block(
                        &mut state,
                        Role::Warning,
                        format!("步骤失败：{}", redact_text(&error)),
                    );
                }
                AgentEvent::PlanPaused { reason, .. } => {
                    state.status = "计划暂停".into();
                    push_block(&mut state, Role::Warning, redact_text(&reason));
                }
                AgentEvent::PlanCompleted { .. } => state.status = "计划完成".into(),
            }
        }
        let _ = self.draw();
    }
}

fn handle_input_key(state: &mut TuiState, key: KeyEvent) -> Option<InputOutcome> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    match key.code {
        KeyCode::Enter => {
            let mut line = state.input.iter().collect::<String>();
            if line.trim().is_empty() {
                return None;
            }
            let suggestions = matching_commands(&state.input);
            if let Some(command) = suggestions.get(state.command_selection) {
                if command.requires_argument {
                    replace_input(state, &format!("{} ", command.name));
                    return None;
                }
                line = command.name.to_owned();
            }
            if state.history.last() != Some(&line) {
                state.history.push(line.clone());
            }
            state.input.clear();
            state.cursor = 0;
            state.history_index = None;
            state.command_selection = 0;
            if line.trim_start().starts_with('/') {
                Some(InputOutcome::Command(line))
            } else {
                Some(InputOutcome::Submit(line))
            }
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.input.is_empty() {
                state.input_interrupted = true;
                Some(InputOutcome::Interrupted)
            } else {
                state.input.clear();
                state.cursor = 0;
                state.history_index = None;
                state.command_selection = 0;
                None
            }
        }
        KeyCode::Char('d')
            if key.modifiers.contains(KeyModifiers::CONTROL) && state.input.is_empty() =>
        {
            Some(InputOutcome::Exit)
        }
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.cursor = 0;
            None
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.cursor = state.input.len();
            None
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.drain(..state.cursor);
            state.cursor = 0;
            None
        }
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.truncate(state.cursor);
            None
        }
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            delete_previous_word(state);
            None
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.input.insert(state.cursor, character);
            state.cursor += 1;
            state.history_index = None;
            state.command_selection = 0;
            None
        }
        KeyCode::Tab => {
            let suggestions = matching_commands(&state.input);
            if let Some(command) = suggestions.get(state.command_selection) {
                let suffix = if command.requires_argument { " " } else { "" };
                replace_input(state, &format!("{}{suffix}", command.name));
            }
            None
        }
        KeyCode::PageUp => {
            state.transcript_scroll = state.transcript_scroll.saturating_add(8);
            None
        }
        KeyCode::PageDown => {
            state.transcript_scroll = state.transcript_scroll.saturating_sub(8);
            None
        }
        KeyCode::Left if state.cursor > 0 => {
            state.cursor -= 1;
            None
        }
        KeyCode::Right if state.cursor < state.input.len() => {
            state.cursor += 1;
            None
        }
        KeyCode::Home => {
            state.cursor = 0;
            None
        }
        KeyCode::End => {
            state.cursor = state.input.len();
            None
        }
        KeyCode::Backspace if state.cursor > 0 => {
            state.cursor -= 1;
            state.input.remove(state.cursor);
            state.history_index = None;
            state.command_selection = 0;
            None
        }
        KeyCode::Delete if state.cursor < state.input.len() => {
            state.input.remove(state.cursor);
            state.history_index = None;
            state.command_selection = 0;
            None
        }
        KeyCode::Up if !matching_commands(&state.input).is_empty() => {
            let count = matching_commands(&state.input).len();
            state.command_selection = (state.command_selection + count.saturating_sub(1)) % count;
            None
        }
        KeyCode::Down if !matching_commands(&state.input).is_empty() => {
            let count = matching_commands(&state.input).len();
            state.command_selection = (state.command_selection + 1) % count;
            None
        }
        KeyCode::Up if !state.history.is_empty() => {
            let index = match state.history_index {
                None => {
                    state.draft.clone_from(&state.input);
                    state.history.len() - 1
                }
                Some(index) => index.saturating_sub(1),
            };
            load_history(state, index);
            None
        }
        KeyCode::Down => {
            match state.history_index {
                Some(index) if index + 1 < state.history.len() => load_history(state, index + 1),
                Some(_) => {
                    state.input.clone_from(&state.draft);
                    state.cursor = state.input.len();
                    state.history_index = None;
                }
                None => {}
            }
            None
        }
        _ => None,
    }
}

fn delete_previous_word(state: &mut TuiState) {
    while state.cursor > 0 && state.input[state.cursor - 1].is_whitespace() {
        state.cursor -= 1;
        state.input.remove(state.cursor);
    }
    while state.cursor > 0 && !state.input[state.cursor - 1].is_whitespace() {
        state.cursor -= 1;
        state.input.remove(state.cursor);
    }
}

fn load_history(state: &mut TuiState, index: usize) {
    state.input = state.history[index].chars().collect();
    state.cursor = state.input.len();
    state.history_index = Some(index);
    state.command_selection = 0;
}

fn load_session_state(state: &mut TuiState, session: &Session) {
    state.transcript.clear();
    state.streaming.clear();
    state.tools.clear();
    state.input.clear();
    state.cursor = 0;
    state.history_index = None;
    state.show_intro = false;
    state.status = "已恢复会话".into();
    state.transcript_scroll = 0;
    state.usage = Some((session.total_input_tokens, session.total_output_tokens));
    for message in &session.messages {
        let role = match message.role {
            MessageRole::User => Role::User,
            MessageRole::Assistant => Role::Assistant,
            MessageRole::System => Role::System,
            MessageRole::Tool => continue,
        };
        if !message.content.trim().is_empty() {
            push_block(state, role, redact_text(&message.content));
        }
    }
}

fn replace_input(state: &mut TuiState, value: &str) {
    state.input = value.chars().collect();
    state.cursor = state.input.len();
    state.history_index = None;
    state.command_selection = 0;
}

fn matching_commands(input: &[char]) -> Vec<&'static SlashCommand> {
    let value = input.iter().collect::<String>();
    if !value.starts_with('/') {
        return Vec::new();
    }
    if value.chars().any(char::is_whitespace) && !value.starts_with("/plan ") {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(&value))
        .take(MAX_COMMAND_SUGGESTIONS)
        .collect()
}

fn push_block(state: &mut TuiState, role: Role, text: String) {
    state.transcript.push_back(TranscriptBlock { role, text });
}

#[allow(dead_code)]
fn draw_header(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    color: bool,
) -> io::Result<()> {
    let title = format!(" XDUDU v{} ", state.version);
    let width = usize::from(columns);
    let left = width.saturating_sub(UnicodeWidthStr::width(title.as_str())) / 2;
    let right = width.saturating_sub(left + UnicodeWidthStr::width(title.as_str()));
    set_color(writer, color, BORDER)?;
    queue!(
        writer,
        MoveTo(0, 0),
        Print("─".repeat(left)),
        SetForegroundColor(PRIMARY),
        SetAttribute(Attribute::Bold),
        Print(title),
        SetAttribute(Attribute::Reset),
        Print("─".repeat(right))
    )?;
    reset_color(writer, color)?;
    Ok(())
}

fn draw_intro(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    rows: u16,
    color: bool,
) -> io::Result<()> {
    if rows < 14 || columns < 42 {
        set_color(writer, color, PRIMARY)?;
        queue!(
            writer,
            MoveTo(2, 1),
            SetAttribute(Attribute::Bold),
            Print(">_ XDUDU"),
            SetAttribute(Attribute::Reset)
        )?;
        set_color(writer, color, MUTED)?;
        queue!(
            writer,
            MoveTo(2, 2),
            Print(format!(
                "{} · {}",
                model_display_name(&state.provider, &state.model),
                state.provider
            )),
            MoveTo(2, 3),
            Print(format!(
                "{} tools · Skills {}",
                state.available_tools.len(),
                if state.skills.is_empty() {
                    "尚未启用"
                } else {
                    "已启用"
                }
            ))
        )?;
        return reset_color(writer, color);
    }

    let icon = [
        "   ▗▄▄▄▄▄▄▄▖",
        "▗▄▐  ▪   ▪  ▌▄▖",
        "  ▀▀▌  ▿  ▐▀▀",
        "    ▀   ▀",
    ];
    for (index, line) in icon.iter().enumerate() {
        set_color(writer, color, if index < 4 { PRIMARY } else { MUTED })?;
        queue!(writer, MoveTo(2, 1 + index as u16), Print(line))?;
    }

    let x = 22;
    set_color(writer, color, TEXT)?;
    queue!(
        writer,
        MoveTo(x, 1),
        SetAttribute(Attribute::Bold),
        Print("XDUDU"),
        SetAttribute(Attribute::Reset)
    )?;
    set_color(writer, color, MUTED)?;
    queue!(
        writer,
        Print(format!(" v{}", state.version)),
        MoveTo(x, 2),
        Print(truncate_to_width(
            &model_display_name(&state.provider, &state.model),
            usize::from(columns.saturating_sub(x + 2))
        )),
        MoveTo(x, 3),
        Print(truncate_to_width(
            &state.cwd.display().to_string(),
            usize::from(columns.saturating_sub(x + 2))
        )),
        MoveTo(x, 4),
        Print(format!(
            "{} tools · {}",
            state.available_tools.len(),
            state.permission
        ))
    )?;
    reset_color(writer, color)
}

#[allow(dead_code)]
fn draw_hermes_intro(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    rows: u16,
    color: bool,
) -> io::Result<()> {
    if rows < 20 || columns < 46 {
        set_color(writer, color, PRIMARY)?;
        queue!(
            writer,
            MoveTo(2, 2),
            SetAttribute(Attribute::Bold),
            Print(">_ XDUDU"),
            SetAttribute(Attribute::Reset)
        )?;
        set_color(writer, color, MUTED)?;
        queue!(
            writer,
            MoveTo(2, 3),
            Print(format!("{} · {}", state.provider, state.model)),
            MoveTo(2, 4),
            Print(format!(
                "{} tools · Skills 尚未启用",
                state.available_tools.len()
            ))
        )?;
        reset_color(writer, color)?;
        return Ok(());
    }

    let left = 1;
    let top = 2;
    let bottom = rows.saturating_sub(CHROME_ROWS + 2).min(17);
    let width = columns.saturating_sub(2);
    let split = (columns / 2).clamp(30, 36);
    set_color(writer, color, BORDER)?;
    queue!(
        writer,
        MoveTo(left, top),
        Print("╭"),
        Print("─".repeat(usize::from(width.saturating_sub(2)))),
        Print("╮")
    )?;
    for row in top + 1..bottom {
        queue!(
            writer,
            MoveTo(left, row),
            Print("│"),
            MoveTo(width, row),
            Print("│")
        )?;
    }
    queue!(
        writer,
        MoveTo(left, bottom),
        Print("╰"),
        Print("─".repeat(usize::from(width.saturating_sub(2)))),
        Print("╯")
    )?;
    for row in top + 1..bottom {
        queue!(writer, MoveTo(split, row), Print("│"))?;
    }

    let icon = [
        "          ·",
        "          │",
        "      ╭───┴───╮",
        "  ╭───│  · ·  │───╮",
        "  │   │   ▿   │   │",
        "  ╰───╰───┬───╯───╯",
        "          │",
        "      ╭───┴───╮",
        "      │  >_   │",
        "      ╰───────╯",
    ];
    for (index, line) in icon.iter().enumerate() {
        set_color(writer, color, if index < 6 { PRIMARY } else { MUTED })?;
        queue!(writer, MoveTo(5, top + 1 + index as u16), Print(line))?;
    }

    let model = truncate_to_width(&state.model, usize::from(split.saturating_sub(7)));
    set_color(writer, color, TEXT)?;
    queue!(
        writer,
        MoveTo(4, bottom.saturating_sub(3)),
        SetAttribute(Attribute::Bold),
        Print(model),
        SetAttribute(Attribute::Reset)
    )?;
    set_color(writer, color, MUTED)?;
    queue!(
        writer,
        MoveTo(4, bottom.saturating_sub(2)),
        Print(&state.provider),
        MoveTo(4, bottom.saturating_sub(1)),
        Print(truncate_to_width(
            &state.cwd.display().to_string(),
            usize::from(split.saturating_sub(6))
        ))
    )?;

    let x = split + 3;
    set_color(writer, color, PRIMARY)?;
    queue!(
        writer,
        MoveTo(x, top + 1),
        SetAttribute(Attribute::Bold),
        Print("当前运行"),
        SetAttribute(Attribute::Reset)
    )?;
    set_color(writer, color, MUTED)?;
    queue!(
        writer,
        MoveTo(x, top + 2),
        Print("模型  "),
        SetForegroundColor(TEXT),
        Print(truncate_to_width(
            &state.model,
            usize::from(columns.saturating_sub(x + 3))
        )),
        SetForegroundColor(MUTED),
        MoveTo(x, top + 3),
        Print(format!(
            "权限  {} · 审批 {}",
            state.permission, state.approval
        ))
    )?;

    set_color(writer, color, PRIMARY)?;
    queue!(
        writer,
        MoveTo(x, top + 5),
        SetAttribute(Attribute::Bold),
        Print("可用工具"),
        SetAttribute(Attribute::Reset)
    )?;
    let groups = tool_groups(&state.available_tools);
    for (index, (label, tools)) in groups.iter().take(4).enumerate() {
        let row = top + 6 + index as u16;
        set_color(writer, color, MUTED)?;
        queue!(writer, MoveTo(x, row), Print(format!("{label:<7}")))?;
        set_color(writer, color, TEXT)?;
        queue!(
            writer,
            Print(truncate_to_width(
                tools,
                usize::from(columns.saturating_sub(x + 10))
            ))
        )?;
    }

    set_color(writer, color, PRIMARY)?;
    queue!(
        writer,
        MoveTo(x, top + 11),
        SetAttribute(Attribute::Bold),
        Print("Skills"),
        SetAttribute(Attribute::Reset)
    )?;
    set_color(writer, color, MUTED)?;
    let skills = if state.skills.is_empty() {
        "尚未启用（M7 规划）".to_owned()
    } else {
        state.skills.join(", ")
    };
    queue!(
        writer,
        MoveTo(x, top + 12),
        Print(truncate_to_width(
            &skills,
            usize::from(columns.saturating_sub(x + 3))
        )),
        MoveTo(x, bottom.saturating_sub(1)),
        Print(format!(
            "{} tools · {} skills · /help",
            state.available_tools.len(),
            state.skills.len()
        ))
    )?;
    reset_color(writer, color)
}

fn tool_groups(tools: &[String]) -> Vec<(&'static str, String)> {
    let collect = |names: &[&str]| {
        names
            .iter()
            .filter(|name| tools.iter().any(|tool| tool == **name))
            .copied()
            .collect::<Vec<_>>()
            .join(", ")
    };
    [
        (
            "文件",
            collect(&["file_read", "file_write", "search_text", "apply_patch"]),
        ),
        ("Git", collect(&["git_status", "git_diff"])),
        ("网络", collect(&["web_search", "web_fetch"])),
        ("系统", collect(&["terminal_exec"])),
    ]
    .into_iter()
    .filter(|(_, tools)| !tools.is_empty())
    .collect()
}

fn draw_transcript(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    rows: u16,
    color: bool,
) -> io::Result<()> {
    let transcript_top = if state.show_intro && rows >= 14 && columns >= 42 {
        8
    } else if state.show_intro {
        6
    } else {
        2
    };
    let available = usize::from(rows.saturating_sub(CHROME_ROWS + transcript_top));
    let body_width = usize::from(columns.saturating_sub(4)).max(10);
    let mut lines: Vec<(Role, String)> = Vec::new();

    for block in &state.transcript {
        if block.role == Role::Assistant {
            append_markdown_wrapped(&mut lines, &block.text, body_width);
        } else {
            append_wrapped(&mut lines, block.role, &block.text, body_width);
        }
        lines.push((Role::System, String::new()));
    }
    for tool in &state.tools {
        let marker = match tool.finished {
            Some(true) => "✓",
            Some(false) => "✗",
            None => "●",
        };
        let duration = tool
            .duration_ms
            .map(|duration| format!(" · {duration} ms"))
            .unwrap_or_default();
        append_wrapped(
            &mut lines,
            if tool.finished == Some(false) {
                Role::Warning
            } else {
                Role::System
            },
            &format!(
                "{marker} {}  {}{duration}",
                tool_display_name(&tool.name),
                tool.detail
            ),
            body_width,
        );
    }
    if !state.streaming.is_empty() {
        append_markdown_wrapped(&mut lines, &state.streaming, body_width);
    }

    let start = transcript_window_start(lines.len(), available, state.transcript_scroll);
    for (offset, (role, line)) in lines.iter().skip(start).take(available).enumerate() {
        let row = transcript_top + offset as u16;
        queue!(writer, MoveTo(1, row))?;
        let glyph = match role {
            Role::User => "❯",
            Role::Assistant
            | Role::AssistantHeading
            | Role::AssistantCode
            | Role::AssistantDiffAdd
            | Role::AssistantDiffRemove => "┊",
            Role::Tool => "●",
            Role::System => "·",
            Role::Warning => "!",
        };
        let tone = match role {
            Role::User => PRIMARY,
            Role::Assistant => TEXT,
            Role::AssistantHeading => PRIMARY,
            Role::AssistantCode => MUTED,
            Role::AssistantDiffAdd => Color::Rgb {
                r: 132,
                g: 169,
                b: 140,
            },
            Role::AssistantDiffRemove => Color::Rgb {
                r: 190,
                g: 110,
                b: 110,
            },
            Role::Tool => PRIMARY,
            Role::System => MUTED,
            Role::Warning => WARNING,
        };
        set_color(writer, color, tone)?;
        queue!(writer, Print(glyph), Print(" "))?;
        if matches!(role, Role::User | Role::AssistantHeading) {
            queue!(writer, SetAttribute(Attribute::Bold))?;
        }
        queue!(writer, Print(line), SetAttribute(Attribute::Reset))?;
        reset_color(writer, color)?;
    }
    Ok(())
}

fn transcript_window_start(total: usize, visible: usize, scroll_from_bottom: usize) -> usize {
    let latest_start = total.saturating_sub(visible);
    latest_start.saturating_sub(scroll_from_bottom.min(latest_start))
}

fn completed_tool_line(tool: &ToolActivity, result: &xdudu_core::tools::ToolResult) -> String {
    let duration = if result.duration_ms >= 1_000 {
        format!("{:.1} s", result.duration_ms as f64 / 1_000.0)
    } else {
        format!("{} ms", result.duration_ms)
    };
    let name = tool_display_name(&tool.name);
    if tool.name == "web_search" {
        let query = result
            .output
            .as_ref()
            .and_then(|output| output.get("query"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                result
                    .error
                    .as_ref()
                    .and_then(|error| error.details.get("query"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(redact_text)
            .map(|value| value.replace(['\r', '\n'], " "))
            .unwrap_or_else(|| "未提供查询词".into());
        let count = result
            .output
            .as_ref()
            .and_then(|output| output.get("resultCount"))
            .and_then(serde_json::Value::as_u64);
        return if result.success {
            format!(
                "{name}（“{query}”）  {} 条结果 · {duration}",
                count.unwrap_or(0)
            )
        } else {
            format!("{name}（“{query}”）  失败 · {duration}")
        };
    }
    if result.success {
        format!("{name}  完成 · {duration}")
    } else {
        format!("{name}  失败 · {duration}")
    }
}

fn draw_chrome(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    rows: u16,
    color: bool,
) -> io::Result<()> {
    let (status_row, _, _, _) = chrome_positions(rows);
    // 状态、模型和权限已在其他区域展示，输入框上方只保留边界。
    // 运行和上翻时仅用边框色彩给出低干扰状态反馈。
    let border_color = if state.status.contains("失败") || state.status.contains("暂停") {
        WARNING
    } else if state.status != "就绪" || state.transcript_scroll > 0 {
        PRIMARY
    } else {
        BORDER
    };
    let _token_usage = state.usage;
    set_color(writer, color, border_color)?;
    queue!(
        writer,
        MoveTo(0, status_row),
        Clear(ClearType::CurrentLine),
        Print("─".repeat(usize::from(columns)))
    )?;
    reset_color(writer, color)?;

    draw_input_area(writer, state, columns, rows, color)
}

fn draw_input_area(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    rows: u16,
    color: bool,
) -> io::Result<()> {
    let (_, input_row, separator_row, help_row) = chrome_positions(rows);
    draw_command_suggestions(writer, state, columns, rows, color)?;
    set_color(writer, color, PRIMARY)?;
    queue!(
        writer,
        MoveTo(1, input_row),
        Clear(ClearType::CurrentLine),
        SetAttribute(Attribute::Bold),
        Print("❯"),
        SetAttribute(Attribute::Reset),
        Print(" ")
    )?;
    reset_color(writer, color)?;
    let input: String = state.input.iter().collect();
    queue!(writer, Print(&input))?;

    set_color(writer, color, BORDER)?;
    queue!(
        writer,
        MoveTo(0, separator_row),
        Clear(ClearType::CurrentLine),
        Print("─".repeat(usize::from(columns)))
    )?;
    reset_color(writer, color)?;

    set_color(writer, color, MUTED)?;
    let hint_text = if matching_commands(&state.input).is_empty() {
        state.input_hint.as_str()
    } else {
        "↑↓ 选择 · Tab 补全 · Enter 确定"
    };
    let hint = truncate_to_width(hint_text, usize::from(columns.saturating_sub(4)));
    queue!(
        writer,
        MoveTo(0, help_row),
        Clear(ClearType::CurrentLine),
        MoveTo(3, help_row),
        Print(hint)
    )?;
    reset_color(writer, color)?;

    if state.input_active {
        let prefix_width = 3_u16;
        let cursor_width = state.input[..state.cursor]
            .iter()
            .map(|character| character.width().unwrap_or(0))
            .sum::<usize>()
            .min(u16::MAX as usize) as u16;
        queue!(
            writer,
            MoveTo(prefix_width.saturating_add(cursor_width), input_row),
            Show
        )?;
    }
    Ok(())
}

fn draw_command_suggestions(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    rows: u16,
    color: bool,
) -> io::Result<()> {
    let suggestions = matching_commands(&state.input);
    if suggestions.is_empty() {
        return Ok(());
    }
    let count = suggestions.len().min(MAX_COMMAND_SUGGESTIONS);
    let start_row = rows.saturating_sub(4 + count as u16);
    for (index, command) in suggestions.into_iter().take(count).enumerate() {
        let row = start_row + index as u16;
        if row >= rows {
            break;
        }
        queue!(writer, MoveTo(2, row), Clear(ClearType::CurrentLine))?;
        let selected = index == state.command_selection.min(count.saturating_sub(1));
        set_color(writer, color, if selected { PRIMARY } else { MUTED })?;
        if selected {
            queue!(writer, SetAttribute(Attribute::Bold))?;
        }
        let line = format!(
            "{} {:<18} {}",
            if selected { "›" } else { " " },
            command.usage,
            command.description
        );
        queue!(
            writer,
            Print(truncate_to_width(
                &line,
                usize::from(columns.saturating_sub(4))
            )),
            SetAttribute(Attribute::Reset)
        )?;
        reset_color(writer, color)?;
    }
    Ok(())
}

fn chrome_positions(rows: u16) -> (u16, u16, u16, u16) {
    (
        rows.saturating_sub(4),
        rows.saturating_sub(3),
        rows.saturating_sub(2),
        rows.saturating_sub(1),
    )
}

fn draw_session_picker(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    rows: u16,
    color: bool,
) -> io::Result<()> {
    let Some(picker) = &state.session_picker else {
        return Ok(());
    };
    let top = 1;
    let bottom = rows.saturating_sub(2);
    for row in top..=bottom {
        queue!(writer, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
    }

    set_color(writer, color, PRIMARY)?;
    queue!(
        writer,
        MoveTo(2, top),
        SetAttribute(Attribute::Bold),
        Print("恢复历史会话"),
        SetAttribute(Attribute::Reset)
    )?;
    set_color(writer, color, MUTED)?;
    queue!(
        writer,
        MoveTo(2, top + 1),
        Print("↑↓ 选择 · Enter 恢复 · Esc 取消")
    )?;

    let visible = usize::from(rows.saturating_sub(6)).max(1);
    let start = picker
        .selected
        .saturating_sub(visible.saturating_sub(1))
        .min(picker.choices.len().saturating_sub(visible));
    for (offset, choice) in picker.choices.iter().skip(start).take(visible).enumerate() {
        let index = start + offset;
        let row = top + 3 + offset as u16;
        let selected = index == picker.selected;
        set_color(writer, color, if selected { PRIMARY } else { TEXT })?;
        if selected {
            queue!(writer, SetAttribute(Attribute::Bold))?;
        }
        let id = choice.id.to_string();
        let line = format!(
            "{} {}  {}  {:<11}  {}",
            if selected { "›" } else { " " },
            &id[..8],
            choice.updated_at,
            choice.status,
            choice.title.replace(['\r', '\n'], " ")
        );
        queue!(
            writer,
            MoveTo(2, row),
            Print(truncate_to_width(
                &line,
                usize::from(columns.saturating_sub(4))
            )),
            SetAttribute(Attribute::Reset)
        )?;
    }
    reset_color(writer, color)
}

fn draw_model_picker(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    rows: u16,
    color: bool,
) -> io::Result<()> {
    let Some(picker) = &state.model_picker else {
        return Ok(());
    };
    let top = 1_u16;
    let bottom = rows.saturating_sub(2);
    for row in top..=bottom {
        queue!(writer, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
    }

    set_color(writer, color, PRIMARY)?;
    queue!(
        writer,
        MoveTo(2, top),
        SetAttribute(Attribute::Bold),
        Print("选择模型"),
        SetAttribute(Attribute::Reset)
    )?;
    set_color(writer, color, MUTED)?;
    queue!(
        writer,
        MoveTo(2, top + 1),
        Print("↑↓/j/k 选择 · Enter 确认 · Esc 取消")
    )?;

    for (index, choice) in picker.choices.iter().enumerate() {
        let row = top + 3 + (index as u16 * 2);
        if row + 1 >= bottom {
            break;
        }
        let selected = index == picker.selected;
        set_color(writer, color, if selected { PRIMARY } else { TEXT })?;
        if selected {
            queue!(writer, SetAttribute(Attribute::Bold))?;
        }
        let current = model_matches(&choice.id, &state.model);
        let title = format!(
            "{} {}{}",
            if selected { "›" } else { " " },
            choice.label,
            if current { "  当前" } else { "" }
        );
        queue!(
            writer,
            MoveTo(2, row),
            Print(truncate_to_width(
                &title,
                usize::from(columns.saturating_sub(4))
            )),
            SetAttribute(Attribute::Reset)
        )?;
        set_color(writer, color, MUTED)?;
        let detail = format!("  {} · {}", choice.description, choice.id);
        queue!(
            writer,
            MoveTo(4, row + 1),
            Print(truncate_to_width(
                &detail,
                usize::from(columns.saturating_sub(6))
            ))
        )?;
    }
    reset_color(writer, color)
}

fn draw_plan_review(
    writer: &mut impl Write,
    state: &TuiState,
    columns: u16,
    rows: u16,
    color: bool,
) -> io::Result<()> {
    let Some(review) = &state.plan_review else {
        return Ok(());
    };
    let recovery = review.mode == PlanDialogMode::Recovery;
    for row in 0..rows {
        queue!(writer, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
    }

    let width = usize::from(columns.saturating_sub(4)).max(20);
    let mut lines = Vec::new();
    append_wrapped(
        &mut lines,
        Role::System,
        &format!("目标：{}", review.plan.goal),
        width,
    );
    lines.push((Role::System, format!("修订版本：{}", review.plan.revision)));
    lines.push((Role::System, format!("状态：{:?}", review.plan.status)));
    if let Some(reason) = &review.plan.paused_reason {
        append_wrapped(
            &mut lines,
            Role::Warning,
            &format!("暂停原因：{reason}"),
            width,
        );
    }
    lines.push((Role::System, String::new()));
    let indexes = review
        .plan
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| (step.id, index + 1))
        .collect::<std::collections::HashMap<_, _>>();
    for (index, step) in review.plan.steps.iter().enumerate() {
        append_wrapped(
            &mut lines,
            Role::Assistant,
            &format!("{}. {} [{:?}]", index + 1, step.title, step.status),
            width,
        );
        if !step.description.trim().is_empty() {
            append_wrapped(
                &mut lines,
                Role::System,
                &format!("   {}", step.description),
                width,
            );
        }
        if !step.dependencies.is_empty() {
            let dependencies = step
                .dependencies
                .iter()
                .filter_map(|id| indexes.get(id))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("、");
            lines.push((Role::System, format!("   依赖步骤：{dependencies}")));
        }
        for criterion in &step.completion_criteria {
            append_wrapped(
                &mut lines,
                Role::System,
                &format!("   ✓ {criterion}"),
                width,
            );
        }
        if let Some(attempt) = step.attempts.last() {
            lines.push((
                Role::System,
                format!("   执行尝试：#{} [{:?}]", attempt.attempt, attempt.status),
            ));
            if let Some(summary) = &attempt.summary {
                append_wrapped(
                    &mut lines,
                    Role::System,
                    &format!("   结果：{summary}"),
                    width,
                );
            }
            if let Some(error) = &attempt.error {
                append_wrapped(
                    &mut lines,
                    Role::Warning,
                    &format!("   错误：{error}"),
                    width,
                );
            }
            for evidence in &attempt.evidence {
                append_wrapped(
                    &mut lines,
                    Role::System,
                    &format!(
                        "   证据 {}：{}",
                        evidence.criterion_index, evidence.evidence
                    ),
                    width,
                );
            }
        }
        lines.push((Role::System, String::new()));
    }

    set_color(writer, color, PRIMARY)?;
    queue!(
        writer,
        MoveTo(2, 0),
        SetAttribute(Attribute::Bold),
        Print(if recovery {
            "恢复暂停计划"
        } else {
            "审阅执行计划"
        }),
        SetAttribute(Attribute::Reset)
    )?;
    set_color(writer, color, MUTED)?;
    if rows > 1 {
        queue!(
            writer,
            MoveTo(2, 1),
            Print(truncate_to_width(
                if recovery {
                    "↑↓/j/k 选择 · Enter 确认 · PgUp/PgDn 滚动 · Esc 保持暂停"
                } else {
                    "↑↓/j/k 选择 · Enter 确认 · PgUp/PgDn 滚动 · Esc 保留待审"
                },
                width
            ))
        )?;
    }

    let option_rows = 4_u16;
    let content_top = 3_u16;
    let content_height = usize::from(rows.saturating_sub(content_top + option_rows));
    let max_scroll = lines.len().saturating_sub(content_height);
    let start = review.scroll.min(max_scroll);
    for (offset, (role, line)) in lines.iter().skip(start).take(content_height).enumerate() {
        let row = content_top + offset as u16;
        let line_color = match role {
            Role::Assistant => TEXT,
            Role::Warning => WARNING,
            _ => MUTED,
        };
        set_color(writer, color, line_color)?;
        queue!(
            writer,
            MoveTo(2, row),
            Print(truncate_to_width(line, width))
        )?;
    }

    let options: &[&str] = if recovery {
        &["继续", "重试当前步骤", "查看详情", "取消计划"]
    } else {
        &["批准计划", "请求修改", "拒绝计划"]
    };
    let options_top = rows.saturating_sub(3);
    for (index, option) in options.iter().enumerate() {
        let selected = index == review.selected;
        set_color(writer, color, if selected { PRIMARY } else { MUTED })?;
        if selected {
            queue!(writer, SetAttribute(Attribute::Bold))?;
        }
        let column = 2 + (index as u16 * columns.saturating_sub(4) / options.len() as u16);
        queue!(
            writer,
            MoveTo(column, options_top),
            Print(if selected { "› " } else { "  " }),
            Print(option),
            SetAttribute(Attribute::Reset)
        )?;
    }
    reset_color(writer, color)
}

fn truncate_to_width(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    let mut output = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width + 1 > width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

fn append_wrapped(lines: &mut Vec<(Role, String)>, role: Role, text: &str, width: usize) {
    for source_line in text.lines() {
        if source_line.is_empty() {
            lines.push((role, String::new()));
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0;
        for character in source_line.chars() {
            let character_width = character.width().unwrap_or(0);
            if current_width + character_width > width && !current.is_empty() {
                lines.push((role, std::mem::take(&mut current)));
                current_width = 0;
            }
            current.push(character);
            current_width += character_width;
        }
        lines.push((role, current));
    }
}

fn append_markdown_wrapped(lines: &mut Vec<(Role, String)>, text: &str, width: usize) {
    for line in terminal_markdown(text) {
        let role = match line.kind {
            MarkdownLineKind::Body => Role::Assistant,
            MarkdownLineKind::Heading => Role::AssistantHeading,
            MarkdownLineKind::Code => Role::AssistantCode,
            MarkdownLineKind::DiffAdd => Role::AssistantDiffAdd,
            MarkdownLineKind::DiffRemove => Role::AssistantDiffRemove,
            MarkdownLineKind::DiffContext => Role::AssistantCode,
        };
        append_wrapped(lines, role, &line.text, width);
    }
}

fn set_color(writer: &mut impl Write, enabled: bool, color: Color) -> io::Result<()> {
    if enabled {
        queue!(writer, SetForegroundColor(terminal_color(color)))?;
    }
    Ok(())
}

fn reset_color(writer: &mut impl Write, _enabled: bool) -> io::Result<()> {
    queue!(writer, ResetColor)?;
    Ok(())
}

/// Terminal.app 未声明真彩色时，把 RGB 安全映射到 xterm-256 调色板。
///
/// 这可以避免旧终端把 `38;2;R;G;B` 中的数值误当成独立 SGR 指令，
/// 从而出现亮洋红背景等错误颜色。
fn terminal_color(color: Color) -> Color {
    match color {
        Color::Rgb { r, g, b } if !supports_true_color() => {
            let component =
                |value: u8| -> u8 { ((u16::from(value) * 5 + 127) / 255).try_into().unwrap_or(5) };
            Color::AnsiValue(16 + 36 * component(r) + 6 * component(g) + component(b))
        }
        _ => color,
    }
}

fn state_label(state: AgentLoopState) -> &'static str {
    match state {
        AgentLoopState::Idle => "就绪",
        AgentLoopState::Planning => "规划中",
        AgentLoopState::Acting => "执行中",
        AgentLoopState::Observing => "观察结果",
        AgentLoopState::Reflecting => "继续思考",
        AgentLoopState::WaitingApproval => "等待批准",
        AgentLoopState::Completed => "已完成",
        AgentLoopState::Incomplete => "未完成",
        AgentLoopState::Interrupted => "已中断",
        AgentLoopState::Error => "错误",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> TuiState {
        TuiState {
            version: "0.6.0",
            provider: "DeepSeek".into(),
            model: "deepseek-chat".into(),
            cwd: PathBuf::from("/work"),
            permission: "auto-safe".into(),
            approval: "ask".into(),
            status: "就绪".into(),
            transcript: VecDeque::new(),
            streaming: String::new(),
            tools: Vec::new(),
            input: Vec::new(),
            cursor: 0,
            history: vec!["first".into()],
            history_index: None,
            draft: Vec::new(),
            command_selection: 0,
            input_hint: String::new(),
            input_active: true,
            input_interrupted: false,
            usage: None,
            transcript_scroll: 0,
            available_tools: vec!["file_read".into(), "git_status".into()],
            skills: Vec::new(),
            show_intro: true,
            session_picker: None,
            plan_review: None,
            model_picker: None,
            debug_trace: false,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn composer_支持编辑提交与历史() {
        let mut state = state();
        handle_input_key(&mut state, key(KeyCode::Char('a')));
        handle_input_key(&mut state, key(KeyCode::Char('c')));
        handle_input_key(&mut state, key(KeyCode::Left));
        handle_input_key(&mut state, key(KeyCode::Char('b')));
        assert_eq!(
            handle_input_key(&mut state, key(KeyCode::Enter)),
            Some(InputOutcome::Submit("abc".into()))
        );
        assert_eq!(handle_input_key(&mut state, key(KeyCode::Up)), None);
        assert_eq!(state.input.iter().collect::<String>(), "abc");
    }

    #[test]
    fn 斜杠命令支持候选选择补全和执行() {
        let mut state = state();
        handle_input_key(&mut state, key(KeyCode::Char('/')));
        assert_eq!(matching_commands(&state.input).len(), 5);

        handle_input_key(&mut state, key(KeyCode::Down));
        assert_eq!(state.command_selection, 1);
        handle_input_key(&mut state, key(KeyCode::Tab));
        assert_eq!(state.input.iter().collect::<String>(), "/new");

        assert_eq!(
            handle_input_key(&mut state, key(KeyCode::Enter)),
            Some(InputOutcome::Command("/new".into()))
        );
    }

    #[test]
    fn model_命令无需完整参数即可执行() {
        let mut state = state();
        for character in "/mod".chars() {
            handle_input_key(&mut state, key(KeyCode::Char(character)));
        }
        assert_eq!(
            handle_input_key(&mut state, key(KeyCode::Enter)),
            Some(InputOutcome::Command("/model".into()))
        );
    }

    #[test]
    fn page_up_和_page_down_控制对话滚动() {
        let mut state = state();
        assert_eq!(handle_input_key(&mut state, key(KeyCode::PageUp)), None);
        assert_eq!(state.transcript_scroll, 8);
        assert_eq!(handle_input_key(&mut state, key(KeyCode::PageDown)), None);
        assert_eq!(state.transcript_scroll, 0);
        assert_eq!(transcript_window_start(40, 8, 0), 32);
        assert_eq!(transcript_window_start(40, 8, 8), 24);
        assert_eq!(transcript_window_start(40, 8, usize::MAX), 0);
    }

    #[test]
    fn 输入区始终固定在终端底部() {
        assert_eq!(chrome_positions(24), (20, 21, 22, 23));
        assert_eq!(chrome_positions(12), (8, 9, 10, 11));
    }

    #[test]
    fn web_search_完成记录包含查询结果数和耗时() {
        let tool = ToolActivity {
            call_id: "call-1".into(),
            name: "web_search".into(),
            detail: "搜索：爪子刀".into(),
            finished: None,
            duration_ms: None,
        };
        let result = xdudu_core::tools::ToolResult::success(
            serde_json::json!({
                "query": "爪子刀",
                "resultCount": 5,
                "results": [],
            }),
            chrono::Utc::now(),
            serde_json::json!({}),
        );
        let line = completed_tool_line(&tool, &result);
        assert!(line.contains("联网搜索（“爪子刀”）"));
        assert!(line.contains("5 条结果"));
        assert!(line.contains("ms"));
    }

    #[test]
    fn resume_可以通过斜杠候选定位() {
        let input = "/r".chars().collect::<Vec<_>>();
        let commands = matching_commands(&input);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "/resume");
    }

    #[test]
    fn plan_可以通过斜杠候选定位并直接打开审阅() {
        let commands = matching_commands(&['/', 'p']);
        let plan = commands
            .iter()
            .find(|command| command.name == "/plan")
            .expect("应包含通用 /plan 命令");
        assert!(!plan.requires_argument);

        let commands = matching_commands(&"/plan r".chars().collect::<Vec<_>>());
        assert!(commands.iter().any(|command| command.name == "/plan run"));
        assert!(commands.iter().any(|command| command.name == "/plan retry"));
        assert!(
            commands
                .iter()
                .any(|command| command.name == "/plan revisions")
        );
    }

    #[test]
    fn 恢复会话会重建用户和助手时间线() {
        let session: Session = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(),
            "title": "历史会话",
            "cwd": "/work",
            "status": "completed",
            "currentState": "COMPLETED",
            "plan": {},
            "providerName": "deepseek",
            "model": "deepseek-chat",
            "messages": [
                {
                    "id": uuid::Uuid::new_v4(),
                    "role": "user",
                    "content": "问题",
                    "sequence": 0,
                    "createdAt": "2026-07-30T00:00:00Z"
                },
                {
                    "id": uuid::Uuid::new_v4(),
                    "role": "assistant",
                    "content": "回答",
                    "sequence": 1,
                    "createdAt": "2026-07-30T00:00:01Z"
                },
                {
                    "id": uuid::Uuid::new_v4(),
                    "role": "tool",
                    "content": "工具结果",
                    "toolCallId": "call-1",
                    "sequence": 2,
                    "createdAt": "2026-07-30T00:00:02Z"
                }
            ],
            "toolCalls": [],
            "totalInputTokens": 12,
            "totalOutputTokens": 8,
            "createdAt": "2026-07-30T00:00:00Z",
            "updatedAt": "2026-07-30T00:00:02Z",
            "completedAt": "2026-07-30T00:00:02Z"
        }))
        .unwrap();
        let mut state = state();
        load_session_state(&mut state, &session);
        assert_eq!(state.transcript.len(), 2);
        assert_eq!(state.transcript[0].role, Role::User);
        assert_eq!(state.transcript[1].role, Role::Assistant);
        assert_eq!(state.usage, Some((12, 8)));
        assert!(!state.show_intro);
    }

    #[test]
    fn 文本按_unicode_显示宽度换行() {
        let mut lines = Vec::new();
        append_wrapped(&mut lines, Role::Assistant, "你好Rust", 6);
        assert_eq!(lines[0].1, "你好Ru");
        assert_eq!(lines[1].1, "st");
    }
}
