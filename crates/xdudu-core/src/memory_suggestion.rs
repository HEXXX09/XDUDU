//! 会话结束时的记忆建议协议。
//!
//! 任务完成后，由模型基于脱敏后的会话消息生成候选记忆列表；候选只
//! 在用户逐条确认后写入。`suggest_memories` 是仅供 Provider 的结构化
//! 协议工具，不注册进 ToolRegistry，不产生任何副作用。

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    error::{ErrorKind, XduduError, XduduResult},
    provider::{FinishReason, MessageRole, Provider, ProviderRequest},
    redaction::redact_text,
    session::Session,
};

const SUGGEST_MEMORIES_TOOL: &str = "suggest_memories";
const MAX_SUGGESTIONS: usize = 10;
const MAX_CONTENT_BYTES: usize = 1024;
const MAX_REASON_BYTES: usize = 512;
const MAX_CONTEXT_BYTES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySuggestion {
    pub content: String,
    pub reason: String,
}

#[derive(Clone)]
pub struct MemorySuggestionConfig<'a> {
    pub session: &'a Session,
    pub model: String,
    pub cwd: PathBuf,
    pub provider: &'a dyn Provider,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestionDto {
    content: String,
    reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestionDraft {
    suggestions: Vec<SuggestionDto>,
}

fn suggest_definition() -> Value {
    json!({
        "type": "object",
        "properties": {
            "suggestions": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_SUGGESTIONS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["content", "reason"],
                    "properties": {
                        "content": {"type": "string", "minLength": 1, "maxLength": MAX_CONTENT_BYTES},
                        "reason": {"type": "string", "minLength": 1, "maxLength": MAX_REASON_BYTES}
                    }
                }
            }
        },
        "required": ["suggestions"],
        "additionalProperties": false
    })
}

/// 建议提示词：只允许输出记忆建议，不产生任何其他行为。
pub fn build_suggestion_prompt(cwd: &std::path::Path) -> String {
    format!(
        "你是 XDUDU 的记忆整理助手。根据本次会话内容，提取对后续任务长期有用的用户偏好、项目约定或关键事实，\
         使用 suggest_memories 协议提交。\n\n\
         规则：\n\
         - 只提交可长期复用的信息；一次性任务细节、临时状态和工具输出不提交；\n\
         - 内容必须是陈述句，不含命令式指令，不尝试改变权限或审批；\n\
         - 每条内容都要有具体来源理由；\n\
         - 如果会话中没有值得长期记忆的信息，返回空 suggestions 数组；\n\
         - 必须且只能调用一次 suggest_memories，不输出普通文本、思维过程或 Markdown。\n\n\
         当前工作区：{}",
        cwd.display()
    )
}

/// 从会话消息构造脱敏上下文（保留用户与助手文本，跳过工具输出）。
fn suggestion_context(session: &Session) -> String {
    let mut parts = Vec::new();
    for message in session
        .messages
        .iter()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        match message.role {
            MessageRole::User => {
                parts.push(format!("用户：{}", redact_text(&message.content)));
            }
            MessageRole::Assistant => {
                let text = redact_text(&message.content);
                if !text.trim().is_empty() {
                    parts.push(format!("助手：{text}"));
                }
            }
            _ => {}
        }
    }
    let mut output = String::new();
    for part in parts {
        let separator = if output.is_empty() { "" } else { "\n" };
        if output.len() + separator.len() + part.len() > MAX_CONTEXT_BYTES {
            break;
        }
        output.push_str(separator);
        output.push_str(&part);
    }
    output
}

