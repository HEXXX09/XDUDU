//! 子代理体系：`AgentProfile` 档案与 `task` 工具的执行循环。
//!
//! 主代理通过 `task` 工具把只读调研或独立子任务委派给隔离的子上下文；
//! 同一批可并行执行多个子代理。子代理不获得父会话没有的权限，工具
//! 执行仍走同一 Permission / Approval / Redaction / ChangeLedger 链，
//! 审计记录随父会话持久化；子代理消息不写入父会话历史。

use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    SideEffectKind,
    approval::ApprovalRequest,
    error::{XduduError, XduduResult},
    events::{AgentEvent, EventSink, emit},
    permission::{PermissionLevel, PermissionMode},
    provider::{
        ContentBlock, MessageContent, MessageRole, Provider, ProviderMessage, ProviderRequest,
        ProviderToolDefinition,
    },
    session::{ToolCallRecord, ToolCallStatus},
    tools::{ToolRegistry, ToolResult},
};

/// 档案模式：Primary 仅主代理可用，Subagent 仅可被委派，All 两者皆可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileMode {
    Primary,
    Subagent,
    All,
}

/// 统一 Agent 配置档案。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentProfile {
    pub id: String,
    pub description: String,
    pub mode: ProfileMode,
    /// 覆盖父会话模型。
    pub model: Option<String>,
    /// 子代理权限模式（与父会话取更严格者）。
    pub permission: PermissionMode,
    /// None = 跟随 permission 全量工具；Some = 显式白名单。
    pub allowed_tools: Option<Vec<String>>,
    pub max_turns: u32,
    /// 追加到系统提示词。
    pub system_extra: Option<String>,
}

/// 内置档案。
pub fn builtin_profiles() -> Vec<AgentProfile> {
    let read_only_tools = vec![
        "file_read".to_owned(),
        "search_text".to_owned(),
        "git_status".to_owned(),
        "git_diff".to_owned(),
    ];
    vec![
        AgentProfile {
            id: "build".into(),
            description: "默认主代理，全量工具。".into(),
            mode: ProfileMode::Primary,
            model: None,
            permission: PermissionMode::AutoSafe,
            allowed_tools: None,
            max_turns: 25,
            system_extra: None,
        },
        AgentProfile {
            id: "plan".into(),
            description: "只读规划，不产生副作用文件。".into(),
            mode: ProfileMode::Primary,
            model: None,
            permission: PermissionMode::ReadOnly,
            allowed_tools: Some(read_only_tools.clone()),
            max_turns: 8,
            system_extra: None,
        },
        AgentProfile {
            id: "explore".into(),
            description: "快速只读代码库探索。".into(),
            mode: ProfileMode::Subagent,
            model: None,
            permission: PermissionMode::ReadOnly,
            allowed_tools: Some(read_only_tools.clone()),
            max_turns: 8,
            system_extra: Some("你是只读探索子代理：只调查并报告，不做修改。".into()),
        },
        AgentProfile {
            id: "general".into(),
            description: "通用多步任务执行。".into(),
            mode: ProfileMode::Subagent,
            model: None,
            permission: PermissionMode::AutoSafe,
            allowed_tools: None,
            max_turns: 15,
            system_extra: None,
        },
        AgentProfile {
            id: "reviewer".into(),
            description: "代码审查，禁止修改。".into(),
            mode: ProfileMode::Subagent,
            model: None,
            permission: PermissionMode::ReadOnly,
            allowed_tools: Some(read_only_tools.clone()),
            max_turns: 8,
            system_extra: Some("你是代码审查子代理：只读审查，绝不修改文件或执行写操作。".into()),
        },
    ]
}

/// 按 ID 查找档案（区分大小写）。
pub fn find_profile<'a>(profiles: &'a [AgentProfile], id: &str) -> Option<&'a AgentProfile> {
    profiles.iter().find(|profile| profile.id == id)
}

/// 合并内置档案与自定义档案；自定义档案不与内置同名（不覆盖内置），
/// 冲突项忽略并返回冲突的 id 供告警。
pub fn merge_profiles(custom: Vec<AgentProfile>) -> (Vec<AgentProfile>, Vec<String>) {
    let mut result = builtin_profiles();
    let mut conflicts = Vec::new();
    for profile in custom {
        if result.iter().any(|builtin| builtin.id == profile.id) {
            conflicts.push(profile.id);
        } else {
            result.push(profile);
        }
    }
    (result, conflicts)
}

