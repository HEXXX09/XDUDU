//! Agent 主循环：规划、行动、观察、反思，直到模型明确结束或达到限制。

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    error::{ErrorKind, XduduError, XduduResult},
    events::{AgentEvent, EventSink, emit},
    permission::PermissionMode,
    prompt::build_system_prompt,
    provider::{
        ContentBlock, FinishReason, MessageContent, MessageRole, Provider, ProviderMessage,
        ProviderRequest, ProviderStreamEvent, ProviderStreamSink,
    },
    session::{
        AgentLoopState, Message, Session, SessionStatus, SessionStore, ToolCallRecord,
        ToolCallStatus,
    },
    tools::{ToolRegistry, ToolResult},
};

const DEFAULT_CONTEXT_INPUT_BUDGET: usize = 24_000;
const SUMMARY_CHARACTER_LIMIT: usize = 12_000;

pub struct AgentRunConfig<'a> {
    pub prompt: String,
    pub model: String,
    pub max_turns: u32,
    pub cwd: PathBuf,
    pub provider: &'a dyn Provider,
    pub tool_registry: &'a ToolRegistry,
    pub session_store: &'a dyn SessionStore,
    pub permission_mode: PermissionMode,
    pub cancellation: CancellationToken,
    pub session_id: Option<Uuid>,
    pub event_sink: Option<&'a dyn EventSink>,
    pub stream: bool,
}

struct AgentProviderSink<'a> {
    sink: Option<&'a dyn EventSink>,
}

#[async_trait]
impl ProviderStreamSink for AgentProviderSink<'_> {
    async fn emit(&self, event: ProviderStreamEvent) {
        match event {
            ProviderStreamEvent::TextDelta { text } => {
                emit(self.sink, AgentEvent::AssistantDelta { text }).await;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentRunResult {
    pub session_id: Uuid,
    pub status: SessionStatus,
    pub turns: u32,
    pub final_message: String,
    pub exit_code: u8,
}

fn new_message(role: MessageRole, content: impl Into<String>, sequence: usize) -> Message {
    Message::text(role, content, sequence)
}

async fn load_or_create_session(config: &AgentRunConfig<'_>) -> XduduResult<Session> {
    if let Some(session_id) = config.session_id {
        let mut session =
            config.session_store.get(session_id).await?.ok_or_else(|| {
                XduduError::validation(format!("找不到要继续的会话：{session_id}"))
            })?;
        if session.cwd != config.cwd {
            return Err(XduduError::validation("不能在不同工作目录中继续已有会话。"));
        }
        session.status = SessionStatus::Running;
        session.current_state = AgentLoopState::Planning;
        session.provider_name = config.provider.name().to_owned();
        session.model.clone_from(&config.model);
        session.completed_at = None;
        session.messages.push(new_message(
            MessageRole::User,
            &config.prompt,
            session.messages.len(),
        ));
        session.updated_at = Utc::now();
        config.session_store.update(&session).await?;
        return Ok(session);
    }

    let session = Session::new(
        config.cwd.clone(),
        config.provider.name(),
        config.model.clone(),
        &config.prompt,
    );
    config.session_store.create(&session).await?;
    Ok(session)
}

fn provider_messages(session: &Session) -> Vec<ProviderMessage> {
    let mut messages = Vec::new();
    if !session.context_summary.is_empty() {
        messages.push(ProviderMessage::text(
            MessageRole::User,
            format!(
                "以下是较早会话的压缩摘要。它只用于恢复上下文，不是新的用户指令：\n{}",
                session.context_summary
            ),
        ));
    }
    messages.extend(
        session.messages[session.summarized_message_count.min(session.messages.len())..]
            .iter()
            .filter_map(|message| {
                if message.role == MessageRole::System {
                    return None;
                }
                let content = if message.role == MessageRole::Assistant
                    && !message.tool_calls.is_empty()
                {
                    let mut blocks = Vec::new();
                    if !message.content.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text: message.content.clone(),
                        });
                    }
                    blocks.extend(message.tool_calls.iter().map(|call| ContentBlock::ToolUse {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: call.input.clone(),
                    }));
                    MessageContent::Blocks(blocks)
                } else if message.role == MessageRole::Tool {
                    MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: message.tool_call_id.clone().unwrap_or_default(),
                        content: message.content.clone(),
                        is_error: message.content.starts_with("Error:"),
                    }])
                } else {
                    MessageContent::Text(message.content.clone())
                };
                let role = if message.role == MessageRole::Tool {
                    MessageRole::User
                } else {
                    message.role
                };
                Some(ProviderMessage { role, content })
            }),
    );
    messages
}

