//! `web_fetch`：仅访问公网 HTTPS、逐跳防御 SSRF 的有界文本抓取。

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{
    Client,
    header::{CONTENT_TYPE, LOCATION},
    redirect::Policy,
};
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::lookup_host;
use url::Url;

use crate::{SideEffectKind, permission::PermissionLevel};

use super::{
    Tool, ToolContext, ToolDefinition, ToolProgressUpdate, ToolResult, object,
    reject_unknown_fields, required_string,
};

const DEFAULT_MAX_BYTES: u64 = 512 * 1024;
const MAX_BYTES: u64 = 1024 * 1024;
const DEFAULT_TIMEOUT_SECONDS: u64 = 20;
const MAX_REDIRECTS: usize = 5;

pub struct WebFetchTool;

fn fake_ip(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(ip) if {
        let [a, b, _, _] = ip.octets();
        a == 198 && matches!(b, 18 | 19)
    })
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 175 && c == 48)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224
        || (a == 255 && b == 255 && c == 255 && d == 255))
}

fn public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4() {
        return public_ipv4(ipv4);
    }
    let segments = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0)
        || (segments[0] == 0x2001 && segments[1] <= 0x01ff)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || segments[0] == 0x2002
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
        || segments[0] == 0x5f00
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_ipv4(ip),
        IpAddr::V6(ip) => public_ipv6(ip),
    }
}

fn validate_url(url: &Url) -> Result<(), String> {
    if url.scheme() != "https" {
        return Err("web_fetch 只允许 HTTPS URL。".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL 不能包含用户名或密码。".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL 缺少主机名。".to_owned())?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return Err("拒绝访问 localhost。".into());
    }
    if let Ok(ip) = host.parse::<IpAddr>()
        && !public_ip(ip)
    {
        return Err(format!("拒绝访问非公网地址：{ip}"));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DohResponse {
    status: u32,
    #[serde(default)]
    answer: Vec<DohAnswer>,
}

#[derive(Debug, Deserialize)]
struct DohAnswer {
    #[serde(rename = "type")]
    record_type: u16,
    data: String,
}

async fn doh_addresses(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<Vec<SocketAddr>, String> {
    const DOH_HOST: &str = "dns.google";
    const DOH_RESPONSE_LIMIT: u64 = 64 * 1024;
    let started = Instant::now();
    let resolver_addresses = [
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)), 443),
    ];
    let client = Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .timeout(timeout)
        .user_agent("XDUDU/0.6")
        .resolve_to_addrs(DOH_HOST, &resolver_addresses)
        .build()
        .map_err(|error| format!("创建安全 DNS 客户端失败：{error}"))?;
    let mut addresses = HashSet::new();
    for record_type in ["A", "AAAA"] {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err("安全 DNS 解析超时。".into());
        }
        let mut url =
            Url::parse("https://dns.google/resolve").expect("固定的安全 DNS URL 必须有效");
        url.query_pairs_mut()
            .append_pair("name", host)
            .append_pair("type", record_type);
        let response = tokio::time::timeout(remaining, client.get(url).send())
            .await
            .map_err(|_| "安全 DNS 解析超时。".to_owned())?
            .map_err(|error| format!("安全 DNS 请求失败：{error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "安全 DNS 返回 HTTP {}。",
                response.status().as_u16()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > DOH_RESPONSE_LIMIT)
        {
            return Err("安全 DNS 响应超过大小限制。".into());
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| format!("读取安全 DNS 响应失败：{error}"))?;
            if bytes.len().saturating_add(chunk.len()) as u64 > DOH_RESPONSE_LIMIT {
                return Err("安全 DNS 响应超过大小限制。".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let response: DohResponse = serde_json::from_slice(&bytes)
            .map_err(|error| format!("安全 DNS 响应无效：{error}"))?;
        if response.status != 0 {
            return Err(format!("安全 DNS 查询失败，状态码 {}。", response.status));
        }
        for answer in response.answer {
            if matches!(answer.record_type, 1 | 28)
                && let Ok(ip) = answer.data.parse::<IpAddr>()
            {
                addresses.insert(SocketAddr::new(ip, port));
            }
        }
    }
    if addresses.is_empty() {
        return Err("安全 DNS 没有返回 A 或 AAAA 地址。".into());
    }
    Ok(addresses.into_iter().collect())
}

pub(crate) async fn pinned_client(url: &Url, timeout: Duration) -> Result<Client, String> {
    let started = Instant::now();
    validate_url(url)?;
    let host = url
        .host_str()
        .ok_or_else(|| "URL 缺少主机名。".to_owned())?;
    let port = url.port_or_known_default().unwrap_or(443);
    let mut addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::time::timeout(timeout, lookup_host((host, port)))
            .await
            .map_err(|_| "DNS 解析超时。".to_owned())?
            .map_err(|error| format!("DNS 解析失败：{error}"))?
            .collect::<Vec<_>>()
    };
    if addresses.is_empty() {
        return Err("DNS 没有返回可用地址。".into());
    }
    if addresses.iter().all(|address| fake_ip(address.ip())) {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err("DNS 解析耗尽了网页抓取超时时间。".into());
        }
        addresses = doh_addresses(host, port, remaining).await?;
    }
    if let Some(address) = addresses.iter().find(|address| !public_ip(address.ip())) {
        return Err(format!("DNS 返回非公网地址，已拒绝访问：{}", address.ip()));
    }
    let request_timeout = timeout.saturating_sub(started.elapsed());
    if request_timeout.is_zero() {
        return Err("DNS 解析耗尽了网页抓取超时时间。".into());
    }
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .timeout(request_timeout)
        .user_agent("XDUDU/0.6");
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    builder
        .build()
        .map_err(|error| format!("创建 HTTPS 客户端失败：{error}"))
}

