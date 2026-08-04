//! `search_text`：遵守忽略规则、结果有界的工作区文本搜索。

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use crate::{SideEffectKind, permission::PermissionLevel};

use super::path_policy::resolve_directory;
use super::{
    Tool, ToolContext, ToolDefinition, ToolProgressUpdate, ToolResult, object,
    reject_unknown_fields, required_string,
};

const MAX_QUERY_BYTES: usize = 1024;
const MAX_PATTERN_ITEMS: usize = 32;
const MAX_PATTERN_BYTES: usize = 256;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SCANNED_FILES: u64 = 20_000;
const MAX_SCANNED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RESULTS: u64 = 500;
const DEFAULT_RESULTS: u64 = 100;
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_LINE_BYTES: usize = 4096;

pub struct SearchTextTool;

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn string_array(map: &serde_json::Map<String, Value>, key: &str, issues: &mut Vec<String>) {
    if let Some(value) = map.get(key) {
        let valid = value.as_array().is_some_and(|values| {
            values.len() <= MAX_PATTERN_ITEMS
                && values.iter().all(|value| {
                    value
                        .as_str()
                        .is_some_and(|value| !value.is_empty() && value.len() <= MAX_PATTERN_BYTES)
                })
        });
        if !valid {
            issues.push(format!(
                "{key} 必须是最多 {MAX_PATTERN_ITEMS} 个非空字符串组成的数组，单项不超过 {MAX_PATTERN_BYTES} 字节。"
            ));
        }
    }
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| format!("非法 glob“{pattern}”：{error}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|error| format!("构建 glob 失败：{error}"))
}

fn build_regex(query: &str, mode: &str, case_sensitive: bool) -> Result<Regex, String> {
    let pattern = if mode == "literal" {
        regex::escape(query)
    } else {
        query.to_owned()
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|error| format!("正则表达式无效：{error}"))
}

fn ignored_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| matches!(name, ".git" | ".xdudu" | ".xycli" | "target"))
}

#[derive(Debug)]
struct SearchOptions {
    query: String,
    mode: String,
    case_sensitive: bool,
    includes: Vec<String>,
    excludes: Vec<String>,
    context_lines: usize,
    max_results: usize,
}

