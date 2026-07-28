//! Agent 主循环：规划、行动、观察，直到模型明确结束或达到限制。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    error::{ErrorKind, XycliError, XycliResult},
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
    tools::ToolRegistry,
};

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
    Message {
        id: Uuid::new_v4(),
        role,
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        sequence,
        created_at: Utc::now(),
    }
}

async fn load_or_create_session(config: &AgentRunConfig<'_>) -> XycliResult<Session> {
    if let Some(session_id) = config.session_id {
        let mut session =
            config.session_store.get(session_id).await?.ok_or_else(|| {
                XycliError::validation(format!("找不到要继续的会话：{session_id}"))
            })?;
        if session.cwd != config.cwd {
            return Err(XycliError::validation("不能在不同工作目录中继续已有会话。"));
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

    let now = Utc::now();
    let session = Session {
        id: Uuid::new_v4(),
        title: config.prompt.chars().take(80).collect(),
        cwd: config.cwd.clone(),
        status: SessionStatus::Running,
        current_state: AgentLoopState::Idle,
        plan: Value::Object(Default::default()),
        provider_name: config.provider.name().to_owned(),
        model: config.model.clone(),
        messages: vec![new_message(MessageRole::User, &config.prompt, 0)],
        tool_calls: Vec::new(),
        total_input_tokens: 0,
        total_output_tokens: 0,
        created_at: now,
        updated_at: now,
        completed_at: None,
    };
    config.session_store.create(&session).await?;
    Ok(session)
}

fn provider_messages(session: &Session) -> Vec<ProviderMessage> {
    session
        .messages
        .iter()
        .filter_map(|message| {
            if message.role == MessageRole::System {
                return None;
            }
            let content =
                if message.role == MessageRole::Assistant && !message.tool_calls.is_empty() {
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
        })
        .collect()
}

fn denied_tool_result(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("PERMISSION_DENIED" | "APPROVAL_DENIED" | "UNSAFE_COMMAND" | "PATH_OUTSIDE_WORKSPACE")
    )
}

/// 执行一次 Agent 任务。输入校验错误直接返回 `Err`；运行期错误会落入会话并返回结果。
pub async fn run_agent(config: AgentRunConfig<'_>) -> XycliResult<AgentRunResult> {
    if !(1..=100).contains(&config.max_turns) {
        return Err(XycliError::validation(
            "maxTurns 必须是 1 到 100 之间的整数。",
        ));
    }
    if config.prompt.trim().is_empty() {
        return Err(XycliError::validation("prompt 不能为空。"));
    }
    let mut session = load_or_create_session(&config).await?;
    let definitions = config.tool_registry.definitions();
    let provider_tools: Vec<_> = definitions
        .iter()
        .map(|definition| definition.provider_definition())
        .collect();
    let system = build_system_prompt(&definitions, Path::new(&config.cwd));
    let mut turns = 0;
    let mut status = SessionStatus::Running;
    let mut state = AgentLoopState::Planning;
    let mut final_message = String::new();
    let mut exit_code = 0;

    while turns < config.max_turns && status == SessionStatus::Running {
        if config.cancellation.is_cancelled() {
            status = SessionStatus::Interrupted;
            state = AgentLoopState::Error;
            final_message = "会话已被用户中断。".into();
            exit_code = 1;
            break;
        }
        turns += 1;
        state = if turns == 1 {
            AgentLoopState::Planning
        } else {
            AgentLoopState::Acting
        };
        session.current_state = state;
        emit(config.event_sink, AgentEvent::StateChanged { state }).await;
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
                state = AgentLoopState::Error;
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
                status = SessionStatus::Completed;
                state = AgentLoopState::Completed;
                final_message = assistant_text;
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
                for call in response.tool_calls {
                    if config.cancellation.is_cancelled() {
                        status = SessionStatus::Interrupted;
                        state = AgentLoopState::Error;
                        final_message = "会话已被用户中断。".into();
                        exit_code = 1;
                        break;
                    }
                    let started_at = Utc::now();
                    emit(
                        config.event_sink,
                        AgentEvent::ToolStarted {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                        },
                    )
                    .await;
                    let result = config
                        .tool_registry
                        .execute(
                            &call.name,
                            call.input.clone(),
                            session.id,
                            &config.cwd,
                            config.permission_mode,
                            config.cancellation.child_token(),
                        )
                        .await;
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
                    session.tool_calls.push(ToolCallRecord {
                        id: call.id.clone(),
                        tool_name: call.name,
                        input: call.input,
                        output: result.output.clone(),
                        error: result.error.as_ref().map(|error| error.message.clone()),
                        status: record_status,
                        duration_ms: Some(result.duration_ms),
                        started_at,
                        ended_at: Some(result.ended_at),
                        approval: result.approval.as_deref().cloned(),
                    });
                    let content = if result.success {
                        serde_json::to_string(&result.output.unwrap_or(Value::Null))?
                    } else {
                        format!(
                            "Error: {}",
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
                }
                if status == SessionStatus::Running {
                    state = AgentLoopState::Observing;
                    session.current_state = state;
                    session.updated_at = Utc::now();
                    config.session_store.update(&session).await?;
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

    #[async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        async fn chat(&self, _request: ProviderRequest) -> XycliResult<ProviderResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| XycliError::provider("没有更多模拟响应", false))
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
}