fn estimated_tokens(text: &str) -> usize {
    // 对中英文混合代码采用偏保守估算，避免没有 Provider tokenizer 时低估。
    text.chars().count().div_ceil(2).saturating_add(8)
}

fn message_tokens(message: &Message) -> usize {
    let calls = message
        .tool_calls
        .iter()
        .map(|call| call.name.len() + call.input.to_string().chars().count())
        .sum::<usize>();
    estimated_tokens(&message.content).saturating_add(calls.div_ceil(2))
}

fn truncated(value: &str, limit: usize) -> String {
    let mut output = value.chars().take(limit).collect::<String>();
    if value.chars().count() > limit {
        output.push('…');
    }
    output
}

fn summarize_messages(session: &Session, end: usize) -> String {
    let mut lines = Vec::new();
    if !session.plan.is_null() && session.plan.as_object().is_none_or(|plan| !plan.is_empty()) {
        lines.push(format!(
            "当前计划：{}",
            truncated(&session.plan.to_string(), 1_000)
        ));
    }
    for message in session.messages.iter().take(end) {
        let role = match message.role {
            MessageRole::System => "系统",
            MessageRole::User => "用户",
            MessageRole::Assistant => "助手",
            MessageRole::Tool => "工具结果",
        };
        if !message.content.trim().is_empty() {
            lines.push(format!(
                "{role}：{}",
                truncated(message.content.trim(), 600)
            ));
        }
        for call in &message.tool_calls {
            lines.push(format!(
                "工具调用：{} {}",
                call.name,
                truncated(&call.input.to_string(), 400)
            ));
        }
    }
    truncated(&lines.join("\n"), SUMMARY_CHARACTER_LIMIT)
}

fn compact_context(session: &mut Session, system: &str, tools_json: &str) -> bool {
    let fixed_tokens = estimated_tokens(system)
        .saturating_add(estimated_tokens(tools_json))
        .saturating_add(1_500);
    let current_tokens = provider_messages(session)
        .iter()
        .map(|message| estimated_tokens(&serde_json::to_string(message).unwrap_or_default()))
        .sum::<usize>()
        .saturating_add(fixed_tokens);
    if current_tokens <= DEFAULT_CONTEXT_INPUT_BUDGET {
        return false;
    }

    let tail_budget = DEFAULT_CONTEXT_INPUT_BUDGET
        .saturating_sub(fixed_tokens)
        .saturating_sub(estimated_tokens(&session.context_summary))
        .max(2_000);
    let mut used: usize = 0;
    let mut start = session.messages.len();
    for (index, message) in session.messages.iter().enumerate().rev() {
        let cost = message_tokens(message);
        if used.saturating_add(cost) > tail_budget && start < session.messages.len() {
            break;
        }
        used = used.saturating_add(cost);
        start = index;
    }
    if start == 0 {
        return false;
    }
    if session.messages[start].role == MessageRole::Tool
        && let Some(call_id) = session.messages[start].tool_call_id.as_deref()
        && let Some(assistant_index) = session.messages[..start]
            .iter()
            .rposition(|message| message.tool_calls.iter().any(|call| call.id == call_id))
    {
        start = assistant_index;
    }
    if start == 0 || start >= session.messages.len() {
        return false;
    }
    session.context_summary = summarize_messages(session, start);
    session.summarized_message_count = start;
    true
}

