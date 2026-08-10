//! 通用 OpenAI Chat Completions 兼容 Provider。
//!
//! 通过 XDUDU 配置中的 `provider.base_url` / `provider.api_key` 指向任意
//! OpenAI Chat Completions 兼容服务（DeepSeek、Moonshot、OpenAI 网关等）。
//! 复用 [`super::openai_wire::OpenAiWire`] 的完整实现。

use std::{env, time::Duration};

use async_trait::async_trait;

use super::{Provider, ProviderRequest, ProviderStreamSink, openai_wire::OpenAiWire};
use crate::error::{XduduError, XduduResult};

/// 通用 OpenAI Chat Completions 兼容 Provider。
pub struct OpenAiCompatibleProvider {
    wire: OpenAiWire,
}

impl OpenAiCompatibleProvider {
    pub fn from_env() -> XduduResult<Self> {
        let api_key = env::var("OPENAI_API_KEY").map_err(|_| {
            XduduError::provider("OPENAI_API_KEY 未设置。请先设置环境变量。", false)
        })?;
        let base_url =
            env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_owned());
        Self::new(api_key, base_url)
    }

    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> XduduResult<Self> {
        Self::with_timeout(api_key, base_url, Duration::from_secs(180))
    }

    pub fn with_timeout(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        timeout: Duration,
    ) -> XduduResult<Self> {
        Ok(Self {
            wire: OpenAiWire::new(api_key, base_url, timeout, "OpenAI 兼容")?,
        })
    }

    #[cfg(test)]
    fn openai_messages(request: &ProviderRequest) -> XduduResult<Vec<serde_json::Value>> {
        OpenAiWire::openai_messages(request)
    }

    #[cfg(test)]
    fn request_body(request: &ProviderRequest) -> XduduResult<serde_json::Value> {
        OpenAiWire::request_body(request)
    }

    #[cfg(test)]
    fn parse_response(value: serde_json::Value) -> XduduResult<super::ProviderResponse> {
        OpenAiWire::parse_response(value, "OpenAI 兼容")
    }
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn name(&self) -> &'static str {
        "openai-compatible"
    }

    async fn chat(&self, request: ProviderRequest) -> XduduResult<super::ProviderResponse> {
        self.wire.chat(request).await
    }

    async fn stream_chat(
        &self,
        request: ProviderRequest,
        sink: &dyn ProviderStreamSink,
    ) -> XduduResult<super::ProviderResponse> {
        self.wire.stream_chat(request, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        ContentBlock, FinishReason, MessageContent, MessageRole, ProviderMessage,
    };
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn 解析工具调用() {
        let response = OpenAiCompatibleProvider::parse_response(json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call-9", "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}
                }]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 6}
        }))
        .unwrap();
        assert_eq!(response.tool_calls[0].input["path"], "a");
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn 解析推理字段() {
        let response = OpenAiCompatibleProvider::parse_response(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "reasoning_content": "内部推理",
                    "content": "结论"
                },
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        assert_eq!(response.message.text_content(), "结论");
        assert_eq!(response.reasoning.as_deref(), Some("内部推理"));
    }

    #[test]
    fn 非v4模型不发送thinking_disabled() {
        let request = ProviderRequest {
            session_id: "s".into(),
            model: "gpt-5".into(),
            messages: vec![],
            tools: vec![],
            system: "system".into(),
            temperature: 0.2,
            max_output_tokens: 100,
            reasoning: false,
            cancellation: CancellationToken::new(),
        };
        let body = OpenAiCompatibleProvider::request_body(&request).unwrap();
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn 消息会展开工具结果() {
        let request = ProviderRequest {
            session_id: "s".into(),
            model: "gpt-5".into(),
            messages: vec![ProviderMessage {
                role: MessageRole::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "ok".into(),
                    is_error: false,
                }]),
            }],
            tools: vec![],
            system: "system".into(),
            temperature: 0.2,
            max_output_tokens: 100,
            reasoning: false,
            cancellation: CancellationToken::new(),
        };
        let messages = OpenAiCompatibleProvider::openai_messages(&request).unwrap();
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call-1");
    }
}