/// 取更严格的权限模式（子代理不获得父会话没有的权限）。
fn more_restrictive(parent: PermissionMode, profile: PermissionMode) -> PermissionMode {
    let rank = |mode: PermissionMode| match mode {
        PermissionMode::ReadOnly => 0,
        PermissionMode::AutoSafe => 1,
        PermissionMode::FullAccess => 2,
    };
    if rank(parent) <= rank(profile) {
        parent
    } else {
        profile
    }
}

/// `task` 工具的 Provider 定义；描述与 Schema 中列出全部可委派档案
/// （内置子代理 + `build`；`plan` 等纯主代理不可委派）。
pub fn task_tool_definition(profiles: &[AgentProfile]) -> ProviderToolDefinition {
    let subagents = profiles
        .iter()
        .filter(|profile| !(profile.mode == ProfileMode::Primary && profile.id != "build"))
        .collect::<Vec<_>>();
    let options = subagents
        .iter()
        .map(|profile| format!("{}：{}", profile.id, profile.description))
        .collect::<Vec<_>>()
        .join("；");
    let ids = subagents
        .iter()
        .map(|profile| profile.id.as_str())
        .collect::<Vec<_>>();
    ProviderToolDefinition {
        name: "task".into(),
        description: format!(
            "把只读调研或独立子任务委派给隔离子上下文，同一批可并行执行多个 task；子代理结果不回写主会话历史。可用档案：{options}",
        ),
        input_schema: json!({
            "type": "object",
            "required": ["agent", "prompt"],
            "additionalProperties": false,
            "properties": {
                "agent": { "type": "string", "enum": ids },
                "prompt": { "type": "string", "minLength": 1, "maxLength": 8192 }
            }
        }),
    }
}