fn denied_tool_result(code: Option<&str>) -> bool {
    matches!(
        code,
        Some(
            "PERMISSION_DENIED"
                | "APPROVAL_DENIED"
                | "UNSAFE_COMMAND"
                | "PATH_OUTSIDE_WORKSPACE"
                | "BATCH_SIDE_EFFECT_SKIPPED"
        )
    )
}

fn append_incomplete_reason(message: &str, reason: &str) -> String {
    if message.trim().is_empty() {
        reason.to_owned()
    } else {
        format!("{message}\n\n{reason}")
    }
}

/// 执行一次 Agent 任务。输入校验错误直接返回 `Err`；运行期错误会落入会话并返回结果。
pub async fn run_agent(config: AgentRunConfig<'_>) -> XduduResult<AgentRunResult> {
    if !(1..=100).contains(&config.max_turns) {
        return Err(XduduError::validation(
            "maxTurns 必须是 1 到 100 之间的整数。",
        ));
    }
    if config.prompt.trim().is_empty() {
        return Err(XduduError::validation("prompt 不能为空。"));
    }
    let mut session = load_or_create_session(&config).await?;
    let definitions = config.tool_registry.definitions();
    let provider_tools: Vec<_> = definitions
        .iter()
        .map(|definition| definition.provider_definition())
        .collect();
    let system = build_system_prompt(&definitions, Path::new(&config.cwd));
    let tools_json = serde_json::to_string(&provider_tools).unwrap_or_default();
    let mut turns = 0;
    let mut status = SessionStatus::Running;
    let mut state = AgentLoopState::Planning;
    let mut final_message = String::new();
    let mut exit_code = 0;
    let mut unresolved_tool_failures = BTreeSet::new();

    while turns < config.max_turns && status == SessionStatus::Running {
        if config.cancellation.is_cancelled() {
            status = SessionStatus::Interrupted;
            state = AgentLoopState::Interrupted;
            final_message = "会话已被用户中断。".into();
            exit_code = 1;
            break;
        }
        turns += 1;
        state = if turns == 1 {
            AgentLoopState::Planning
        } else {
            AgentLoopState::Reflecting
        };
        session.current_state = state;
        emit(config.event_sink, AgentEvent::StateChanged { state }).await;
        if compact_context(&mut session, &system, &tools_json) {
            session.updated_at = Utc::now();
            config.session_store.update(&session).await?;
            emit(
                config.event_sink,
                AgentEvent::Warning {
                    code: "CONTEXT_COMPACTED".into(),
                    message: format!(
                        "已压缩 {} 条较早消息，原始记录仍保存在本地会话中。",
                        session.summarized_message_count
                    ),
                },
            )
            .await;
        }
        let request = ProviderRequest {
            session_id: session.id.to_string(),
            model: config.model.clone(),
            messages: provider_messages(&session),
            tools: provider_tools.clone(),
            system: system.clone(),
            temperature: 0.2,
            max_output_tokens: 4096,
            cancellation: config.cancellation.child_token(),
        };
        let response_result = if config.stream {
            config
                .provider
                .stream_chat(
                    request,
                    &AgentProviderSink {
                        sink: config.event_sink,
                    },
                )
                .await
        } else {
            config.provider.chat(request).await
        };
        let response = match response_result {
            Ok(response) => response,
            Err(error) => {
                status = if config.cancellation.is_cancelled() {
                    SessionStatus::Interrupted
                } else {
                    SessionStatus::Error
                };
                state = if status == SessionStatus::Interrupted {
                    AgentLoopState::Interrupted
                } else {
                    AgentLoopState::Error
                };
                final_message = if status == SessionStatus::Interrupted {
                    "会话已被用户中断。".into()
                } else {
                    error.message
                };
                exit_code = if status == SessionStatus::Interrupted {
                    1
                } else {
                    ErrorKind::ProviderError.exit_code()
                };
                break;
            }
        };
        session.total_input_tokens += response.usage.input_tokens;
        session.total_output_tokens += response.usage.output_tokens;
        emit(
            config.event_sink,
            AgentEvent::UsageUpdated {
                usage: response.usage.clone(),
            },
        )
        .await;
        let assistant_text = response.message.text_content();
        if !config.stream && !assistant_text.is_empty() {
            emit(
                config.event_sink,
                AgentEvent::AssistantDelta {
                    text: assistant_text.clone(),
                },
            )
            .await;
        }
        session.messages.push(Message {
            id: Uuid::new_v4(),
            role: MessageRole::Assistant,
            content: assistant_text.clone(),
            tool_calls: response.tool_calls.clone(),
            tool_call_id: None,
            sequence: session.messages.len(),
            created_at: Utc::now(),
        });

        match response.finish_reason {
            FinishReason::Stop => {
                let has_unexecuted_calls = !response.tool_calls.is_empty()
                    || session.tool_calls.iter().any(|call| {
                        matches!(
                            call.status,
                            ToolCallStatus::Pending | ToolCallStatus::Running
                        )
                    });
                if has_unexecuted_calls || !unresolved_tool_failures.is_empty() {
                    status = SessionStatus::Incomplete;
                    state = AgentLoopState::Incomplete;
                    exit_code = 1;
                    let reason = if has_unexecuted_calls {
                        "模型停止时仍存在未执行或结果未知的工具调用，任务不能确认完成。".to_owned()
                    } else {
                        format!(
                            "仍有未解决的工具失败：{}。任务不能确认完成。",
                            unresolved_tool_failures
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join("、")
                        )
                    };
                    final_message = append_incomplete_reason(&assistant_text, &reason);
                    emit(
                        config.event_sink,
                        AgentEvent::Warning {
                            code: "UNCONFIRMED_COMPLETION".into(),
                            message: reason,
                        },
                    )
                    .await;
                } else {
                    status = SessionStatus::Completed;
                    state = AgentLoopState::Completed;
                    final_message = assistant_text;
                }
            }
            FinishReason::Length => {
                status = SessionStatus::Incomplete;
                state = AgentLoopState::Incomplete;
                exit_code = 1;
                final_message = format!(
                    "{}{}模型输出因长度限制被截断，任务尚未确认完成。",
                    assistant_text,
                    if assistant_text.is_empty() {
                        ""
                    } else {
                        "\n\n"
                    }
                );
                emit(
                    config.event_sink,
                    AgentEvent::Warning {
                        code: "OUTPUT_TRUNCATED".into(),
                        message: "模型输出因长度限制被截断。".into(),
                    },
                )
                .await;
            }
            FinishReason::ToolCalls if !response.tool_calls.is_empty() => {
                state = AgentLoopState::Acting;
                session.current_state = state;
                session.updated_at = Utc::now();
                config.session_store.update(&session).await?;
                emit(config.event_sink, AgentEvent::StateChanged { state }).await;
                let mut side_effect_denied_in_batch = false;
                for call in response.tool_calls {
                    if config.cancellation.is_cancelled() {
                        status = SessionStatus::Interrupted;
                        state = AgentLoopState::Interrupted;
                        final_message = "会话已被用户中断。".into();
                        exit_code = 1;
                        break;
                    }
                    let started_at = Utc::now();
                    let record_index = session.tool_calls.len();
                    session.tool_calls.push(ToolCallRecord {
                        id: call.id.clone(),
                        tool_name: call.name.clone(),
                        input: call.input.clone(),
                        output: None,
                        error: None,
                        status: ToolCallStatus::Pending,
                        duration_ms: None,
                        started_at,
                        ended_at: None,
                        approval: None,
                    });
                    session.current_state = AgentLoopState::Acting;
                    session.updated_at = Utc::now();
                    // 工具执行前先提交 pending，崩溃恢复时不会把结果未知的
                    // 副作用调用误认为尚未开始并自动重放。
                    config.session_store.update(&session).await?;
                    emit(
                        config.event_sink,
                        AgentEvent::ToolStarted {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                        },
                    )
                    .await;
                    let has_side_effect = config
                        .tool_registry
                        .get(&call.name)
                        .is_some_and(|tool| tool.definition().side_effect.requires_approval());
                    let result = if side_effect_denied_in_batch && has_side_effect {
                        ToolResult::failure(
                            "BATCH_SIDE_EFFECT_SKIPPED",
                            format!(
                                "同批较早的工具调用已被拒绝，为防止绕过审批，未执行工具“{}”。",
                                call.name
                            ),
                            started_at,
                            serde_json::json!({
                                "toolName": call.name,
                                "reason": "earlier-side-effect-denied",
                            }),
                        )
                    } else {
                        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);
                        let execution = config.tool_registry.execute_with_progress(
                            &call.name,
                            call.input.clone(),
                            session.id,
                            &config.cwd,
                            config.permission_mode,
                            config.cancellation.child_token(),
                            Some(progress_tx),
                        );
                        tokio::pin!(execution);
                        let mut progress_open = true;
                        loop {
                            tokio::select! {
                                result = &mut execution => break result,
                                update = progress_rx.recv(), if progress_open => {
                                    let Some(update) = update else {
                                        progress_open = false;
                                        continue;
                                    };
                                    emit(
                                        config.event_sink,
                                        AgentEvent::ToolProgress {
                                            call_id: call.id.clone(),
                                            name: call.name.clone(),
                                            phase: update.phase,
                                            completed: update.completed,
                                            total: update.total,
                                            unit: update.unit,
                                            message: update.message,
                                        },
                                    )
                                    .await;
                                }
                            }
                        }
                    };
                    emit(
                        config.event_sink,
                        AgentEvent::ToolFinished {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            result: result.clone(),
                        },
                    )
                    .await;
                    let error_code = result.error.as_ref().map(|error| error.code.as_str());
                    let record_status = if result.success {
                        ToolCallStatus::Succeeded
                    } else if denied_tool_result(error_code) {
                        ToolCallStatus::Denied
                    } else {
                        ToolCallStatus::Failed
                    };
                    if result.success {
                        unresolved_tool_failures.remove(&call.name);
                    } else {
                        unresolved_tool_failures.insert(call.name.clone());
                    }
                    if record_status == ToolCallStatus::Denied {
                        side_effect_denied_in_batch = true;
                    }
                    let record = &mut session.tool_calls[record_index];
                    record.output = result.output.clone();
                    record.error = result.error.as_ref().map(|error| error.message.clone());
                    record.status = record_status;
                    record.duration_ms = Some(result.duration_ms);
                    record.ended_at = Some(result.ended_at);
                    record.approval = result.approval.as_deref().cloned();
                    let content = if result.success {
                        serde_json::to_string(&result.output.unwrap_or(Value::Null))?
                    } else {
                        format!(
                            "Error [{}]: {}",
                            result
                                .error
                                .as_ref()
                                .map(|error| error.code.as_str())
                                .unwrap_or("UNKNOWN_ERROR"),
                            result
                                .error
                                .as_ref()
                                .map(|error| error.message.as_str())
                                .unwrap_or("未知错误")
                        )
                    };
                    let mut message =
                        new_message(MessageRole::Tool, content, session.messages.len());
                    message.tool_call_id = Some(call.id);
                    session.messages.push(message);
                    session.updated_at = Utc::now();
                    config.session_store.update(&session).await?;
                }
                if status == SessionStatus::Running {
                    state = AgentLoopState::Observing;
                    session.current_state = state;
                    session.updated_at = Utc::now();
                    config.session_store.update(&session).await?;
                    emit(config.event_sink, AgentEvent::StateChanged { state }).await;
                }
            }
            reason => {
                status = SessionStatus::Error;
                state = AgentLoopState::Error;
                exit_code = ErrorKind::ProviderError.exit_code();
                final_message = format!("Provider 以异常原因结束：{reason:?}");
            }
        }
    }

    if status == SessionStatus::Running && turns >= config.max_turns {
        status = SessionStatus::Incomplete;
        state = AgentLoopState::Incomplete;
        exit_code = 1;
        final_message = format!("已达到最大轮次 {}，任务尚未确认完成。", config.max_turns);
        emit(
            config.event_sink,
            AgentEvent::Warning {
                code: "MAX_TURNS_REACHED".into(),
                message: final_message.clone(),
            },
        )
        .await;
    }
    emit(config.event_sink, AgentEvent::StateChanged { state }).await;
    session.status = status;
    session.current_state = state;
    session.updated_at = Utc::now();
    session.completed_at = Some(Utc::now());
    let _ = config.session_store.update(&session).await;

    Ok(AgentRunResult {
        session_id: session.id,
        status,
        turns,
        final_message,
        exit_code,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::tempdir;

    use crate::{
        provider::{ProviderResponse, TokenUsage, ToolCall},
        session::JsonSessionStore,
        tools::register_builtins,
    };

    use super::*;

    struct MockProvider {
        responses: Mutex<VecDeque<ProviderResponse>>,
    }

    #[derive(Default)]
    struct RecordingEventSink {
        events: Mutex<Vec<AgentEvent>>,
    }

    #[async_trait]
    impl EventSink for RecordingEventSink {
        async fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl RecordingEventSink {
        fn states(&self) -> Vec<AgentLoopState> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::StateChanged { state } => Some(*state),
                    _ => None,
                })
                .collect()
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        async fn chat(&self, _request: ProviderRequest) -> XduduResult<ProviderResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| XduduError::provider("没有更多模拟响应", false))
        }
    }

    fn text_response(text: &str) -> ProviderResponse {
        ProviderResponse {
            message: ProviderMessage::text(MessageRole::Assistant, text),
            tool_calls: vec![],
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                ..Default::default()
            },
            finish_reason: FinishReason::Stop,
        }
    }

    fn tool_response(calls: Vec<ToolCall>) -> ProviderResponse {
        ProviderResponse {
            message: ProviderMessage {
                role: MessageRole::Assistant,
                content: MessageContent::Blocks(
                    calls
                        .iter()
                        .map(|call| ContentBlock::ToolUse {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            input: call.input.clone(),
                        })
                        .collect(),
                ),
            },
            tool_calls: calls,
            usage: TokenUsage::default(),
            finish_reason: FinishReason::ToolCalls,
        }
    }

    fn config<'a>(
        dir: &Path,
        provider: &'a dyn Provider,
        registry: &'a ToolRegistry,
        store: &'a dyn SessionStore,
    ) -> AgentRunConfig<'a> {
        AgentRunConfig {
            prompt: "测试任务".into(),
            model: "test".into(),
            max_turns: 5,
            cwd: dir.to_path_buf(),
            provider,
            tool_registry: registry,
            session_store: store,
            permission_mode: PermissionMode::AutoSafe,
            cancellation: CancellationToken::new(),
            session_id: None,
            event_sink: None,
            stream: false,
        }
    }

    #[tokio::test]
    async fn 文本响应一轮完成并保存会话() {
        let dir = tempdir().unwrap();
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([text_response("已完成")])),
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry).unwrap();
        let store = JsonSessionStore::new(dir.path());
        let result = run_agent(config(dir.path(), &provider, &registry, &store))
            .await
            .unwrap();
        assert_eq!(result.status, SessionStatus::Completed);
        assert_eq!(result.turns, 1);
        assert_eq!(
            store
                .get(result.session_id)
                .await
                .unwrap()
                .unwrap()
                .messages
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn 工具调用后继续下一轮() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let call = ToolCall {
            id: "call-1".into(),
            name: "file_read".into(),
            input: json!({"path":"a.txt"}),
        };
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                ProviderResponse {
                    message: ProviderMessage {
                        role: MessageRole::Assistant,
                        content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            input: call.input.clone(),
                        }]),
                    },
                    tool_calls: vec![call],
                    usage: TokenUsage::default(),
                    finish_reason: FinishReason::ToolCalls,
                },
                text_response("读取完成"),
            ])),
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry).unwrap();
        let store = JsonSessionStore::new(dir.path());
        let result = run_agent(config(dir.path(), &provider, &registry, &store))
            .await
            .unwrap();
        let session = store.get(result.session_id).await.unwrap().unwrap();
        assert_eq!(result.turns, 2);
        assert_eq!(session.tool_calls[0].status, ToolCallStatus::Succeeded);
    }

    #[tokio::test]
    async fn 工具结果后的状态依次进入观察和反思() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                tool_response(vec![ToolCall {
                    id: "call-state".into(),
                    name: "file_read".into(),
                    input: json!({"path":"a.txt"}),
                }]),
                text_response("读取完成"),
            ])),
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry).unwrap();
        let store = JsonSessionStore::new(dir.path());
        let sink = RecordingEventSink::default();
        let mut cfg = config(dir.path(), &provider, &registry, &store);
        cfg.event_sink = Some(&sink);

        let result = run_agent(cfg).await.unwrap();

        assert_eq!(result.status, SessionStatus::Completed);
        assert_eq!(
            sink.states(),
            vec![
                AgentLoopState::Planning,
                AgentLoopState::Acting,
                AgentLoopState::Observing,
                AgentLoopState::Reflecting,
                AgentLoopState::Completed,
            ]
        );
    }

    #[tokio::test]
    async fn 未解决的工具失败会阻止误报完成() {
        let dir = tempdir().unwrap();
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                tool_response(vec![ToolCall {
                    id: "call-failed".into(),
                    name: "file_read".into(),
                    input: json!({"path":"missing.txt"}),
                }]),
                text_response("没有找到文件"),
            ])),
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry).unwrap();
        let store = JsonSessionStore::new(dir.path());

        let result = run_agent(config(dir.path(), &provider, &registry, &store))
            .await
            .unwrap();

        assert_eq!(result.status, SessionStatus::Incomplete);
        assert_eq!(result.exit_code, 1);
        assert!(result.final_message.contains("未解决的工具失败：file_read"));
    }

    #[tokio::test]
    async fn 同一工具成功重试后可以完成() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                tool_response(vec![ToolCall {
                    id: "call-failed".into(),
                    name: "file_read".into(),
                    input: json!({"path":"missing.txt"}),
                }]),
                tool_response(vec![ToolCall {
                    id: "call-retry".into(),
                    name: "file_read".into(),
                    input: json!({"path":"a.txt"}),
                }]),
                text_response("已读取正确文件"),
            ])),
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry).unwrap();
        let store = JsonSessionStore::new(dir.path());
        let sink = RecordingEventSink::default();
        let mut cfg = config(dir.path(), &provider, &registry, &store);
        cfg.event_sink = Some(&sink);

        let result = run_agent(cfg).await.unwrap();

        assert_eq!(result.status, SessionStatus::Completed);
        assert_eq!(result.turns, 3);
        assert_eq!(
            sink.states(),
            vec![
                AgentLoopState::Planning,
                AgentLoopState::Acting,
                AgentLoopState::Observing,
                AgentLoopState::Reflecting,
                AgentLoopState::Acting,
                AgentLoopState::Observing,
                AgentLoopState::Reflecting,
                AgentLoopState::Completed,
            ]
        );
    }

    #[tokio::test]
    async fn 同批拒绝后跳过后续副作用但保留只读调用() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                tool_response(vec![
                    ToolCall {
                        id: "call-write".into(),
                        name: "file_write".into(),
                        input: json!({
                            "path":"blocked.txt",
                            "content":"blocked",
                            "createIfMissing":true
                        }),
                    },
                    ToolCall {
                        id: "call-exec".into(),
                        name: "terminal_exec".into(),
                        input: json!({"command":"pwd"}),
                    },
                    ToolCall {
                        id: "call-read".into(),
                        name: "file_read".into(),
                        input: json!({"path":"a.txt"}),
                    },
                ]),
                text_response("执行受限"),
            ])),
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry).unwrap();
        let store = JsonSessionStore::new(dir.path());

        let result = run_agent(config(dir.path(), &provider, &registry, &store))
            .await
            .unwrap();
        let session = store.get(result.session_id).await.unwrap().unwrap();

        assert_eq!(result.status, SessionStatus::Incomplete);
        assert_eq!(session.tool_calls[0].status, ToolCallStatus::Denied);
        assert_eq!(session.tool_calls[1].status, ToolCallStatus::Denied);
        assert!(
            session.tool_calls[1]
                .error
                .as_deref()
                .unwrap()
                .contains("未执行工具")
        );
        assert_eq!(session.tool_calls[2].status, ToolCallStatus::Succeeded);
        assert!(!dir.path().join("blocked.txt").exists());
    }

    #[tokio::test]
    async fn stop_携带未执行工具调用时标记未完成() {
        let dir = tempdir().unwrap();
        let call = ToolCall {
            id: "call-unexecuted".into(),
            name: "file_read".into(),
            input: json!({"path":"a.txt"}),
        };
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([ProviderResponse {
                message: ProviderMessage::text(MessageRole::Assistant, "准备读取"),
                tool_calls: vec![call],
                usage: TokenUsage::default(),
                finish_reason: FinishReason::Stop,
            }])),
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry).unwrap();
        let store = JsonSessionStore::new(dir.path());

        let result = run_agent(config(dir.path(), &provider, &registry, &store))
            .await
            .unwrap();

        assert_eq!(result.status, SessionStatus::Incomplete);
        assert!(result.final_message.contains("未执行或结果未知"));
    }

    #[tokio::test]
    async fn 达到轮次上限不会误报成功() {
        let dir = tempdir().unwrap();
        let call = || ToolCall {
            id: Uuid::new_v4().to_string(),
            name: "file_read".into(),
            input: json!({"path":"missing"}),
        };
        let responses = (0..2)
            .map(|_| {
                let call = call();
                ProviderResponse {
                    message: ProviderMessage::text(MessageRole::Assistant, "继续"),
                    tool_calls: vec![call],
                    usage: TokenUsage::default(),
                    finish_reason: FinishReason::ToolCalls,
                }
            })
            .collect();
        let provider = MockProvider {
            responses: Mutex::new(responses),
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry).unwrap();
        let store = JsonSessionStore::new(dir.path());
        let mut cfg = config(dir.path(), &provider, &registry, &store);
        cfg.max_turns = 2;
        let result = run_agent(cfg).await.unwrap();
        assert_eq!(result.status, SessionStatus::Incomplete);
        assert_eq!(result.exit_code, 1);
    }

    #[test]
    fn 长会话压缩后保留原始记录计划和最近消息() {
        let dir = tempdir().unwrap();
        let now = Utc::now();
        let mut session = Session {
            id: Uuid::new_v4(),
            title: "长会话".into(),
            cwd: dir.path().to_path_buf(),
            status: SessionStatus::Running,
            current_state: AgentLoopState::Planning,
            plan: json!({"goal":"必须保留的计划"}),
            provider_name: "mock".into(),
            model: "test".into(),
            messages: (0..100)
                .map(|index| {
                    new_message(
                        if index % 2 == 0 {
                            MessageRole::User
                        } else {
                            MessageRole::Assistant
                        },
                        format!("消息 {index} {}", "上下文内容".repeat(300)),
                        index,
                    )
                })
                .collect(),
            tool_calls: Vec::new(),
            context_summary: String::new(),
            summarized_message_count: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            created_at: now,
            updated_at: now,
            completed_at: None,
        };
        let original_count = session.messages.len();
        assert!(compact_context(&mut session, "系统约束", "[]"));
        assert_eq!(session.messages.len(), original_count);
        assert!(session.summarized_message_count > 0);
        assert!(session.context_summary.contains("必须保留的计划"));
        assert!(provider_messages(&session).len() < original_count);
        assert!(
            provider_messages(&session)
                .last()
                .unwrap()
                .text_content()
                .contains("消息 99")
        );
    }
}
