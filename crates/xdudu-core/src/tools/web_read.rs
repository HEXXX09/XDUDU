//! `web_read`：分段拉取大型页面 + LLM 提炼子循环，解决 `web_fetch`
//! 无法读取大页面的问题。
//!
//! 复用 `web_fetch` 的逐跳 SSRF / DNS 固定 / 内容类型边界；提炼请求
//! 使用与主循环同一 Provider 的独立请求（隐藏思考、不进入会话历史）。

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    SideEffectKind,
    permission::PermissionLevel,
    provider::{MessageRole, Provider, ProviderMessage, ProviderRequest, ProviderToolDefinition},
};

use super::{
    Tool, ToolContext, ToolDefinition, ToolResult, object, reject_unknown_fields, required_string,
};

/// 单次拉取的字节上限（与 web_fetch 一致）。
const CHUNK_BYTES: u64 = 1024 * 1024;
/// 最大分段数。
const DEFAULT_MAX_CHUNKS: u64 = 8;
const MAX_CHUNKS: u64 = 8;
const MAX_START_REF: u64 = 10_000;
/// 提炼文本块的字符上限。
const EXTRACT_CHAR_LIMIT: usize = 8 * 1024;
/// 单次工具调用最多触发的提炼请求数，避免超大网页造成不可控模型成本。
const MAX_EXTRACT_BLOCKS: usize = 8;
/// 提炼输出 keyPoints 上限。
const MAX_KEY_POINTS: usize = 8;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);

pub struct WebReadTool {
    provider: Option<Arc<dyn Provider>>,
    model: String,
    temperature: f32,
    max_output_tokens: u32,
    reasoning: bool,
}

impl WebReadTool {
    /// `provider` 为 None 时提炼回退为纯文本段（测试与无模型场景）。
    pub fn new(
        provider: Option<Arc<dyn Provider>>,
        model: String,
        temperature: f32,
        max_output_tokens: u32,
        reasoning: bool,
    ) -> Self {
        Self {
            provider,
            model,
            temperature,
            max_output_tokens,
            reasoning,
        }
    }
}

/// 提炼协议 DTO：字段严格、未知字段拒绝。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractDto {
    summary: String,
    #[serde(default)]
    key_points: Vec<String>,
}

fn extract_tool_definition() -> ProviderToolDefinition {
    ProviderToolDefinition {
        name: "submit_content_summary".into(),
        description: "提交网页内容提炼：summary 为针对目标的中文总结，key_points 为要点列表。"
            .into(),
        input_schema: json!({
            "type":"object",
            "required":["summary","key_points"],
            "additionalProperties":false,
            "properties":{
                "summary":{"type":"string","minLength":1,"maxLength":4096},
                "key_points":{"type":"array","items":{"type":"string","minLength":1,"maxLength":512},"maxItems":12}
            }
        }),
    }
}

/// 把纯文本按约 `limit` 字符切成块（满限即切，拼接后与原文一致）。
fn split_blocks(text: &str, limit: usize) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        current.push(character);
        if current.chars().count() >= limit {
            blocks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

fn sample_blocks(blocks: &[String], limit: usize) -> Vec<&String> {
    if blocks.len() <= limit {
        return blocks.iter().collect();
    }
    if limit <= 1 {
        return blocks.first().into_iter().collect();
    }
    (0..limit)
        .map(|index| {
            let source_index = index * (blocks.len() - 1) / (limit - 1);
            &blocks[source_index]
        })
        .collect()
}

/// 提炼单块文本；失败返回 None（调用方回退纯文本）。
async fn extract_block(
    tool: &WebReadTool,
    goal: &str,
    text: &str,
    cancellation: CancellationToken,
) -> Option<(String, Vec<String>)> {
    let provider = tool.provider.as_deref()?;
    let request = ProviderRequest {
        session_id: "web-read-extract".into(),
        model: tool.model.clone(),
        messages: vec![ProviderMessage::text(
            MessageRole::User,
            format!(
                "阅读目标：{goal}\n\n网页内容：\n{}",
                text.chars().take(EXTRACT_CHAR_LIMIT).collect::<String>()
            ),
        )],
        tools: vec![extract_tool_definition()],
        system: "你是网页内容提炼器。针对给定的阅读目标，提炼网页内容为中文总结与要点列表；\
        不得编造内容，不得输出思维链。请调用 submit_content_summary 提交结果。"
            .to_owned(),
        temperature: tool.temperature,
        max_output_tokens: tool.max_output_tokens,
        reasoning: tool.reasoning,
        cancellation,
    };
    let response = provider.chat(request).await.ok()?;
    if response.finish_reason != crate::provider::FinishReason::ToolCalls {
        return None;
    }
    let call = response
        .tool_calls
        .iter()
        .find(|call| call.name == "submit_content_summary")?;
    let dto: ExtractDto = serde_json::from_value(call.input.clone()).ok()?;
    let summary = dto.summary.trim();
    if summary.is_empty() {
        return None;
    }
    let points = dto
        .key_points
        .into_iter()
        .map(|point| point.trim().to_owned())
        .filter(|point| !point.is_empty())
        .take(MAX_KEY_POINTS)
        .collect();
    Some((summary.to_owned(), points))
}

async fn read_limited_response(
    response: reqwest::Response,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<(Vec<u8>, bool), String> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = response
        .content_length()
        .is_some_and(|length| length > limit as u64);
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            _ = cancellation.cancelled() => return Err("网页读取已取消。".into()),
            next = stream.next() => next,
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|error| format!("读取网页响应失败：{error}"))?;
        let remaining = limit.saturating_sub(bytes.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            truncated = true;
            break;
        }
    }
    Ok((bytes, truncated))
}