fn run_search(
    root: PathBuf,
    workspace: PathBuf,
    options: SearchOptions,
    cancellation: tokio_util::sync::CancellationToken,
    progress: Option<tokio::sync::mpsc::Sender<ToolProgressUpdate>>,
) -> Result<Value, String> {
    let regex = build_regex(&options.query, &options.mode, options.case_sensitive)?;
    let include_set = build_globset(&options.includes)?;
    let exclude_set = build_globset(&options.excludes)?;
    let has_includes = !options.includes.is_empty();
    let mut matches = Vec::new();
    let mut matched_files = HashSet::new();
    let mut scanned_files = 0_u64;
    let mut scanned_bytes = 0_u64;
    let mut output_bytes = 0_usize;
    let mut truncated = false;
    let mut truncation_reason: Option<&str> = None;
    let mut last_progress = Instant::now();

    let mut walker = WalkBuilder::new(&root);
    let filter_root = root.clone();
    walker
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .parents(true)
        .filter_entry(move |entry| entry.path() == filter_root || !ignored_directory(entry.path()));

    for entry in walker.build().filter_map(Result::ok) {
        if cancellation.is_cancelled() {
            return Err("搜索已取消。".into());
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        if scanned_files >= MAX_SCANNED_FILES {
            truncated = true;
            truncation_reason = Some("scanned_files_limit");
            break;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        if scanned_bytes.saturating_add(metadata.len()) > MAX_SCANNED_BYTES {
            truncated = true;
            truncation_reason = Some("scanned_bytes_limit");
            break;
        }
        let relative = match entry.path().strip_prefix(&workspace) {
            Ok(path) => path,
            Err(_) => continue,
        };
        let normalized = relative.to_string_lossy().replace('\\', "/");
        if has_includes && !include_set.is_match(&normalized) {
            continue;
        }
        if exclude_set.is_match(&normalized) {
            continue;
        }
        scanned_files += 1;
        scanned_bytes += metadata.len();
        if scanned_files % 1000 == 0 || last_progress.elapsed() >= Duration::from_millis(250) {
            if let Some(progress) = &progress {
                let _ = progress.try_send(ToolProgressUpdate::counted(
                    "scanning",
                    scanned_files,
                    Some(MAX_SCANNED_FILES),
                    "files",
                ));
            }
            last_progress = Instant::now();
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        if bytes.iter().take(8192).any(|byte| *byte == 0) {
            continue;
        }
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let lines = content.lines().collect::<Vec<_>>();
        for (line_index, line) in lines.iter().enumerate() {
            for found in regex.find_iter(line) {
                if matches.len() >= options.max_results {
                    truncated = true;
                    truncation_reason = Some("max_results");
                    break;
                }
                let start = line_index.saturating_sub(options.context_lines);
                let end = (line_index + options.context_lines + 1).min(lines.len());
                let before = lines[start..line_index]
                    .iter()
                    .map(|line| bounded_text(line, MAX_LINE_BYTES))
                    .collect::<Vec<_>>();
                let after = lines[line_index + 1..end]
                    .iter()
                    .map(|line| bounded_text(line, MAX_LINE_BYTES))
                    .collect::<Vec<_>>();
                let line_text = bounded_text(line, MAX_LINE_BYTES);
                let matched_text = bounded_text(found.as_str(), MAX_LINE_BYTES);
                let estimated = normalized.len()
                    + line_text.len()
                    + matched_text.len()
                    + before.iter().map(String::len).sum::<usize>()
                    + after.iter().map(String::len).sum::<usize>()
                    + 128;
                if output_bytes.saturating_add(estimated) > MAX_OUTPUT_BYTES {
                    truncated = true;
                    truncation_reason = Some("output_bytes_limit");
                    break;
                }
                output_bytes += estimated;
                matched_files.insert(normalized.clone());
                matches.push(json!({
                    "path": normalized,
                    "line": line_index + 1,
                    "column": line[..found.start()].chars().count() + 1,
                    "matchedText": matched_text,
                    "lineText": line_text,
                    "before": before,
                    "after": after,
                }));
            }
            if truncated {
                break;
            }
        }
        if truncated {
            break;
        }
    }

    let next_action_hint = matches.is_empty().then_some(
        "本地未找到匹配项。若用户询问通用知识、查询、研究或时效性信息且未限制仅使用本地资料，请改用 web_search；若问题明确属于当前项目，请调整关键词或搜索范围。",
    );
    Ok(json!({
        "matches": matches,
        "matchedFiles": matched_files.len(),
        "scannedFiles": scanned_files,
        "truncated": truncated,
        "truncationReason": truncation_reason,
        "nextActionHint": next_action_hint,
    }))
}

#[async_trait]
impl Tool for SearchTextTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search_text".into(),
            description:
                "在工作区内搜索文本或正则表达式，遵守忽略规则并返回有界的行号、列号和上下文。"
                    .into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "query":{"type":"string","minLength":1,"maxLength":MAX_QUERY_BYTES},
                    "path":{"type":"string","minLength":1,"maxLength":4096},
                    "mode":{"type":"string","enum":["literal","regex"]},
                    "caseSensitive":{"type":"boolean"},
                    "include":{"type":"array","maxItems":MAX_PATTERN_ITEMS,"items":{"type":"string","minLength":1,"maxLength":MAX_PATTERN_BYTES}},
                    "exclude":{"type":"array","maxItems":MAX_PATTERN_ITEMS,"items":{"type":"string","minLength":1,"maxLength":MAX_PATTERN_BYTES}},
                    "contextLines":{"type":"integer","minimum":0,"maximum":5},
                    "maxResults":{"type":"integer","minimum":1,"maximum":MAX_RESULTS}
                },
                "required":["query"],
                "additionalProperties":false
            }),
            permission_level: PermissionLevel::ReadOnly,
            side_effect: SideEffectKind::None,
            default_timeout: Duration::from_secs(30),
        }
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let map = object(input)?;
        let mut issues = Vec::new();
        reject_unknown_fields(
            map,
            &[
                "query",
                "path",
                "mode",
                "caseSensitive",
                "include",
                "exclude",
                "contextLines",
                "maxResults",
            ],
            &mut issues,
        );
        let query = required_string(map, "query", MAX_QUERY_BYTES, &mut issues);
        if let Some(path) = map.get("path")
            && !path
                .as_str()
                .is_some_and(|path| !path.is_empty() && path.len() <= 4096)
        {
            issues.push("path 必须是 1 到 4096 字节的字符串。".into());
        }
        let mode = map.get("mode").and_then(Value::as_str).unwrap_or("literal");
        if !matches!(mode, "literal" | "regex") {
            issues.push("mode 只能是 literal 或 regex。".into());
        }
        if map
            .get("caseSensitive")
            .is_some_and(|value| !value.is_boolean())
        {
            issues.push("caseSensitive 必须是布尔值。".into());
        }
        string_array(map, "include", &mut issues);
        string_array(map, "exclude", &mut issues);
        if !map
            .get("contextLines")
            .is_none_or(|value| value.as_u64().is_some_and(|value| value <= 5))
        {
            issues.push("contextLines 必须是 0 到 5 的整数。".into());
        }
        if !map.get("maxResults").is_none_or(|value| {
            value
                .as_u64()
                .is_some_and(|value| (1..=MAX_RESULTS).contains(&value))
        }) {
            issues.push(format!("maxResults 必须是 1 到 {MAX_RESULTS} 的整数。"));
        }
        if let Some(query) = query
            && mode == "regex"
            && let Err(error) = build_regex(query, mode, true)
        {
            issues.push(error);
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        let requested_path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let root = match resolve_directory(Path::new(requested_path), &context.cwd).await {
            Ok(path) => path,
            Err(error) => {
                return ToolResult::failure(
                    "PATH_OUTSIDE_WORKSPACE",
                    error.message,
                    context.started_at,
                    json!({"path":requested_path}),
                );
            }
        };
        let workspace = match tokio::fs::canonicalize(&context.cwd).await {
            Ok(path) => path,
            Err(error) => {
                return ToolResult::failure(
                    "SEARCH_ERROR",
                    error.to_string(),
                    context.started_at,
                    json!({}),
                );
            }
        };
        let strings = |key: &str| {
            input
                .get(key)
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let options = SearchOptions {
            query: input["query"].as_str().unwrap_or_default().to_owned(),
            mode: input
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("literal")
                .to_owned(),
            case_sensitive: input
                .get("caseSensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            includes: strings("include"),
            excludes: strings("exclude"),
            context_lines: input
                .get("contextLines")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            max_results: input
                .get("maxResults")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_RESULTS) as usize,
        };
        let cancellation = context.cancellation.clone();
        let progress = context.progress.clone();
        match tokio::task::spawn_blocking(move || {
            run_search(root, workspace, options, cancellation, progress)
        })
        .await
        {
            Ok(Ok(output)) => {
                ToolResult::success(output, context.started_at, json!({"path":requested_path}))
            }
            Ok(Err(message)) => ToolResult::failure(
                "SEARCH_ERROR",
                message,
                context.started_at,
                json!({"path":requested_path}),
            ),
            Err(error) => ToolResult::failure(
                "SEARCH_ERROR",
                format!("搜索任务失败：{error}"),
                context.started_at,
                json!({"path":requested_path}),
            ),
        }
    }
}