pub(crate) fn content_kind(content_type: &str) -> Option<&'static str> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if mime == "text/html" {
        Some("html")
    } else if mime == "text/plain" {
        Some("text")
    } else if mime == "application/json" || mime.ends_with("+json") {
        Some("json")
    } else {
        None
    }
}

fn strip_element(mut html: String, tag: &str) -> String {
    let pattern = format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}\s*>");
    regex::Regex::new(&pattern)
        .map(|regex| regex.replace_all(&html, " ").into_owned())
        .unwrap_or_else(|_| std::mem::take(&mut html))
}

pub(crate) fn html_text(bytes: &[u8]) -> (Option<String>, String) {
    let mut source = String::from_utf8_lossy(bytes).into_owned();
    for tag in ["script", "style", "noscript", "svg"] {
        source = strip_element(source, tag);
    }
    let document = Html::parse_document(&source);
    let title = Selector::parse("title")
        .ok()
        .and_then(|selector| document.select(&selector).next())
        .map(|element| {
            element
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|title| !title.is_empty());
    let text = Selector::parse("body")
        .ok()
        .and_then(|selector| document.select(&selector).next())
        .map(|element| element.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| document.root_element().text().collect::<Vec<_>>().join(" "))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (title, text)
}

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".into(),
            description: "抓取公网 HTTPS 网页、纯文本或 JSON；逐跳校验 DNS 和重定向，不携带 Cookie、认证或代理。".into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "url":{"type":"string","minLength":1,"maxLength":8192},
                    "maxBytes":{"type":"integer","minimum":1,"maximum":MAX_BYTES},
                    "timeoutSeconds":{"type":"integer","minimum":1,"maximum":30}
                },
                "required":["url"],
                "additionalProperties":false
            }),
            // 网页读取不修改本地状态，三种权限模式均可请求；
            // 是否真正联网由 NetworkAccess 审批单独控制。
            permission_level: PermissionLevel::ReadOnly,
            side_effect: SideEffectKind::NetworkAccess,
            default_timeout: Duration::from_secs(35),
        }
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let map = object(input)?;
        let mut issues = Vec::new();
        reject_unknown_fields(map, &["url", "maxBytes", "timeoutSeconds"], &mut issues);
        if let Some(raw) = required_string(map, "url", 8192, &mut issues) {
            match Url::parse(raw) {
                Ok(url) => {
                    if let Err(error) = validate_url(&url) {
                        issues.push(error);
                    }
                }
                Err(error) => issues.push(format!("URL 无效：{error}")),
            }
        }
        if !map.get("maxBytes").is_none_or(|value| {
            value
                .as_u64()
                .is_some_and(|value| (1..=MAX_BYTES).contains(&value))
        }) {
            issues.push(format!("maxBytes 必须是 1 到 {MAX_BYTES} 的整数。"));
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
        let requested = input["url"].as_str().unwrap_or_default();
        let mut current = match Url::parse(requested) {
            Ok(url) => url,
            Err(error) => {
                return ToolResult::failure(
                    "INVALID_URL",
                    error.to_string(),
                    context.started_at,
                    json!({"url":requested}),
                );
            }
        };
        let max_bytes = input
            .get("maxBytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES) as usize;
        let timeout = Duration::from_secs(
            input
                .get("timeoutSeconds")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        );
        let deadline = Instant::now() + timeout;
        let mut redirects = 0;
        loop {
            if context.cancellation.is_cancelled() {
                return ToolResult::failure(
                    "TOOL_ABORTED",
                    "网页抓取已取消。",
                    context.started_at,
                    json!({"url":current}),
                );
            }
            context.report_progress(ToolProgressUpdate::phase(
                "resolving",
                format!("校验 {}", current.host_str().unwrap_or_default()),
            ));
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return ToolResult::failure(
                    "WEB_FETCH_TIMEOUT",
                    format!("网页抓取超过 {} 秒。", timeout.as_secs()),
                    context.started_at,
                    json!({"url":current}),
                );
            }
            let client = match pinned_client(&current, remaining).await {
                Ok(client) => client,
                Err(message) => {
                    let code = if message.contains("超时") || message.contains("超时时间") {
                        "WEB_FETCH_TIMEOUT"
                    } else if message.starts_with("DNS ")
                        || message.starts_with("安全 DNS")
                        || message.starts_with("创建 HTTPS")
                        || message.starts_with("创建安全 DNS")
                    {
                        "WEB_FETCH_ERROR"
                    } else {
                        "NETWORK_POLICY_DENIED"
                    };
                    return ToolResult::failure(
                        code,
                        message,
                        context.started_at,
                        json!({"url":current}),
                    );
                }
            };
            let response = match tokio::select! {
                _ = context.cancellation.cancelled() => None,
                result = client.get(current.clone()).send() => Some(result),
            } {
                Some(Ok(response)) => response,
                Some(Err(error)) => {
                    return ToolResult::failure(
                        "WEB_FETCH_ERROR",
                        error.to_string(),
                        context.started_at,
                        json!({"url":current}),
                    );
                }
                None => {
                    return ToolResult::failure(
                        "TOOL_ABORTED",
                        "网页抓取已取消。",
                        context.started_at,
                        json!({"url":current}),
                    );
                }
            };
            if response.status().is_redirection() {
                if redirects >= MAX_REDIRECTS {
                    return ToolResult::failure(
                        "TOO_MANY_REDIRECTS",
                        format!("重定向次数超过 {MAX_REDIRECTS}。"),
                        context.started_at,
                        json!({"url":current}),
                    );
                }
                let Some(location) = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                else {
                    return ToolResult::failure(
                        "INVALID_REDIRECT",
                        "重定向响应缺少有效 Location。",
                        context.started_at,
                        json!({"url":current}),
                    );
                };
                current = match current.join(location) {
                    Ok(url) => url,
                    Err(error) => {
                        return ToolResult::failure(
                            "INVALID_REDIRECT",
                            error.to_string(),
                            context.started_at,
                            json!({"location":location}),
                        );
                    }
                };
                if let Err(message) = validate_url(&current) {
                    return ToolResult::failure(
                        "NETWORK_POLICY_DENIED",
                        message,
                        context.started_at,
                        json!({"url":current}),
                    );
                }
                redirects += 1;
                continue;
            }
            let status = response.status();
            if !status.is_success() {
                return ToolResult::failure(
                    "HTTP_STATUS",
                    format!("网页返回 HTTP {}。", status.as_u16()),
                    context.started_at,
                    json!({"url":current,"status":status.as_u16()}),
                );
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let Some(kind) = content_kind(&content_type) else {
                return ToolResult::failure(
                    "UNSUPPORTED_CONTENT_TYPE",
                    format!("不支持的响应类型：{content_type}"),
                    context.started_at,
                    json!({"url":current,"contentType":content_type}),
                );
            };
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            let mut truncated = false;
            let mut last_progress = Instant::now();
            while let Some(chunk) = tokio::select! {
                _ = context.cancellation.cancelled() => None,
                chunk = stream.next() => chunk,
            } {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        return ToolResult::failure(
                            "WEB_FETCH_ERROR",
                            error.to_string(),
                            context.started_at,
                            json!({"url":current}),
                        );
                    }
                };
                let remaining = max_bytes.saturating_sub(bytes.len());
                if chunk.len() > remaining {
                    bytes.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                bytes.extend_from_slice(&chunk);
                if bytes.len() % (64 * 1024) < chunk.len()
                    || last_progress.elapsed() >= Duration::from_millis(250)
                {
                    context.report_progress(ToolProgressUpdate::counted(
                        "downloading",
                        bytes.len() as u64,
                        Some(max_bytes as u64),
                        "bytes",
                    ));
                    last_progress = Instant::now();
                }
            }
            if context.cancellation.is_cancelled() {
                return ToolResult::failure(
                    "TOOL_ABORTED",
                    "网页抓取已取消。",
                    context.started_at,
                    json!({"url":current}),
                );
            }
            if kind == "json" && truncated {
                return ToolResult::failure(
                    "RESPONSE_TOO_LARGE",
                    "JSON 响应超过大小限制，拒绝解析残缺内容。",
                    context.started_at,
                    json!({"url":current,"maxBytes":max_bytes}),
                );
            }
            let (title, text, json_value) = match kind {
                "html" => {
                    let (title, text) = html_text(&bytes);
                    (title, Some(text), None)
                }
                "text" => (
                    None,
                    Some(String::from_utf8_lossy(&bytes).into_owned()),
                    None,
                ),
                "json" => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(value) => (None, None, Some(value)),
                    Err(error) => {
                        return ToolResult::failure(
                            "INVALID_JSON_RESPONSE",
                            error.to_string(),
                            context.started_at,
                            json!({"url":current}),
                        );
                    }
                },
                _ => unreachable!(),
            };
            return ToolResult::success(
                json!({
                    "requestedUrl":requested,
                    "finalUrl":current,
                    "status":status.as_u16(),
                    "contentType":content_type,
                    "title":title,
                    "text":text,
                    "json":json_value,
                    "bytesRead":bytes.len(),
                    "truncated":truncated,
                    "redirects":redirects,
                }),
                context.started_at,
                json!({}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拒绝常见内网与保留地址() {
        for value in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
        ] {
            assert!(!public_ip(value.parse().unwrap()), "{value}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn 只把基准测试网段识别为_fake_ip() {
        assert!(fake_ip("198.18.1.11".parse().unwrap()));
        assert!(fake_ip("198.19.255.254".parse().unwrap()));
        assert!(!fake_ip("198.51.100.1".parse().unwrap()));
        assert!(!fake_ip("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn 解析安全_dns_json_响应() {
        let response: DohResponse = serde_json::from_value(json!({
            "Status": 0,
            "Answer": [
                {"name":"example.com.","type":1,"TTL":300,"data":"93.184.216.34"},
                {"name":"example.com.","type":5,"TTL":300,"data":"alias.example."}
            ]
        }))
        .unwrap();
        assert_eq!(response.status, 0);
        assert_eq!(response.answer[0].record_type, 1);
        assert_eq!(response.answer[0].data, "93.184.216.34");
    }

    #[test]
    fn 拒绝_ipv6_私网及_ipv4_映射地址() {
        for value in [
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(!public_ip(value.parse().unwrap()), "{value}");
        }
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn html_移除脚本并提取标题正文() {
        let source = r#"<html><head><title>示例</title><style>x</style></head><body>你好<script>secret()</script>世界</body></html>"#;
        let (title, text) = html_text(source.as_bytes());
        assert_eq!(title.as_deref(), Some("示例"));
        assert_eq!(text, "你好 世界");
    }
}
