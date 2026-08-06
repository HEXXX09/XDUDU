//! 启动时静默检查 GitHub Releases 最新版本。
//!
//! 只在交互 TUI 启动时异步执行一次；网络失败、解析失败或已是最新时
//! 完全静默，不影响启动与任何本地行为。

use std::time::Duration;

use serde_json::Value;

/// 解析 "v1.2.3" / "1.2.3" 形式版本号。
pub fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
    let cleaned = version.trim().trim_start_matches('v');
    let mut parts = cleaned.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// `latest` 是否严格新于 `installed`；任一无法解析时视为不新（静默）。
pub fn is_newer(installed: &str, latest: &str) -> bool {
    match (parse_version(installed), parse_version(latest)) {
        (Some(current), Some(candidate)) => candidate > current,
        _ => false,
    }
}

/// 请求 GitHub Releases API 获取最新 tag；任何失败返回 None。
pub async fn fetch_latest_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .user_agent(concat!("xdudu/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let response = client
        .get("https://api.github.com/repos/HEXXX09/XDUDU/releases/latest")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    body.get("tag_name")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 版本解析与比较() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.8.0"), Some((0, 8, 0)));
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("1.2"), None);
        assert!(is_newer("0.8.0", "v1.0.0"));
        assert!(is_newer("0.8.0", "0.9.0"));
        assert!(is_newer("0.8.0", "0.8.1"));
        assert!(!is_newer("1.0.0", "0.9.0"));
        assert!(!is_newer("0.8.0", "0.8.0"));
        assert!(!is_newer("0.8.0", "unknown"));
        assert!(!is_newer("unknown", "1.0.0"));
    }
}
