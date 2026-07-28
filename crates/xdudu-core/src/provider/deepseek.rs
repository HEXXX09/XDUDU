//! DeepSeek 的 OpenAI Chat Completions 兼容适配器。

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

pub struct DeepSeekProvider {
    api_key: String,
    base_url: String,
    client: Client,
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
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: http_client(timeout)?,
        })
    }

    fn openai_messages(request: &ProviderRequest) -> XduduResult<Vec<Value>> {
        let mut output = vec![json!({ "role": "system", "content": request.system })];
        for message in &request.messages {
            match &message.content {
                MessageContent::Text(content) => output.push(json!({
                    "role": message.role,
                    "content": content,
                })),
                MessageContent::Blocks(blocks) => {
                    let mut text = String::new();
                    let mut tool_calls = Vec::new();
                    let mut tool_results = Vec::new();
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text: block_text } => text.push_str(block_text),
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
                        output.push(json!({
                            "role": "assistant",
                            "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                            "tool_calls": tool_calls,
                        }));
                    } else if !tool_results.is_empty() {
                        output.extend(tool_results);
                    } else {
                        output.push(json!({ "role": message.role, "content": text }));
                    }
                }
            }
        }
        Ok(output)
    }

    fn request_body(request: &ProviderRequest) -> XduduResult<Value> {
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
        Ok(json!({
            "model": request.model,
            "messages": Self::openai_messages(request)?,
            "tools": tools,
            "temperature": request.temperature,
            "max_tokens": request.max_output_tokens,
        }))
    }

    fn parse_response(value: Value) -> XduduResult<ProviderResponse> {
        let choice = value
            .pointer("/choices/0")
            .ok_or_else(|| XduduError::provider("DeepSeek 响应缺少 choices[0]。", false))?;
        let message = choice
            .get("message")
            .ok_or_else(|| XduduError::provider("DeepSeek 响应缺少 message。", false))?;
        let mut blocks = Vec::new();
        if let Some(content) = message.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            blocks.push(ContentBlock::Text {
                text: content.to_owned(),
            });
        }
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
                XduduError::provider(format!("DeepSeek 工具参数不是有效 JSON：{error}"), false)
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
        })
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn name(&self) -> &'static str {
        "deepseek"
    }

    async fn chat(&self, request: ProviderRequest) -> XduduResult<ProviderResponse> {
        let body = Self::request_body(&request)?;
        let send = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();
        let response = tokio::select! {
            _ = request.cancellation.cancelled() => return Err(XduduError::provider("DeepSeek 请求已中断。", false)),
            response = send => response.map_err(|error| XduduError::provider(format!("DeepSeek 请求失败：{error}"), error.is_timeout() || error.is_connect()))?,
        };
        Self::parse_response(parse_http_response(response, "DeepSeek").await?)
    }

    async fn stream_chat(
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
            _ = request.cancellation.cancelled() => return Err(XduduError::provider("DeepSeek 请求已中断。", false)),
            response = send => response.map_err(|error| XduduError::provider(format!("DeepSeek 请求失败：{error}"), error.is_timeout() || error.is_connect()))?,
        };
        if !response.status().is_success() {
            return Err(parse_http_response(response, "DeepSeek").await.unwrap_err());
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
        let mut tools = BTreeMap::<usize, ToolAccumulator>::new();
        let mut usage = TokenUsage::default();
        let mut finish_reason = FinishReason::Stop;
        let mut saw_done = false;
        let mut emitted = false;

        while let Some(chunk) = tokio::select! {
            _ = request.cancellation.cancelled() => return Err(XduduError::provider("DeepSeek 流已中断。", false)),
            chunk = chunks.next() => chunk,
        } {
            let chunk = chunk.map_err(|error| {
                XduduError::provider(format!("读取 DeepSeek 流失败：{error}"), !emitted)
            })?;
            for data in decoder.push(&chunk)? {
                if data == "[DONE]" {
                    saw_done = true;
                    continue;
                }
                let value: Value = serde_json::from_str(&data).map_err(|error| {
                    XduduError::provider(format!("DeepSeek 流事件不是有效 JSON：{error}"), false)
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
                "DeepSeek 流在 [DONE] 前结束。",
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
                    format!("DeepSeek 流式工具参数不是有效 JSON：{error}"),
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            cancellation: CancellationToken::new(),
        };
        let messages = DeepSeekProvider::openai_messages(&request).unwrap();
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call-1");
    }
}