/// 子代理执行结果：工具结果 + 审计记录 + 用量（由主循环汇总写入会话）。
#[derive(Debug)]
pub struct SubagentOutcome {
    pub result: ToolResult,
    /// 子代理内部工具调用的审计记录（随父会话持久化，不写消息历史）。
    pub audit: Vec<ToolCallRecord>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl SubagentOutcome {
    fn failure(
        code: &str,
        message: impl Into<String>,
        started_at: DateTime<Utc>,
        details: Value,
    ) -> Self {
        Self {
            result: ToolResult::failure(code, message, started_at, details),
            audit: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }
}

/// 子代理执行所需上下文（由主循环提供，均为借用）。
pub struct SubagentContext<'a> {
    pub provider: &'a dyn Provider,
    pub model: String,
    pub registry: &'a ToolRegistry,
    pub cwd: &'a Path,
    pub permission_mode: PermissionMode,
    pub cancellation: CancellationToken,
    pub event_sink: Option<&'a dyn EventSink>,
    pub session_id: Uuid,
    pub temperature: f32,
    pub max_output_tokens: u32,
    pub reasoning: bool,
    pub profiles: &'a [AgentProfile],
    /// 已渲染的自定义指令片段（用户/项目/仓库约定）。
    pub instructions: Vec<String>,
}

/// 执行一次子代理任务：隔离上下文循环，最多 `profile.max_turns` 轮。
///
/// - 子代理消息不写入父会话历史；工具结果转换为隔离消息回传；
/// - 工具执行走同一 Permission / Approval / Redaction 链，审计记录随
///   返回值由主循环持久化；
/// - 超出轮数或不可恢复错误返回 `SUBAGENT_INCOMPLETE`，不影响同批其他调用。
pub async fn run_subagent(
    context: &SubagentContext<'_>,
    input: Value,
    started_at: DateTime<Utc>,
) -> SubagentOutcome {
    let agent_id = input
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let prompt = input
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let Some(profile) = find_profile(context.profiles, agent_id) else {
        return SubagentOutcome::failure(
            "TOOL_NOT_FOUND",
            format!("子代理档案“{agent_id}”不存在。"),
            started_at,
            json!({ "agent": agent_id }),
        );
    };
    if profile.mode == ProfileMode::Primary && agent_id != "build" {
        return SubagentOutcome::failure(
            "INVALID_SUBAGENT",
            format!("档案“{agent_id}”是主代理，不能被委派。"),
            started_at,
            json!({ "agent": agent_id }),
        );
    }
    if prompt.is_empty() {
        return SubagentOutcome::failure(
            "INVALID_TOOL_INPUT",
            "prompt 不能为空。",
            started_at,
            json!({ "agent": agent_id }),
        );
    }
    // task 调用级别审批：general/build 需要 full-access 或审批（不能绕过审批）。
    let needs_task_approval = matches!(agent_id, "general" | "build")
        && context.permission_mode != PermissionMode::FullAccess;
    if needs_task_approval {
        let decision = context
            .registry
            .approval_gate()
            .review(&ApprovalRequest {
                id: Uuid::new_v4(),
                session_id: context.session_id,
                tool_name: "task".into(),
                input: input.clone(),
                permission_level: PermissionLevel::ReadOnly,
                side_effect: SideEffectKind::ProcessExecution,
                requested_at: Utc::now(),
            })
            .await;
        if !decision.approved {
            return SubagentOutcome::failure(
                "APPROVAL_DENIED",
                format!("子代理 task({agent_id}) 未获批准：{}", decision.reason),
                started_at,
                json!({ "agent": agent_id }),
            );
        }
    }
    let effective_permission = more_restrictive(context.permission_mode, profile.permission);
    emit(
        context.event_sink,
        AgentEvent::SubagentStarted {
            agent_id: agent_id.to_owned(),
            prompt: prompt.to_owned(),
        },
    )
    .await;

    // 隔离上下文：从用户提示开始，不继承父会话消息。
    let mut messages: Vec<ProviderMessage> = vec![ProviderMessage::text(MessageRole::User, prompt)];
    // 受限工具集：None 跟随权限全量；Some 显式白名单。
    let tool_defs: Vec<ProviderToolDefinition> = context
        .registry
        .definitions()
        .into_iter()
        .filter(|definition| {
            profile
                .allowed_tools
                .as_ref()
                .is_none_or(|allowed| allowed.contains(&definition.name))
        })
        .map(|definition| definition.provider_definition())
        .collect();
    let mut system =
        crate::prompt::build_system_prompt(&context.registry.definitions(), context.cwd);
    if !context.instructions.is_empty() {
        system.push_str("\n\n## 自定义指令\n\n");
        system.push_str(&context.instructions.join("\n"));
    }
    if let Some(extra) = &profile.system_extra {
        system.push_str("\n\n");
        system.push_str(extra);
    }
    let model = profile
        .model
        .clone()
        .unwrap_or_else(|| context.model.clone());

    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut audit: Vec<ToolCallRecord> = Vec::new();
    for turn in 0..profile.max_turns {
        if context.cancellation.is_cancelled() {
            emit(
                context.event_sink,
                AgentEvent::SubagentFinished {
                    agent_id: agent_id.to_owned(),
                    result: ToolResult::failure(
                        "SUBAGENT_ABORTED",
                        "子代理已中断。",
                        started_at,
                        json!({ "agent": agent_id }),
                    ),
                },
            )
            .await;
            return SubagentOutcome::failure(
                "SUBAGENT_ABORTED",
                "子代理已中断。",
                started_at,
                json!({ "agent": agent_id }),
            );
        }
        let request = ProviderRequest {
            session_id: format!("subagent-{}-{}", context.session_id, agent_id),
            model: model.clone(),
            messages: messages.clone(),
            tools: tool_defs.clone(),
            system: system.clone(),
            temperature: context.temperature,
            max_output_tokens: context.max_output_tokens,
            reasoning: context.reasoning,
            cancellation: context.cancellation.child_token(),
        };
        let response = match context.provider.chat(request).await {
            Ok(response) => response,
            Err(error) => {
                emit(
                    context.event_sink,
                    AgentEvent::SubagentFinished {
                        agent_id: agent_id.to_owned(),
                        result: ToolResult::failure(
                            "SUBAGENT_PROVIDER_ERROR",
                            error.message.clone(),
                            started_at,
                            json!({ "agent": agent_id }),
                        ),
                    },
                )
                .await;
                return SubagentOutcome::failure(
                    "SUBAGENT_PROVIDER_ERROR",
                    error.message,
                    started_at,
                    json!({ "agent": agent_id }),
                );
            }
        };
        input_tokens = input_tokens.saturating_add(response.usage.input_tokens);
        output_tokens = output_tokens.saturating_add(response.usage.output_tokens);
        let assistant_text = response.message.text_content();
        let mut blocks = Vec::new();
        if !assistant_text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: assistant_text.clone(),
            });
        }
        for call in &response.tool_calls {
            blocks.push(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            });
        }
        messages.push(ProviderMessage {
            role: MessageRole::Assistant,
            content: MessageContent::Blocks(blocks),
        });
        if response.tool_calls.is_empty() {
            let result = ToolResult::success(
                json!({
                    "agent": agent_id,
                    "result": assistant_text,
                    "turns": turn + 1,
                    "toolCalls": audit.len(),
                }),
                started_at,
                json!({ "agent": agent_id, "turns": turn + 1 }),
            );
            emit(
                context.event_sink,
                AgentEvent::SubagentFinished {
                    agent_id: agent_id.to_owned(),
                    result: result.clone(),
                },
            )
            .await;
            return SubagentOutcome {
                result,
                audit,
                input_tokens,
                output_tokens,
            };
        }
        // 执行子代理工具（同一审批链；结果落回隔离消息）。
        for call in &response.tool_calls {
            let call_started = Utc::now();
            let prefix = format!("task.{agent_id}.{}", call.name);
            emit(
                context.event_sink,
                AgentEvent::ToolStarted {
                    call_id: format!("sub.{agent_id}.{}", call.id),
                    name: prefix.clone(),
                },
            )
            .await;
            // Provider 返回的工具名仍是不可信输入。即使工具定义中只暴露了
            // 白名单，也必须在执行前再次强制检查，避免异常响应绕过档案边界。
            let allowed_by_profile = profile
                .allowed_tools
                .as_ref()
                .is_none_or(|allowed| allowed.contains(&call.name));
            let result = if allowed_by_profile {
                context
                    .registry
                    .execute_with_progress(
                        &call.name,
                        call.input.clone(),
                        context.session_id,
                        context.cwd,
                        effective_permission,
                        context.cancellation.child_token(),
                        None,
                    )
                    .await
            } else {
                ToolResult::failure(
                    "SUBAGENT_TOOL_DENIED",
                    format!("子代理档案“{agent_id}”不允许调用工具“{}”。", call.name),
                    call_started,
                    json!({ "agent": agent_id, "tool": call.name }),
                )
            };
            emit(
                context.event_sink,
                AgentEvent::ToolFinished {
                    call_id: format!("sub.{agent_id}.{}", call.id),
                    name: prefix,
                    result: result.clone(),
                },
            )
            .await;
            let record_status = if result.success {
                ToolCallStatus::Succeeded
            } else if result.error.as_ref().is_some_and(|error| {
                matches!(
                    error.code.as_str(),
                    "PERMISSION_DENIED"
                        | "APPROVAL_DENIED"
                        | "UNSAFE_COMMAND"
                        | "PATH_OUTSIDE_WORKSPACE"
                        | "BATCH_SIDE_EFFECT_SKIPPED"
                        | "SUBAGENT_TOOL_DENIED"
                )
            }) {
                ToolCallStatus::Denied
            } else {
                ToolCallStatus::Failed
            };
            audit.push(ToolCallRecord {
                id: call.id.clone(),
                tool_name: call.name.clone(),
                input: call.input.clone(),
                output: result.output.clone(),
                error: result.error.as_ref().map(|error| error.message.clone()),
                status: record_status,
                duration_ms: Some(result.duration_ms),
                started_at: call_started,
                ended_at: Some(result.ended_at),
                approval: result.approval.as_deref().cloned(),
            });
            let content = if result.success {
                serde_json::to_string(&result.output.unwrap_or(Value::Null))
                    .unwrap_or_else(|_| "{}".to_owned())
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
            messages.push(ProviderMessage {
                role: MessageRole::Tool,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content,
                    is_error: !result.success,
                }]),
            });
        }
    }
    emit(
        context.event_sink,
        AgentEvent::SubagentFinished {
            agent_id: agent_id.to_owned(),
            result: ToolResult::failure(
                "SUBAGENT_INCOMPLETE",
                format!("子代理 {agent_id} 在 {} 轮后未完成。", profile.max_turns),
                started_at,
                json!({ "agent": agent_id }),
            ),
        },
    )
    .await;
    SubagentOutcome::failure(
        "SUBAGENT_INCOMPLETE",
        format!("子代理 {agent_id} 在 {} 轮后未完成。", profile.max_turns),
        started_at,
        json!({ "agent": agent_id }),
    )
}

