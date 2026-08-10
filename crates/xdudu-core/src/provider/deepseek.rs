//! DeepSeek 的 OpenAI Chat Completions 兼容适配器。
//!
//! 复用 [`super::openai_wire::OpenAiWire`] 的实现；与通用 `openai-compatible`
//! 的差异仅是 DeepSeek V4 系列在思考关闭时需显式发送 `thinking.disabled`。

use std::{env, time::Duration};

use async_trait::async_trait;

use super::{Provider, ProviderRequest, ProviderStreamSink, openai_wire::OpenAiWire};
use crate::error::{XduduError, XduduResult};

/// DeepSeek 的 OpenAI Chat Completions 兼容适配器。
pub struct DeepSeekProvider {
    wire: OpenAiWire,
}

impl DeepSeekProvider {
    pub fn from_env() -> XduduResult<Self> {
        let api_key = env::var("DEEPSEEK_API_KEY").map_err(|_| {
            XduduError::provider("DEEPSEEK_API_KEY 未设置。请先设置环境变量。", false)
        })?;
        let base_url =
            env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".to_owned());
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
            wire: OpenAiWire::new(api_key, base_url, timeout, "DeepSeek")?,
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
        OpenAiWire::parse_response(value, "DeepSeek")
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn name(&self) -> &'static str {
        "deepseek"
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
        let response = DeepSeekProvider::parse_response(json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call-2", "type": "function",
                    "function": {"name": "terminal_exec", "arguments": "{\"command\":\"pwd\"}"}
                }]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4}
        }))
        .unwrap();
        assert_eq!(response.tool_calls[0].input["command"], "pwd");
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn 推理字段不会进入公开助手文本() {
        let response = DeepSeekProvider::parse_response(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "reasoning_content": "这里是不能公开的原始推理",
                    "content": "这是最终结论"
                },
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        assert_eq!(response.message.text_content(), "这是最终结论");
        assert!(!response.message.text_content().contains("原始推理"));
        assert_eq!(
            response.reasoning.as_deref(),
            Some("这里是不能公开的原始推理")
        );
    }

    #[test]
    fn 消息会展开工具结果() {
        let request = ProviderRequest {
            session_id: "s".into(),
            model: "deepseek-chat".into(),
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
        let messages = DeepSeekProvider::openai_messages(&request).unwrap();
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call-1");
    }

    #[test]
    fn v4_请求默认关闭思考以兼容工具循环() {
        let request = ProviderRequest {
            session_id: "s".into(),
            model: "deepseek-v4-pro".into(),
            messages: vec![],
            tools: vec![],
            system: "system".into(),
            temperature: 0.2,
            max_output_tokens: 100,
            reasoning: false,
            cancellation: CancellationToken::new(),
        };
        let body = DeepSeekProvider::request_body(&request).unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn v4_思考开启时不发送thinking_disabled() {
        let request = ProviderRequest {
            session_id: "s".into(),
            model: "deepseek-v4-pro".into(),
            messages: vec![],
            tools: vec![],
            system: "system".into(),
            temperature: 0.2,
            max_output_tokens: 100,
            reasoning: true,
            cancellation: CancellationToken::new(),
        };
        let body = DeepSeekProvider::request_body(&request).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "思考开启时不应发送 thinking disabled"
        );
    }
}