#[async_trait]
impl Tool for WebReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_read".into(),
            description: "分段拉取大型公开网页并针对目标提炼总结；返回 summary、keyPoints、读取进度与续读锚点。".into(),
            input_schema: json!({
                "type":"object",
                "required":["url","goal"],
                "additionalProperties":false,
                "properties":{
                    "url":{"type":"string","minLength":1,"maxLength":8192},
                    "goal":{"type":"string","minLength":1,"maxLength":2048},
                    "maxChunks":{"type":"integer","minimum":1,"maximum":MAX_CHUNKS,"default":DEFAULT_MAX_CHUNKS},
                    "startRef":{"type":"integer","minimum":0,"maximum":MAX_START_REF,"default":0}
                }
            }),
            // 网页读取不修改本地状态，三种权限模式均可请求；
            // 是否真正联网由 NetworkAccess 审批单独控制（与 web_fetch 一致）。
            permission_level: PermissionLevel::ReadOnly,
            side_effect: SideEffectKind::NetworkAccess,
            default_timeout: DEFAULT_TIMEOUT,
        }
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let map = object(input)?;
        let mut issues = Vec::new();
        reject_unknown_fields(map, &["url", "goal", "maxChunks", "startRef"], &mut issues);
        if required_string(map, "url", 8192, &mut issues).is_none() {
            issues.push("url 必须是 1 到 8192 字节的字符串。".into());
        }
        if required_string(map, "goal", 2048, &mut issues).is_none() {
            issues.push("goal 必须是 1 到 2048 字节的字符串。".into());
        }
        if let Some(chunks) = map.get("maxChunks")
            && !chunks
                .as_u64()
                .is_some_and(|value| (1..=MAX_CHUNKS).contains(&value))
        {
            issues.push(format!("maxChunks 必须在 1 到 {MAX_CHUNKS} 之间。"));
        }
        if let Some(start) = map.get("startRef")
            && !start.as_u64().is_some_and(|value| value <= MAX_START_REF)
        {
            issues.push(format!("startRef 必须在 0 到 {MAX_START_REF} 之间。"));
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        let url = input["url"].as_str().unwrap_or_default();
        let goal = input["goal"].as_str().unwrap_or_default();
        let max_chunks = input
            .get("maxChunks")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_CHUNKS)
            .min(MAX_CHUNKS);
        let start_ref = input.get("startRef").and_then(Value::as_u64).unwrap_or(0);
        let parsed = match url::Url::parse(url) {
            Ok(parsed) => parsed,
            Err(error) => {
                return ToolResult::failure(
                    "INVALID_URL",
                    format!("URL 无效：{error}"),
                    context.started_at,
                    json!({ "url": url }),
                );
            }
        };
        // SSRF / DNS 固定 / 内容类型边界与 web_fetch 一致（内部含 validate_url）。
        let client = match super::web_fetch::pinned_client(&parsed, DEFAULT_TIMEOUT).await {
            Ok(client) => client,
            Err(reason) => {
                return ToolResult::failure(
                    "WEB_BLOCKED",
                    reason,
                    context.started_at,
                    json!({ "url": url }),
                );
            }
        };
        let mut chunks_read: u64 = 0;
        let mut combined_text = String::new();
        let mut title = None;
        let mut truncated = false;
        let mut next_start_ref: Option<u64> = None;
        context.report_progress(crate::tools::ToolProgressUpdate::phase(
            "reading",
            "分段读取网页",
        ));
        for chunk_index in 0..max_chunks {
            if context.cancellation.is_cancelled() {
                return ToolResult::failure(
                    "TOOL_ABORTED",
                    "网页读取已取消。",
                    context.started_at,
                    json!({ "url": url }),
                );
            }
            let offset = (start_ref + chunk_index) * CHUNK_BYTES;
            let request = client.get(parsed.clone()).header(
                "Range",
                format!("bytes={offset}-{}", offset + CHUNK_BYTES - 1),
            );
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    return ToolResult::failure(
                        "WEB_REQUEST_ERROR",
                        format!("网络请求失败：{error}"),
                        context.started_at,
                        json!({ "url": url }),
                    );
                }
            };
            let status = response.status();
            if !status.is_success() && status.as_u16() != 206 {
                return ToolResult::failure(
                    "WEB_HTTP_ERROR",
                    format!("HTTP 状态 {status}"),
                    context.started_at,
                    json!({ "url": url, "status": status.as_u16() }),
                );
            }
            if start_ref > 0 && status.as_u16() != 206 {
                return ToolResult::failure(
                    "RANGE_UNSUPPORTED",
                    "目标服务器不支持分段续读。",
                    context.started_at,
                    json!({ "url": url, "startRef": start_ref }),
                );
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let Some(kind) = super::web_fetch::content_kind(&content_type) else {
                return ToolResult::failure(
                    "UNSUPPORTED_CONTENT_TYPE",
                    format!("不支持的内容类型：{content_type}"),
                    context.started_at,
                    json!({ "url": url, "contentType": content_type }),
                );
            };
            let (bytes, body_truncated) =
                match read_limited_response(response, CHUNK_BYTES as usize, &context.cancellation)
                    .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        let code = if context.cancellation.is_cancelled() {
                            "TOOL_ABORTED"
                        } else {
                            "WEB_READ_ERROR"
                        };
                        return ToolResult::failure(
                            code,
                            error,
                            context.started_at,
                            json!({ "url": url }),
                        );
                    }
                };
            if kind != "html" {
                // 非 HTML 内容按纯文本处理。
                let text = String::from_utf8_lossy(&bytes);
                combined_text.push_str(&text);
            } else {
                let (page_title, text) = super::web_fetch::html_text(&bytes);
                if title.is_none() {
                    title = page_title;
                }
                combined_text.push_str(&text);
                combined_text.push('\n');
            }
            chunks_read += 1;
            // 200 表示服务器忽略 Range；只允许读取首段，避免重复拉取同一内容。
            if status.as_u16() != 206 {
                truncated = body_truncated;
                break;
            }
            let has_more = body_truncated || (bytes.len() as u64) == CHUNK_BYTES;
            if !has_more {
                break;
            }
            truncated = true;
            next_start_ref = Some(start_ref + chunks_read);
            context.report_progress(crate::tools::ToolProgressUpdate::counted(
                "reading",
                chunks_read,
                Some(max_chunks),
                "chunks",
            ));
        }
        if chunks_read == 0 {
            return ToolResult::failure(
                "WEB_EMPTY",
                "未读取到任何内容。",
                context.started_at,
                json!({ "url": url }),
            );
        }
        context.report_progress(crate::tools::ToolProgressUpdate::phase(
            "summarizing",
            "提炼网页内容",
        ));
        // 提炼：未截断时单次；截断时按块增量提炼并汇总。
        let blocks = split_blocks(&combined_text, EXTRACT_CHAR_LIMIT);
        let sampled_blocks = sample_blocks(&blocks, MAX_EXTRACT_BLOCKS);
        let total_blocks = blocks.len() as u64;
        let content_sampled = sampled_blocks.len() < blocks.len();
        let mut summaries: Vec<String> = Vec::new();
        let mut key_points: Vec<String> = Vec::new();
        let mut fallback_text = String::new();
        match &self.provider {
            Some(_) if !sampled_blocks.is_empty() => {
                for (index, block) in sampled_blocks.iter().enumerate() {
                    if context.cancellation.is_cancelled() {
                        break;
                    }
                    let plan =
                        extract_block(self, goal, block, context.cancellation.child_token()).await;
                    match plan {
                        Some((summary, points)) => {
                            summaries.push(summary);
                            for point in points {
                                if !key_points.contains(&point) {
                                    key_points.push(point);
                                }
                            }
                            context.report_progress(crate::tools::ToolProgressUpdate::counted(
                                "summarizing",
                                (index + 1) as u64,
                                Some(sampled_blocks.len() as u64),
                                "chunks",
                            ));
                        }
                        None => {
                            // 提炼失败：回退该块纯文本，不阻塞工作。
                            fallback_text.push_str(block);
                            fallback_text.push('\n');
                        }
                    }
                }
            }
            _ => {
                // 无 Provider：整体回退纯文本段。
                fallback_text = sampled_blocks
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
            }
        }
        let summary = if summaries.is_empty() {
            fallback_text
                .chars()
                .take(EXTRACT_CHAR_LIMIT)
                .collect::<String>()
        } else {
            summaries.join("\n")
        };
        key_points.truncate(MAX_KEY_POINTS);
        let output = json!({
            "url": url,
            "title": title,
            "summary": summary,
            "keyPoints": key_points,
            "chunksRead": chunks_read,
            "totalChunks": total_blocks,
            "truncated": truncated,
            "nextStartRef": next_start_ref,
            "contentSampled": content_sampled,
        });
        ToolResult::success(
            output,
            context.started_at,
            json!({ "url": url, "chunksRead": chunks_read, "truncated": truncated }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 文本按限制切块() {
        let blocks = split_blocks("一二三四五六七八九十", 6);
        assert!(blocks.iter().all(|block| block.chars().count() <= 6));
        assert_eq!(blocks.join(""), "一二三四五六七八九十");
    }

    #[test]
    fn 长文本切成多块() {
        let text = "a".repeat(10) + "\n" + &"b".repeat(10);
        let blocks = split_blocks(&text, 5);
        assert!(blocks.len() >= 3);
    }

    #[test]
    fn 超多文本块只选择有界样本且覆盖首尾() {
        let blocks = (0..100)
            .map(|index| format!("block-{index}"))
            .collect::<Vec<_>>();
        let sampled = sample_blocks(&blocks, MAX_EXTRACT_BLOCKS);
        assert_eq!(sampled.len(), MAX_EXTRACT_BLOCKS);
        assert_eq!(sampled.first().unwrap().as_str(), "block-0");
        assert_eq!(sampled.last().unwrap().as_str(), "block-99");
    }

    #[test]
    fn _schema_要求_url_goal() {
        let tool = WebReadTool::new(None, "test".into(), 0.2, 4096, false);
        assert!(tool.validate(&json!({ "url": "https://x" })).is_err());
        assert!(
            tool.validate(&json!({ "url": "https://x", "goal": "总结" }))
                .is_ok()
        );
        assert!(
            tool.validate(&json!({ "url": "https://x", "goal": "总结", "maxChunks": 9 }))
                .is_err()
        );
        assert!(
            tool.validate(&json!({ "url": "https://x", "goal": "总结", "startRef": 3 }))
                .is_ok()
        );
        assert!(
            tool.validate(&json!({ "url": "https://x", "goal": "总结", "startRef": 10001 }))
                .is_err()
        );
        assert!(
            tool.validate(&json!({ "url": "https://x", "goal": "总结", "unknown": 1 }))
                .is_err()
        );
    }

    #[test]
    fn 提炼_dto_拒绝未知字段() {
        let parsed: Result<ExtractDto, _> =
            serde_json::from_value(json!({ "summary": "s", "key_points": ["k"] }));
        assert!(parsed.is_ok());
        let rejected: Result<ExtractDto, _> =
            serde_json::from_value(json!({ "summary": "s", "key_points": [], "extra": 1 }));
        assert!(rejected.is_err());
    }
}
