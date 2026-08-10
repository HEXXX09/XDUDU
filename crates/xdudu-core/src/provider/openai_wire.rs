//! 通用的 OpenAI Chat Completions 兼容 wire 适配器。
//!
//! DeepSeek 与 `openai-compatible` 复用同一实现；唯一差异是
//! DeepSeek V4 系列模型在「思考关闭」时需显式发送 `thinking.disabled`。

use std::{collections::BTreeMap, time::Duration};

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

/// 该 provider 是否需要为思考关闭发送 `thinking.disabled`。
/// DeepSeek V4 返回 `true`；通用 `openai-compatible` 返回 `false`。
fn v4_thinking_guard(model: &str) -> bool {
    matches!(model, "deepseek-v4-flash" | "deepseek-v4-pro")
}

pub struct OpenAiWire {
    pub api_key: String,
    pub base_url: String,
    pub client: Client,
    /// 错误信息与请求日志中展示的名称。
    pub display_name: &'static str,
}

impl OpenAiWire {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        timeout: Duration,
        display_name: &'static str,
    ) -> XduduResult<Self> {
        Ok(Self {
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: http_client(timeout)?,
            display_name,
        })
    }

    pub fn openai_messages(request: &ProviderRequest) -> XduduResult<Vec<Value>> {
        let mut output = vec![json!({ "role": "system", "content": request.system })];
        for message in &request.messages {
            match &message.content {
                MessageContent::Text(content) => output.push(json!({
                    "role": message.role,
                    "content": content,
                })),
                MessageContent::Blocks(blocks) => {
                    let mut text = String::new();
                    let mut reasoning = String::new();
                    let mut tool_calls = Vec::new();
                    let mut tool_results = Vec::new();
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text: block_text } => text.push_str(block_text),
                            ContentBlock::Thinking { text: thinking } => {
                                reasoning.push_str(thinking)
                            }
                            ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": serde_json::to_string(input)? }
                            })),
                            ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } => {
                                tool_results.push(json!({
                                    "role": "tool",
                                    "tool_call_id": tool_use_id,
                                    "content": content,
                                }));
                            }
                        }
                    }
                    if !tool_calls.is_empty() {
                        let mut assistant = json!({
                            "role": "assistant",
                            "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                            "tool_calls": tool_calls,
                        });
                        if !reasoning.is_empty() {
                            assistant["reasoning_content"] = Value::String(reasoning);
                        }
                        output.push(assistant);
                    } else if !tool_results.is_empty() {
                        output.extend(tool_results);
                    } else if !reasoning.is_empty() {
                        // 仅含内部推理的助手消息：回传 reasoning_content 维持思考闭环。
                        output.push(json!({
                            "role": "assistant",
                            "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                            "reasoning_content": reasoning,
                        }));
                    } else {
                        output.push(json!({ "role": message.role, "content": text }));
                    }
                }
            }
        }
        Ok(output)
    }

    pub fn request_body(request: &ProviderRequest) -> XduduResult<Value> {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect::<Vec<_>>();
        let mut body = json!({
            "model": request.model,
            "messages": Self::openai_messages(request)?,
            "tools": tools,
            "temperature": request.temperature,
            "max_tokens": request.max_output_tokens,
        });
        if !request.reasoning && v4_thinking_guard(&request.model) {
            // V4 默认启用思考模式。XDUDU 公开边界不保存原始思维链，
            // 思考关闭时显式禁用，避免工具调用后无法回传 reasoning_content。
            body["thinking"] = json!({ "type": "disabled" });
        }
        if request.reasoning {
            // 思考开启：不发送 thinking.disabled，交由模型返回 reasoning_content。
        }
        Ok(body)
    }

    pub fn parse_response(value: Value, display_name: &str) -> XduduResult<ProviderResponse> {
        let choice = value.pointer("/choices/0").ok_or_else(|| {
            XduduError::provider(format!("{display_name} 响应缺少 choices[0]。"), false)
        })?;
        let message = choice.get("message").ok_or_else(|| {
            XduduError::provider(format!("{display_name} 响应缺少 message。"), false)
        })?;
        let mut blocks = Vec::new();
        if let Some(content) = message.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            blocks.push(ContentBlock::Text {
                text: content.to_owned(),
            });
        }
        let reasoning = message
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        let mut tool_calls = Vec::new();
        for call in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(arguments).map_err(|error| {
                XduduError::provider(
                    format!("{display_name} 工具参数不是有效 JSON：{error}"),
                    false,
                )
            })?;
            blocks.push(ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
            tool_calls.push(ToolCall { id, name, input });
        }
        let finish_reason = match choice.get("finish_reason").and_then(Value::as_str) {
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            Some("stop") | None => FinishReason::Stop,
            _ => FinishReason::Error,
        };
        let usage = TokenUsage {
            input_tokens: value
                .pointer("/usage/prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: value
                .pointer("/usage/completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            ..TokenUsage::default()
        };
        let content = if blocks.is_empty() {
            MessageContent::Text(String::new())
        } else {
            MessageContent::Blocks(blocks)
        };
        Ok(ProviderResponse {
            message: ProviderMessage {
                role: MessageRole::Assistant,
                content,
            },
            tool_calls,
            usage,
            finish_reason,
            reasoning,
        })
    }

    pub async fn chat(&self, request: ProviderRequest) -> XduduResult<ProviderResponse> {
        let body = Self::request_body(&request)?;
        let send = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();
        let response = tokio::select! {
            _ = request.cancellation.cancelled() => return Err(XduduError::provider(format!("{} 请求已中断。", self.display_name), false)),
            response = send => response.map_err(|error| XduduError::provider(format!("{} 请求失败：{error}", self.display_name), error.is_timeout() || error.is_connect()))?,
        };
        Self::parse_response(
            parse_http_response(response, self.display_name).await?,
            self.display_name,
        )
    }

    pub async fn stream_chat(
        &self,
        request: ProviderRequest,
        sink: &dyn ProviderStreamSink,
    ) -> XduduResult<ProviderResponse> {
        let mut body = Self::request_body(&request)?;
        body["stream"] = Value::Bool(true);
        body["stream_options"] = json!({ "include_usage": true });
        let send = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();
        let response = tokio::select! {
            _ = request.cancellation.cancelled() => return Err(XduduError::provider(format!("{} 请求已中断。", self.display_name), false)),
            response = send => response.map_err(|error| XduduError::provider(format!("{} 请求失败：{error}", self.display_name), error.is_timeout() || error.is_connect()))?,
        };
        if !response.status().is_success() {
            return Err(parse_http_response(response, self.display_name)
                .await
                .unwrap_err());
        }

        #[derive(Debug, Default)]
        struct ToolAccumulator {
            id: String,
            name: String,
            arguments: String,
        }

        let mut decoder = super::stream::SseDecoder::default();
        let mut chunks = response.bytes_stream();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tools = BTreeMap::<usize, ToolAccumulator>::new();
        let mut usage = TokenUsage::default();
        let mut finish_reason = FinishReason::Stop;
        let mut saw_done = false;
        let mut emitted = false;

        while let Some(chunk) = tokio::select! {
            _ = request.cancellation.cancelled() => return Err(XduduError::provider(format!("{} 流已中断。", self.display_name), false)),
            chunk = chunks.next() => chunk,
        } {
            let chunk = chunk.map_err(|error| {
                XduduError::provider(
                    format!("读取 {} 流失败：{error}", self.display_name),
                    !emitted,
                )
            })?;
            for data in decoder.push(&chunk)? {
                if data == "[DONE]" {
                    saw_done = true;
                    continue;
                }
                let value: Value = serde_json::from_str(&data).map_err(|error| {
                    XduduError::provider(
                        format!("{} 流事件不是有效 JSON：{error}", self.display_name),
                        false,
                    )
                })?;
                if let Some(input_tokens) = value
                    .pointer("/usage/prompt_tokens")
                    .and_then(Value::as_u64)
                {
                    usage.input_tokens = input_tokens;
                }
                if let Some(output_tokens) = value
                    .pointer("/usage/completion_tokens")
                    .and_then(Value::as_u64)
                {
                    usage.output_tokens = output_tokens;
                }
                let Some(choice) = value.pointer("/choices/0") else {
                    continue;
                };
                if let Some(delta) = choice.pointer("/delta/content").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        emitted = true;
                        text.push_str(delta);
                        sink.emit(ProviderStreamEvent::TextDelta {
                            text: delta.to_owned(),
                        })
                        .await;
                    }
                }
                if let Some(delta) = choice
                    .pointer("/delta/reasoning_content")
                    .and_then(Value::as_str)
                {
                    if !delta.is_empty() {
                        reasoning.push_str(delta);
                        sink.emit(ProviderStreamEvent::ReasoningDelta {
                            text: delta.to_owned(),
                        })
                        .await;
                    }
                }
                for call in choice
                    .pointer("/delta/tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let accumulator = tools.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str)
                        && accumulator.id.is_empty()
                    {
                        accumulator.id.push_str(id);
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        accumulator.name.push_str(name);
                    }
                    if let Some(arguments) =
                        call.pointer("/function/arguments").and_then(Value::as_str)
                    {
                        accumulator.arguments.push_str(arguments);
                    }
                }
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    finish_reason = match reason {
                        "tool_calls" => FinishReason::ToolCalls,
                        "length" => FinishReason::Length,
                        "content_filter" => FinishReason::ContentFilter,
                        "stop" => FinishReason::Stop,
                        _ => FinishReason::Error,
                    };
                }
            }
        }
        for data in decoder.finish()? {
            if data == "[DONE]" {
                saw_done = true;
            }
        }
        if !saw_done {
            return Err(XduduError::provider(
                format!("{} 流在 [DONE] 前结束。", self.display_name),
                !emitted,
            ));
        }

        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::Text { text });
        }
        let mut tool_calls = Vec::new();
        for tool in tools.into_values() {
            let input: Value = serde_json::from_str(if tool.arguments.is_empty() {
                "{}"
            } else {
                &tool.arguments
            })
            .map_err(|error| {
                XduduError::provider(
                    format!("{} 流式工具参数不是有效 JSON：{error}", self.display_name),
                    false,
                )
            })?;
            blocks.push(ContentBlock::ToolUse {
                id: tool.id.clone(),
                name: tool.name.clone(),
                input: input.clone(),
            });
            tool_calls.push(ToolCall {
                id: tool.id,
                name: tool.name,
                input,
            });
        }
        Ok(ProviderResponse {
            message: ProviderMessage {
                role: MessageRole::Assistant,
                content: MessageContent::Blocks(blocks),
            },
            tool_calls,
            usage,
            finish_reason,
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
        })
    }
}

#[async_trait]
impl Provider for OpenAiWire {
    fn name(&self) -> &'static str {
        self.display_name
    }

    async fn chat(&self, request: ProviderRequest) -> XduduResult<ProviderResponse> {
        OpenAiWire::chat(self, request).await
    }

    async fn stream_chat(
        &self,
        request: ProviderRequest,
        sink: &dyn ProviderStreamSink,
    ) -> XduduResult<ProviderResponse> {
        OpenAiWire::stream_chat(self, request, sink).await
    }
}
