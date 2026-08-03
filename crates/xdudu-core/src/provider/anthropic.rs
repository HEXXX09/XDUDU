//! Anthropic Messages API 适配器。

use std::{collections::BTreeMap, env, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};

use super::{
    ContentBlock, FinishReason, MessageContent, MessageRole, Provider, ProviderMessage,
    ProviderRequest, ProviderResponse, ProviderStreamEvent, ProviderStreamSink, TokenUsage,
    ToolCall, http_client, parse_http_response,
};
use crate::error::{XduduError, XduduResult};

pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    client: Client,
}

impl AnthropicProvider {
    pub fn from_env() -> XduduResult<Self> {
        let api_key = env::var("ANTHROPIC_API_KEY").map_err(|_| {
            XduduError::provider("ANTHROPIC_API_KEY 未设置。请先设置环境变量。", false)
        })?;
        let base_url = env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_owned());
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
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: http_client(timeout)?,
        })
    }

    fn request_body(request: &ProviderRequest) -> Value {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .filter(|message| message.role != MessageRole::System)
            .map(|message| json!({ "role": message.role, "content": message.content }))
            .collect();
        json!({
            "model": request.model,
            "max_tokens": request.max_output_tokens,
            "temperature": request.temperature,
            "system": request.system,
            "messages": messages,
            "tools": request.tools,
        })
    }

    fn parse_response(value: Value) -> XduduResult<ProviderResponse> {
        let blocks = value
            .get("content")
            .cloned()
            .ok_or_else(|| XduduError::provider("Anthropic 响应缺少 content。", false))?;
        let public_blocks = blocks
            .as_array()
            .ok_or_else(|| XduduError::provider("Anthropic content 必须是数组。", false))?
            .iter()
            .filter(|block| {
                !matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("thinking" | "redacted_thinking")
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let content: Vec<ContentBlock> = serde_json::from_value(Value::Array(public_blocks))
            .map_err(|error| {
                XduduError::provider(format!("Anthropic content 格式无效：{error}"), false)
            })?;
        let tool_calls = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        let finish_reason = match value.get("stop_reason").and_then(Value::as_str) {
            Some("tool_use") => FinishReason::ToolCalls,
            Some("max_tokens") => FinishReason::Length,
            _ => FinishReason::Stop,
        };
        let usage = TokenUsage {
            input_tokens: value
                .pointer("/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: value
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_read_tokens: value
                .pointer("/usage/cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_write_tokens: value
                .pointer("/usage/cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        };
        Ok(ProviderResponse {
            message: ProviderMessage {
                role: MessageRole::Assistant,
                content: MessageContent::Blocks(content),
            },
            tool_calls,
            usage,
            finish_reason,
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn chat(&self, request: ProviderRequest) -> XduduResult<ProviderResponse> {
        let send = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&Self::request_body(&request))
            .send();
        let response = tokio::select! {
            _ = request.cancellation.cancelled() => return Err(XduduError::provider("Anthropic 请求已中断。", false)),
            response = send => response.map_err(|error| XduduError::provider(format!("Anthropic 请求失败：{error}"), error.is_timeout() || error.is_connect()))?,
        };
        Self::parse_response(parse_http_response(response, "Anthropic").await?)
    }

    async fn stream_chat(
        &self,
        request: ProviderRequest,
        sink: &dyn ProviderStreamSink,
    ) -> XduduResult<ProviderResponse> {
        let mut body = Self::request_body(&request);
        body["stream"] = Value::Bool(true);
        let send = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send();
        let response = tokio::select! {
            _ = request.cancellation.cancelled() => return Err(XduduError::provider("Anthropic 请求已中断。", false)),
            response = send => response.map_err(|error| XduduError::provider(format!("Anthropic 请求失败：{error}"), error.is_timeout() || error.is_connect()))?,
        };
        if !response.status().is_success() {
            return Err(parse_http_response(response, "Anthropic")
                .await
                .unwrap_err());
        }

        #[derive(Debug)]
        enum Block {
            Text(String),
            Tool {
                id: String,
                name: String,
                arguments: String,
            },
        }

        let mut decoder = super::stream::SseDecoder::default();
        let mut chunks = response.bytes_stream();
        let mut blocks = BTreeMap::<usize, Block>::new();
        let mut usage = TokenUsage::default();
        let mut finish_reason = FinishReason::Stop;
        let mut saw_stop = false;
        let mut emitted = false;

        while let Some(chunk) = tokio::select! {
            _ = request.cancellation.cancelled() => return Err(XduduError::provider("Anthropic 流已中断。", false)),
            chunk = chunks.next() => chunk,
        } {
            let chunk = chunk.map_err(|error| {
                XduduError::provider(format!("读取 Anthropic 流失败：{error}"), !emitted)
            })?;
            for data in decoder.push(&chunk)? {
                if data == "[DONE]" {
                    saw_stop = true;
                    continue;
                }
                let value: Value = serde_json::from_str(&data).map_err(|error| {
                    XduduError::provider(format!("Anthropic 流事件不是有效 JSON：{error}"), false)
                })?;
                match value.get("type").and_then(Value::as_str) {
                    Some("message_start") => {
                        usage.input_tokens = value
                            .pointer("/message/usage/input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                    }
                    Some("content_block_start") => {
                        let index =
                            value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let block = value.get("content_block").ok_or_else(|| {
                            XduduError::provider("Anthropic 流缺少 content_block。", false)
                        })?;
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                let text = block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned();
                                if !text.is_empty() {
                                    emitted = true;
                                    sink.emit(ProviderStreamEvent::TextDelta {
                                        text: text.clone(),
                                    })
                                    .await;
                                }
                                blocks.insert(index, Block::Text(text));
                            }
                            Some("tool_use") => {
                                blocks.insert(
                                    index,
                                    Block::Tool {
                                        id: block
                                            .get("id")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .to_owned(),
                                        name: block
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .to_owned(),
                                        arguments: String::new(),
                                    },
                                );
                            }
                            _ => {}
                        }
                    }
                    Some("content_block_delta") => {
                        let index =
                            value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        match value.pointer("/delta/type").and_then(Value::as_str) {
                            Some("text_delta") => {
                                let text = value
                                    .pointer("/delta/text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                if let Some(Block::Text(content)) = blocks.get_mut(&index) {
                                    content.push_str(text);
                                }
                                if !text.is_empty() {
                                    emitted = true;
                                    sink.emit(ProviderStreamEvent::TextDelta {
                                        text: text.to_owned(),
                                    })
                                    .await;
                                }
                            }
                            Some("input_json_delta") => {
                                let partial = value
                                    .pointer("/delta/partial_json")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                if let Some(Block::Tool { arguments, .. }) = blocks.get_mut(&index)
                                {
                                    arguments.push_str(partial);
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("message_delta") => {
                        finish_reason = match value
                            .pointer("/delta/stop_reason")
                            .and_then(Value::as_str)
                        {
                            Some("tool_use") => FinishReason::ToolCalls,
                            Some("max_tokens") => FinishReason::Length,
                            Some("end_turn") | Some("stop_sequence") | None => FinishReason::Stop,
                            _ => FinishReason::Error,
                        };
                        usage.output_tokens = value
                            .pointer("/usage/output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(usage.output_tokens);
                    }
                    Some("message_stop") => saw_stop = true,
                    Some("error") => {
                        let message = value
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("未知流式错误");
                        let retryable = value.pointer("/error/type").and_then(Value::as_str)
                            == Some("overloaded_error")
                            && !emitted;
                        return Err(XduduError::provider(
                            format!("Anthropic 流错误：{message}"),
                            retryable,
                        ));
                    }
                    _ => {}
                }
            }
        }
        for data in decoder.finish()? {
            if data == "[DONE]" {
                saw_stop = true;
            }
        }
        if !saw_stop {
            return Err(XduduError::provider(
                "Anthropic 流在 message_stop 前结束。",
                !emitted,
            ));
        }

        let mut content = Vec::new();
        let mut tool_calls = Vec::new();
        for block in blocks.into_values() {
            match block {
                Block::Text(text) => content.push(ContentBlock::Text { text }),
                Block::Tool {
                    id,
                    name,
                    arguments,
                } => {
                    let input: Value = serde_json::from_str(if arguments.is_empty() {
                        "{}"
                    } else {
                        &arguments
                    })
                    .map_err(|error| {
                        XduduError::provider(
                            format!("Anthropic 流式工具参数不是有效 JSON：{error}"),
                            false,
                        )
                    })?;
                    content.push(ContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                    tool_calls.push(ToolCall { id, name, input });
                }
            }
        }
        Ok(ProviderResponse {
            message: ProviderMessage {
                role: MessageRole::Assistant,
                content: MessageContent::Blocks(content),
            },
            tool_calls,
            usage,
            finish_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析工具调用() {
        let response = AnthropicProvider::parse_response(json!({
            "content": [
                {"type": "text", "text": "先读取文件"},
                {"type": "tool_use", "id": "call-1", "name": "file_read", "input": {"path": "README.md"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        }))
        .unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(response.tool_calls[0].name, "file_read");
        assert_eq!(response.usage.input_tokens, 10);
    }

    #[test]
    fn thinking_内容块不会进入公开助手文本() {
        let response = AnthropicProvider::parse_response(json!({
            "content": [
                {"type":"thinking","thinking":"这里是不能公开的原始推理","signature":"sig"},
                {"type":"redacted_thinking","data":"opaque"},
                {"type":"text","text":"这是最终结论"}
            ],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":2}
        }))
        .unwrap();
        assert_eq!(response.message.text_content(), "这是最终结论");
        assert!(!response.message.text_content().contains("原始推理"));
    }
}
