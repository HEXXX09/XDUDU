//! `web_search`：通过固定的公网搜索入口返回有界、结构化的网页结果。

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use scraper::{Html, Selector};
use serde_json::{Value, json};
use url::Url;

use crate::{SideEffectKind, permission::PermissionLevel};

use super::{
    Tool, ToolContext, ToolDefinition, ToolProgressUpdate, ToolResult, object,
    reject_unknown_fields, required_string, web_fetch::pinned_client,
};

const SEARCH_ENDPOINT: &str = "https://search.brave.com/search";
const DEFAULT_RESULTS: u64 = 5;
const MAX_RESULTS: u64 = 10;
const DEFAULT_TIMEOUT_SECONDS: u64 = 20;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_QUERY_BYTES: usize = 1024;
const MAX_TITLE_CHARS: usize = 512;
const MAX_SNIPPET_CHARS: usize = 2_000;

pub struct WebSearchTool;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

fn normalized_text(element: scraper::ElementRef<'_>, limit: usize) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

fn parse_results(html: &[u8], max_results: usize) -> Vec<SearchResult> {
    let document = Html::parse_document(&String::from_utf8_lossy(html));
    let Ok(result_selector) = Selector::parse(r#".snippet[data-type="web"]"#) else {
        return Vec::new();
    };
    let Ok(link_selector) = Selector::parse("a[href]") else {
        return Vec::new();
    };
    let Ok(title_selector) = Selector::parse(".search-snippet-title") else {
        return Vec::new();
    };
    let Ok(snippet_selector) = Selector::parse(".generic-snippet .content") else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    document
        .select(&result_selector)
        .filter_map(|result| {
            let link = result.select(&link_selector).next()?;
            let url = Url::parse(link.value().attr("href")?).ok()?;
            if url.scheme() != "https" || url.username() != "" || url.password().is_some() {
                return None;
            }
            let url = url.to_string();
            if !seen.insert(url.clone()) {
                return None;
            }
            let title = result
                .select(&title_selector)
                .next()
                .map(|element| normalized_text(element, MAX_TITLE_CHARS))?;
            if title.is_empty() {
                return None;
            }
            let snippet = result
                .select(&snippet_selector)
                .next()
                .map(|element| normalized_text(element, MAX_SNIPPET_CHARS))
                .unwrap_or_default();
            Some(SearchResult {
                title,
                url,
                snippet,
            })
        })
        .take(max_results)
        .collect()
}

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".into(),
            description: "搜索公开互联网并返回标题、HTTPS 链接和摘要；适用于通用知识、查询、研究和时效性问题。".into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "query":{"type":"string","minLength":1,"maxLength":MAX_QUERY_BYTES},
                    "maxResults":{"type":"integer","minimum":1,"maximum":MAX_RESULTS},
                    "timeoutSeconds":{"type":"integer","minimum":1,"maximum":30}
                },
                "required":["query"],
                "additionalProperties":false
            }),
            permission_level: PermissionLevel::ReadOnly,
            side_effect: SideEffectKind::NetworkAccess,
            default_timeout: Duration::from_secs(35),
        }
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let map = object(input)?;
        let mut issues = Vec::new();
        reject_unknown_fields(map, &["query", "maxResults", "timeoutSeconds"], &mut issues);
        required_string(map, "query", MAX_QUERY_BYTES, &mut issues);
        if !map.get("maxResults").is_none_or(|value| {
            value
                .as_u64()
                .is_some_and(|value| (1..=MAX_RESULTS).contains(&value))
        }) {
            issues.push(format!("maxResults 必须是 1 到 {MAX_RESULTS} 的整数。"));
        }
        if !map.get("timeoutSeconds").is_none_or(|value| {
            value
                .as_u64()
                .is_some_and(|value| (1..=30).contains(&value))
        }) {
            issues.push("timeoutSeconds 必须是 1 到 30 的整数。".into());
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        let query = input["query"].as_str().unwrap_or_default();
        let max_results = input
            .get("maxResults")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_RESULTS) as usize;
        let timeout = Duration::from_secs(
            input
                .get("timeoutSeconds")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        );
        let started = Instant::now();
        let mut url = Url::parse(SEARCH_ENDPOINT).expect("固定搜索 URL 必须有效");
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("source", "web");
        context.report_progress(ToolProgressUpdate::phase(
            "searching",
            format!("搜索：{query}"),
        ));
        let client = match pinned_client(&url, timeout).await {
            Ok(client) => client,
            Err(message) => {
                let code = if message.contains("超时") || message.contains("超时时间") {
                    "WEB_SEARCH_TIMEOUT"
                } else if message.starts_with("DNS ")
                    || message.starts_with("安全 DNS")
                    || message.starts_with("创建 HTTPS")
                    || message.starts_with("创建安全 DNS")
                {
                    "WEB_SEARCH_ERROR"
                } else {
                    "NETWORK_POLICY_DENIED"
                };
                return ToolResult::failure(
                    code,
                    message,
                    context.started_at,
                    json!({"query":query}),
                );
            }
        };
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return ToolResult::failure(
                "WEB_SEARCH_TIMEOUT",
                "网络搜索超时。",
                context.started_at,
                json!({"query":query}),
            );
        }
        let response = match tokio::select! {
            _ = context.cancellation.cancelled() => None,
            response = tokio::time::timeout(remaining, client.get(url).send()) => Some(response),
        } {
            None => {
                return ToolResult::failure(
                    "TOOL_ABORTED",
                    "网络搜索已取消。",
                    context.started_at,
                    json!({"query":query}),
                );
            }
            Some(Err(_)) => {
                return ToolResult::failure(
                    "WEB_SEARCH_TIMEOUT",
                    "网络搜索超时。",
                    context.started_at,
                    json!({"query":query}),
                );
            }
            Some(Ok(Err(error))) => {
                return ToolResult::failure(
                    "WEB_SEARCH_ERROR",
                    error.to_string(),
                    context.started_at,
                    json!({"query":query}),
                );
            }
            Some(Ok(Ok(response))) => response,
        };
        if !response.status().is_success() {
            return ToolResult::failure(
                "WEB_SEARCH_HTTP_STATUS",
                format!("搜索服务返回 HTTP {}。", response.status().as_u16()),
                context.started_at,
                json!({"query":query,"status":response.status().as_u16()}),
            );
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !content_type.starts_with("text/html") {
            return ToolResult::failure(
                "WEB_SEARCH_INVALID_RESPONSE",
                format!("搜索服务返回了非 HTML 内容：{content_type}"),
                context.started_at,
                json!({"query":query,"contentType":content_type}),
            );
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return ToolResult::failure(
                "WEB_SEARCH_RESPONSE_TOO_LARGE",
                "搜索响应超过 1 MiB 限制。",
                context.started_at,
                json!({"query":query}),
            );
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::select! {
            _ = context.cancellation.cancelled() => None,
            chunk = stream.next() => chunk,
        } {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    return ToolResult::failure(
                        "WEB_SEARCH_ERROR",
                        error.to_string(),
                        context.started_at,
                        json!({"query":query}),
                    );
                }
            };
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return ToolResult::failure(
                    "WEB_SEARCH_RESPONSE_TOO_LARGE",
                    "搜索响应超过 1 MiB 限制。",
                    context.started_at,
                    json!({"query":query}),
                );
            }
            bytes.extend_from_slice(&chunk);
        }
        if context.cancellation.is_cancelled() {
            return ToolResult::failure(
                "TOOL_ABORTED",
                "网络搜索已取消。",
                context.started_at,
                json!({"query":query}),
            );
        }
        context.report_progress(ToolProgressUpdate::phase("parsing", "整理搜索结果"));
        let results = parse_results(&bytes, max_results);
        let output = results
            .iter()
            .map(|result| {
                json!({
                    "title":result.title,
                    "url":result.url,
                    "snippet":result.snippet,
                })
            })
            .collect::<Vec<_>>();
        ToolResult::success(
            json!({
                "query":query,
                "engine":"brave",
                "results":output,
                "resultCount":output.len(),
                "noResults":output.is_empty(),
            }),
            context.started_at,
            json!({"engine":"brave","bytesRead":bytes.len()}),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use crate::{
        AllowAllApprovalGate, PermissionMode,
        tools::{ToolRegistry, WebSearchTool},
    };

    use super::*;

    #[test]
    fn 解析并限制公开_https_搜索结果() {
        let html = br#"
        <div class="snippet" data-type="web">
          <a href="https://www.rust-lang.org/">
            <div class="title search-snippet-title"> Rust Programming Language </div>
          </a>
          <div class="generic-snippet"><div class="content">Reliable and efficient software.</div></div>
        </div>
        <div class="snippet" data-type="web">
          <a href="http://unsafe.example/">
            <div class="title search-snippet-title">HTTP result</div>
          </a>
        </div>
        <div class="snippet" data-type="web">
          <a href="https://example.com/second">
            <div class="title search-snippet-title">Second result</div>
          </a>
          <div class="generic-snippet"><div class="content">Second snippet.</div></div>
        </div>
        "#;

        let results = parse_results(html, 1);

        assert_eq!(
            results,
            vec![SearchResult {
                title: "Rust Programming Language".into(),
                url: "https://www.rust-lang.org/".into(),
                snippet: "Reliable and efficient software.".into(),
            }]
        );
    }

    #[tokio::test]
    #[ignore = "需要访问公网搜索服务"]
    async fn 真实公网搜索返回结构化结果() {
        let dir = tempdir().unwrap();
        let mut registry = ToolRegistry::with_approval_gate(Arc::new(AllowAllApprovalGate));
        registry.register(WebSearchTool).unwrap();

        let result = registry
            .execute(
                "web_search",
                json!({"query":"Rust programming language official","maxResults":3}),
                Uuid::new_v4(),
                dir.path(),
                PermissionMode::ReadOnly,
                CancellationToken::new(),
            )
            .await;

        assert!(
            result.success,
            "{}",
            result
                .error
                .as_ref()
                .map(|error| error.message.as_str())
                .unwrap_or("未知错误")
        );
        let output = result.output.unwrap();
        assert!(
            output["resultCount"]
                .as_u64()
                .is_some_and(|count| count > 0)
        );
        assert!(
            output["results"][0]["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://"))
        );
    }
}