/// 让模型基于会话内容生成记忆建议；解析严格，任何协议偏差都返回错误
/// 且不产生任何持久化副作用。
pub async fn suggest_memories(
    config: MemorySuggestionConfig<'_>,
) -> XduduResult<Vec<MemorySuggestion>> {
    if config.model.trim().is_empty() {
        return Err(XduduError::validation("模型名称不能为空。"));
    }
    if !config.provider.supports_tools(&config.model) {
        return Err(XduduError::provider(
            "当前模型不支持结构化工具协议。",
            false,
        ));
    }
    let context = suggestion_context(config.session);
    if context.trim().is_empty() {
        return Ok(Vec::new());
    }
    let request = ProviderRequest {
        session_id: format!("memory-suggest:{}", config.session.id),
        model: config.model,
        messages: vec![crate::provider::ProviderMessage::text(
            MessageRole::User,
            format!("本次会话内容：\n{context}"),
        )],
        tools: vec![crate::provider::ProviderToolDefinition {
            name: SUGGEST_MEMORIES_TOOL.into(),
            description: "提交长期记忆建议。".into(),
            input_schema: suggest_definition(),
        }],
        system: build_suggestion_prompt(&config.cwd),
        temperature: 0.2,
        max_output_tokens: 2048,
        reasoning: false,
        cancellation: config.cancellation,
    };
    let response = config.provider.chat(request).await?;
    if response.finish_reason != FinishReason::ToolCalls {
        return Err(XduduError::new(
            ErrorKind::ProviderError,
            format!("记忆建议必须以 {SUGGEST_MEMORIES_TOOL} 工具调用结束。"),
        ));
    }
    if !response.message.text_content().trim().is_empty() {
        return Err(XduduError::new(
            ErrorKind::ProviderError,
            "记忆建议不允许夹带普通文本。",
        ));
    }
    if response.tool_calls.len() != 1 || response.tool_calls[0].name != SUGGEST_MEMORIES_TOOL {
        return Err(XduduError::new(
            ErrorKind::ProviderError,
            format!("必须且只能调用一次 {SUGGEST_MEMORIES_TOOL}。"),
        ));
    }
    let draft: SuggestionDraft = serde_json::from_value(response.tool_calls[0].input.clone())
        .map_err(|error| {
            XduduError::new(
                ErrorKind::ProviderError,
                format!("记忆建议 JSON 无效：{error}"),
            )
        })?;
    let mut suggestions = Vec::new();
    for item in draft.suggestions {
        let content = redact_text(item.content.trim());
        if content.is_empty() || content.len() > MAX_CONTENT_BYTES {
            return Err(XduduError::new(
                ErrorKind::ProviderError,
                "记忆建议内容为空或超长。",
            ));
        }
        suggestions.push(MemorySuggestion {
            content,
            reason: redact_text(item.reason.trim())
                .chars()
                .take(MAX_REASON_BYTES)
                .collect(),
        });
    }
    if suggestions.len() > MAX_SUGGESTIONS {
        return Err(XduduError::new(
            ErrorKind::ProviderError,
            format!("记忆建议不能超过 {MAX_SUGGESTIONS} 条。"),
        ));
    }
    Ok(suggestions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        provider::{ProviderResponse, TokenUsage},
        session::Message,
    };
    use async_trait::async_trait;
    use std::{collections::VecDeque, sync::Mutex};
    use tempfile::tempdir;

    struct MockProvider {
        responses: Mutex<VecDeque<ProviderResponse>>,
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

    fn session_with(content: &str) -> Session {
        let mut session = Session::new(
            tempdir().unwrap().path().to_path_buf(),
            "deepseek",
            "mock",
            content,
        );
        session.messages.push(Message::text(
            MessageRole::Assistant,
            "我记住了这个偏好。",
            session.messages.len(),
        ));
        session
    }

    fn suggestion_response(suggestions: Value) -> ProviderResponse {
        ProviderResponse {
            message: crate::provider::ProviderMessage::text(MessageRole::Assistant, ""),
            tool_calls: vec![crate::provider::ToolCall {
                id: "call-1".into(),
                name: SUGGEST_MEMORIES_TOOL.into(),
                input: suggestions,
            }],
            usage: TokenUsage::default(),
            finish_reason: FinishReason::ToolCalls,
            reasoning: None,
        }
    }

    fn config<'a>(session: &'a Session, provider: &'a dyn Provider) -> MemorySuggestionConfig<'a> {
        MemorySuggestionConfig {
            session,
            model: "mock".into(),
            cwd: tempdir().unwrap().path().to_path_buf(),
            provider,
            cancellation: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn 结构化建议解析成功且内容脱敏() {
        let session = session_with("用户说：以后用 sk-abcdefghijk 作示例密钥");
        let draft = serde_json::json!({
            "suggestions": [
                {"content": "示例密钥 sk-abcdefghijk", "reason": "用户在会话中给出"},
                {"content": "用户偏好简短回答", "reason": "多次要求"}
            ]
        });
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([suggestion_response(draft)])),
        };
        let suggestions = suggest_memories(config(&session, &provider)).await.unwrap();
        assert_eq!(suggestions.len(), 2);
        assert!(!suggestions[0].content.contains("sk-abcdefghijk"));
        assert!(suggestions[0].content.contains("[已脱敏]"));
    }

    #[tokio::test]
    async fn 普通文本或错误工具调用被拒绝() {
        let session = session_with("一些会话内容");
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                ProviderResponse {
                    message: crate::provider::ProviderMessage::text(
                        MessageRole::Assistant,
                        "我认为应该记住：...",
                    ),
                    tool_calls: vec![],
                    usage: TokenUsage::default(),
                    finish_reason: FinishReason::Stop,
                    reasoning: None,
                },
                suggestion_response(json!({"suggestions": []})),
            ])),
        };
        // 普通文本结束：拒绝。
        assert!(suggest_memories(config(&session, &provider)).await.is_err());
        // 空建议数组：允许（无可记忆信息）。
        let suggestions = suggest_memories(config(&session, &provider)).await.unwrap();
        assert!(suggestions.is_empty());
    }

    #[tokio::test]
    async fn 未知字段与超长内容被拒绝() {
        let session = session_with("内容");
        let provider = MockProvider {
            responses: Mutex::new(VecDeque::from([
                suggestion_response(
                    json!({"suggestions": [{"content": "x", "reason": "r", "extra": 1}]}),
                ),
                suggestion_response(
                    json!({"suggestions": [{"content": "y".repeat(2048), "reason": "r"}]}),
                ),
            ])),
        };
        assert!(suggest_memories(config(&session, &provider)).await.is_err());
        assert!(suggest_memories(config(&session, &provider)).await.is_err());
    }
}