/// 子代理系统提示词：档案定位 + 追加指令。
pub fn build_subagent_system(
    profile: &AgentProfile,
    cwd: &Path,
    instructions: &[String],
) -> String {
    let mut system = crate::prompt::build_system_prompt(&[], cwd);
    if !instructions.is_empty() {
        system.push_str("\n\n## 自定义指令\n\n");
        system.push_str(&instructions.join("\n"));
    }
    if let Some(extra) = &profile.system_extra {
        system.push_str("\n\n");
        system.push_str(extra);
    }
    system
}

/// 校验自定义档案（配置加载时使用）：id 合法、description 非空、max_turns 有界。
pub fn validate_profile(profile: &AgentProfile) -> XduduResult<()> {
    if profile.id.is_empty() || profile.id.len() > 64 {
        return Err(XduduError::validation("档案 id 必须是 1 到 64 字符。"));
    }
    if profile.description.trim().is_empty() {
        return Err(XduduError::validation(format!(
            "档案 {} 缺少 description。",
            profile.id
        )));
    }
    if !(1..=100).contains(&profile.max_turns) {
        return Err(XduduError::validation(format!(
            "档案 {} 的 max_turns 必须是 1 到 100。",
            profile.id
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use crate::{
        approval::AllowAllApprovalGate,
        changes::NoopChangeLedger,
        provider::{FinishReason, ProviderResponse, TokenUsage, ToolCall},
        tools::register_builtins,
    };
    use tempfile::tempdir;

    struct MockProvider {
        responses: Mutex<VecDeque<ProviderResponse>>,
    }

    #[async_trait::async_trait]
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
            usage: TokenUsage::default(),
            finish_reason: FinishReason::Stop,
            reasoning: None,
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
            reasoning: None,
        }
    }

    fn context<'a>(
        provider: &'a dyn Provider,
        registry: &'a ToolRegistry,
        cwd: &'a Path,
        profiles: &'a [AgentProfile],
    ) -> SubagentContext<'a> {
        SubagentContext {
            provider,
            model: "test".into(),
            registry,
            cwd,
            permission_mode: PermissionMode::AutoSafe,
            cancellation: CancellationToken::new(),
            event_sink: None,
            session_id: Uuid::new_v4(),
            temperature: 0.2,
            max_output_tokens: 4096,
            reasoning: false,
            profiles,
            instructions: Vec::new(),
        }
    }

    #[tokio::test]
    async fn 文本结果直接返回且无审计记录() {
        let dir = tempdir().unwrap();
        let mut registry =
            ToolRegistry::with_runtime(Arc::new(AllowAllApprovalGate), Arc::new(NoopChangeLedger));
        register_builtins(&mut registry).unwrap();
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([text_response("探索结果")])),
        };
        let profiles = builtin_profiles();
        let outcome = run_subagent(
            &context(&provider, &registry, dir.path(), &profiles),
            json!({ "agent": "explore", "prompt": "调查项目" }),
            Utc::now(),
        )
        .await;
        assert!(outcome.result.success, "{:?}", outcome.result.error);
        assert_eq!(outcome.result.output.unwrap()["result"], "探索结果");
        assert!(outcome.audit.is_empty());
    }

    #[tokio::test]
    async fn 工具调用结果落回隔离消息并生成审计() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let mut registry =
            ToolRegistry::with_runtime(Arc::new(AllowAllApprovalGate), Arc::new(NoopChangeLedger));
        register_builtins(&mut registry).unwrap();
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                tool_response(vec![ToolCall {
                    id: "call-1".into(),
                    name: "file_read".into(),
                    input: json!({ "path": "a.txt" }),
                }]),
                text_response("读取完成"),
            ])),
        };
        let profiles = builtin_profiles();
        let outcome = run_subagent(
            &context(&provider, &registry, dir.path(), &profiles),
            json!({ "agent": "explore", "prompt": "读取 a.txt" }),
            Utc::now(),
        )
        .await;
        assert!(outcome.result.success, "{:?}", outcome.result.error);
        assert_eq!(outcome.audit.len(), 1);
        assert_eq!(outcome.audit[0].tool_name, "file_read");
        assert_eq!(outcome.audit[0].status, ToolCallStatus::Succeeded);
    }

    #[tokio::test]
    async fn 异常provider响应不能绕过子代理工具白名单() {
        let dir = tempdir().unwrap();
        let mut registry =
            ToolRegistry::with_runtime(Arc::new(AllowAllApprovalGate), Arc::new(NoopChangeLedger));
        register_builtins(&mut registry).unwrap();
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                tool_response(vec![ToolCall {
                    id: "call-denied".into(),
                    name: "terminal_exec".into(),
                    input: json!({ "command": "echo", "args": ["blocked"] }),
                }]),
                text_response("已观察拒绝结果"),
            ])),
        };
        let profiles = builtin_profiles();
        let outcome = run_subagent(
            &context(&provider, &registry, dir.path(), &profiles),
            json!({ "agent": "explore", "prompt": "尝试越权工具" }),
            Utc::now(),
        )
        .await;
        assert!(outcome.result.success, "{:?}", outcome.result.error);
        assert_eq!(outcome.audit.len(), 1);
        assert_eq!(outcome.audit[0].tool_name, "terminal_exec");
        assert_eq!(outcome.audit[0].status, ToolCallStatus::Denied);
        assert!(
            outcome.audit[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("不允许调用"))
        );
    }

    #[tokio::test]
    async fn 超出轮数返回_未完成() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let mut registry =
            ToolRegistry::with_runtime(Arc::new(AllowAllApprovalGate), Arc::new(NoopChangeLedger));
        register_builtins(&mut registry).unwrap();
        // 只有工具调用响应：max_turns=1 时第二轮回合前退出。
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                tool_response(vec![ToolCall {
                    id: "call-1".into(),
                    name: "file_read".into(),
                    input: json!({ "path": "a.txt" }),
                }]),
                tool_response(vec![ToolCall {
                    id: "call-2".into(),
                    name: "file_read".into(),
                    input: json!({ "path": "a.txt" }),
                }]),
            ])),
        };
        let mut profiles = builtin_profiles();
        if let Some(explore) = profiles.iter_mut().find(|profile| profile.id == "explore") {
            explore.max_turns = 1;
        }
        let outcome = run_subagent(
            &context(&provider, &registry, dir.path(), &profiles),
            json!({ "agent": "explore", "prompt": "读取文件" }),
            Utc::now(),
        )
        .await;
        assert_eq!(outcome.result.error.unwrap().code, "SUBAGENT_INCOMPLETE");
    }

    #[tokio::test]
    async fn 未知档案与主代理档案被拒绝() {
        let dir = tempdir().unwrap();
        let mut registry =
            ToolRegistry::with_runtime(Arc::new(AllowAllApprovalGate), Arc::new(NoopChangeLedger));
        register_builtins(&mut registry).unwrap();
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::new()),
        };
        let profiles = builtin_profiles();
        let missing = run_subagent(
            &context(&provider, &registry, dir.path(), &profiles),
            json!({ "agent": "missing", "prompt": "调查" }),
            Utc::now(),
        )
        .await;
        assert_eq!(missing.result.error.unwrap().code, "TOOL_NOT_FOUND");
        // plan 是纯主代理，不可委派。
        let plan = run_subagent(
            &context(&provider, &registry, dir.path(), &profiles),
            json!({ "agent": "plan", "prompt": "规划" }),
            Utc::now(),
        )
        .await;
        assert_eq!(plan.result.error.unwrap().code, "INVALID_SUBAGENT");
    }

    #[test]
    fn task_定义只列可委派档案且_schema_严格() {
        let definition = task_tool_definition(&builtin_profiles());
        let schema = &definition.input_schema;
        assert_eq!(schema["required"], json!(["agent", "prompt"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        let ids = schema["properties"]["agent"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"explore"));
        assert!(ids.contains(&"general"));
        assert!(ids.contains(&"reviewer"));
        assert!(ids.contains(&"build"));
        assert!(!ids.contains(&"plan"));
        assert_eq!(schema["properties"]["prompt"]["maxLength"], 8192);
    }
}
