//! 模型 Provider 抽象、公共领域类型与 HTTP 边界。

mod anthropic;
mod deepseek;
mod factory;
mod openai_compatible;
mod openai_wire;
mod retry;
mod stream;

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, XduduError, XduduResult};

pub use anthropic::AnthropicProvider;
pub use deepseek::DeepSeekProvider;
pub use factory::{DefaultProviderFactory, ProviderFactory};
pub use openai_compatible::OpenAiCompatibleProvider;
pub use retry::RetryingProvider;
pub use stream::{ProviderStreamEvent, ProviderStreamSink};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderMessage {
    pub role: MessageRole,
    pub content: MessageContent,
}

impl ProviderMessage {
    pub fn text(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: MessageContent::Text(content.into()),
        }
    }

    pub fn text_content(&self) -> String {
        match &self.content {
            MessageContent::Text(text) => text.clone(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub session_id: String,
    pub model: String,
    pub messages: Vec<ProviderMessage>,
    pub tools: Vec<ProviderToolDefinition>,
    pub system: String,
    pub temperature: f32,
    pub max_output_tokens: u32,
    /// 是否启用内部思考闭环（reasoning_content 解析、持久化与回传）。
    pub reasoning: bool,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Error,
}

#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub message: ProviderMessage,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub finish_reason: FinishReason,
    /// 内部推理内容（仅思考路径启用时填充），不进入公开文本。
    pub reasoning: Option<String>,
}

impl ProviderResponse {
    /// 兼容构造：无内部推理。
    pub fn with_reasoning(mut self, reasoning: Option<String>) -> Self {
        self.reasoning = reasoning;
        self
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn chat(&self, request: ProviderRequest) -> XduduResult<ProviderResponse>;
    async fn stream_chat(
        &self,
        request: ProviderRequest,
        sink: &dyn ProviderStreamSink,
    ) -> XduduResult<ProviderResponse> {
        let response = self.chat(request).await?;
        let text = response.message.text_content();
        if !text.is_empty() {
            sink.emit(ProviderStreamEvent::TextDelta { text }).await;
        }
        Ok(response)
    }
    fn supports_tools(&self, _model: &str) -> bool {
        true
    }
}

pub(super) fn http_client(timeout: Duration) -> XduduResult<Client> {
    Client::builder()
        .timeout(timeout)
        .user_agent(concat!("xdudu/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| XduduError::provider(format!("无法创建 HTTP 客户端：{error}"), false))
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429 | 500 | 502 | 503 | 504)
}

pub(super) async fn parse_http_response(
    response: reqwest::Response,
    provider: &str,
) -> XduduResult<Value> {
    let status = response.status();
    let request_id = response
        .headers()
        .get("request-id")
        .or_else(|| response.headers().get("x-request-id"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let retry_after_ms = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1000));
    let text = response.text().await.map_err(|error| {
        XduduError::provider(format!("读取 {provider} 响应失败：{error}"), true)
    })?;

    if !status.is_success() {
        let remote_message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| text.chars().take(500).collect());
        return Err(XduduError {
            kind: ErrorKind::ProviderError,
            message: format!(
                "{provider} API 错误（HTTP {}）：{remote_message}",
                status.as_u16()
            ),
            retryable: retryable_status(status),
            details: json!({
                "status": status.as_u16(),
                "requestId": request_id,
                "retryAfterMs": retry_after_ms,
            }),
        });
    }

    serde_json::from_str(&text).map_err(|error| {
        XduduError::provider(format!("{provider} 返回了无效 JSON：{error}"), false)
    })
}
