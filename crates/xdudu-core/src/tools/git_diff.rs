//! `git_diff`：固定参数、输出有界的工作区或暂存区差异。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{SideEffectKind, permission::PermissionLevel};

use super::git_common::{repository_root, run_git, safe_relative_path};
use super::{Tool, ToolContext, ToolDefinition, ToolResult, object, reject_unknown_fields};

const DEFAULT_MAX_BYTES: u64 = 512 * 1024;
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PATHS: usize = 50;

pub struct GitDiffTool;

fn parse_name_status(bytes: &[u8]) -> Vec<Value> {
    let fields = bytes
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let status = &fields[index];
        let kind = status.chars().next().unwrap_or('?');
        if matches!(kind, 'R' | 'C') && index + 2 < fields.len() {
            files.push(json!({
                "status": kind.to_string(),
                "score": status.get(1..),
                "originalPath": fields[index + 1],
                "path": fields[index + 2],
            }));
            index += 3;
        } else if index + 1 < fields.len() {
            files.push(json!({
                "status": kind.to_string(),
                "score": Value::Null,
                "originalPath": Value::Null,
                "path": fields[index + 1],
            }));
            index += 2;
        } else {
            break;
        }
    }
    files
}

fn paths(input: &Value) -> Vec<String> {
    input
        .get("paths")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn base_args(input: &Value) -> Vec<String> {
    let scope = input
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("worktree");
    let context_lines = input
        .get("contextLines")
        .and_then(Value::as_u64)
        .unwrap_or(3);
    let mut args = vec![
        "--no-optional-locks".to_owned(),
        "diff".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned(),
        format!("--unified={context_lines}"),
        "--src-prefix=a/".to_owned(),
        "--dst-prefix=b/".to_owned(),
    ];
    if scope == "staged" {
        args.push("--cached".to_owned());
    }
    args.push("--".to_owned());
    args.extend(paths(input));
    args
}

#[async_trait]
impl Tool for GitDiffTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_diff".into(),
            description: "返回工作区或暂存区的 Git unified diff 和结构化文件状态，不执行外部 diff 或 textconv。".into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "scope":{"type":"string","enum":["worktree","staged"]},
                    "paths":{"type":"array","maxItems":MAX_PATHS,"items":{"type":"string","minLength":1,"maxLength":4096}},
                    "contextLines":{"type":"integer","minimum":0,"maximum":20},
                    "maxBytes":{"type":"integer","minimum":1,"maximum":MAX_BYTES}
                },
                "additionalProperties":false
            }),
            permission_level: PermissionLevel::ReadOnly,
            side_effect: SideEffectKind::None,
            default_timeout: Duration::from_secs(20),
        }
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let map = object(input)?;
        let mut issues = Vec::new();
        reject_unknown_fields(
            map,
            &["scope", "paths", "contextLines", "maxBytes"],
            &mut issues,
        );
        if !map.get("scope").is_none_or(|value| {
            value
                .as_str()
                .is_some_and(|value| matches!(value, "worktree" | "staged"))
        }) {
            issues.push("scope 只能是 worktree 或 staged。".into());
        }
        if let Some(value) = map.get("paths") {
            match value.as_array() {
                Some(values)
                    if values.len() <= MAX_PATHS
                        && values
                            .iter()
                            .all(|value| value.as_str().is_some_and(safe_relative_path)) => {}
                _ => issues.push(format!(
                    "paths 必须是最多 {MAX_PATHS} 个安全工作区相对路径组成的数组。"
                )),
            }
        }
        if !map
            .get("contextLines")
            .is_none_or(|value| value.as_u64().is_some_and(|value| value <= 20))
        {
            issues.push("contextLines 必须是 0 到 20 的整数。".into());
        }
        if !map.get("maxBytes").is_none_or(|value| {
            value
                .as_u64()
                .is_some_and(|value| (1..=MAX_BYTES).contains(&value))
        }) {
            issues.push(format!("maxBytes 必须是 1 到 {MAX_BYTES} 的整数。"));
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        let (workspace, root) = match repository_root(&context.cwd).await {
            Ok(value) => value,
            Err(message) => {
                return ToolResult::failure(
                    "NOT_GIT_REPOSITORY",
                    message,
                    context.started_at,
                    json!({}),
                );
            }
        };
        let max_bytes = input
            .get("maxBytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES) as usize;
        let diff = match run_git(&workspace, &root, &base_args(&input), max_bytes).await {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                return ToolResult::failure(
                    "GIT_DIFF_ERROR",
                    String::from_utf8_lossy(&output.stderr).trim(),
                    context.started_at,
                    json!({}),
                );
            }
            Err(message) => {
                return ToolResult::failure(
                    "GIT_DIFF_ERROR",
                    message,
                    context.started_at,
                    json!({}),
                );
            }
        };
        let mut name_args = base_args(&input);
        name_args.retain(|arg| !arg.starts_with("--unified="));
        let separator = name_args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(name_args.len());
        name_args.insert(separator, "--name-status".to_owned());
        name_args.insert(separator + 1, "-z".to_owned());
        let files = match run_git(&workspace, &root, &name_args, 2 * 1024 * 1024).await {
            Ok(output) if output.status.success() => parse_name_status(&output.stdout),
            _ => Vec::new(),
        };
        let bytes_returned = diff.stdout.len();
        ToolResult::success(
            json!({
                "scope": input.get("scope").and_then(Value::as_str).unwrap_or("worktree"),
                "diff": String::from_utf8_lossy(&diff.stdout),
                "files": files,
                "bytesReturned": bytes_returned,
                "truncated": diff.stdout_truncated,
            }),
            context.started_at,
            json!({"repositoryRoot":root}),
        )
    }
}
