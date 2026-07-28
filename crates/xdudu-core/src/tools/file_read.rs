//! `file_read`：安全读取工作区内普通文件。

use std::{path::Path, time::Duration};

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncReadExt};

use crate::{SideEffectKind, permission::PermissionLevel};

use super::path_policy::resolve_existing;
use super::{
    Tool, ToolContext, ToolDefinition, ToolResult, object, reject_unknown_fields, required_string,
};

const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;

pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_read",
            description: "读取工作区内文件，可指定行范围。超过 2 MiB 的文件会被截断，并返回 SHA-256。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type":"string","minLength":1,"maxLength":4096,"description":"工作区内文件路径"},
                    "startLine": {"type":"integer","minimum":1},
                    "endLine": {"type":"integer","minimum":1},
                    "maxBytes": {"type":"integer","minimum":1,"maximum":DEFAULT_MAX_BYTES}
                },
                "required": ["path"],
                "additionalProperties": false
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
            &["path", "startLine", "endLine", "maxBytes"],
            &mut issues,
        );
        required_string(map, "path", 4096, &mut issues);
        let start = map
            .get("startLine")
            .map(Value::as_u64)
            .transpose_option("startLine", &mut issues);
        let end = map
            .get("endLine")
            .map(Value::as_u64)
            .transpose_option("endLine", &mut issues);
        let max = map
            .get("maxBytes")
            .map(Value::as_u64)
            .transpose_option("maxBytes", &mut issues);
        if let Some(Some(value)) = start
            && value < 1
        {
            issues.push("startLine 必须大于等于 1。".into());
        }
        if let Some(Some(value)) = end
            && value < 1
        {
            issues.push("endLine 必须大于等于 1。".into());
        }
        if let (Some(Some(start)), Some(Some(end))) = (start, end)
            && end < start
        {
            issues.push("endLine 必须大于或等于 startLine。".into());
        }
        if let Some(Some(value)) = max
            && !(1..=DEFAULT_MAX_BYTES as u64).contains(&value)
        {
            issues.push(format!("maxBytes 必须在 1 到 {DEFAULT_MAX_BYTES} 之间。"));
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        let path_text = input["path"].as_str().unwrap();
        let resolved = match resolve_existing(Path::new(path_text), &context.cwd).await {
            Ok(path) => path,
            Err(error) => {
                let code = if error.message.contains("超出工作区") {
                    "PATH_OUTSIDE_WORKSPACE"
                } else {
                    "FILE_NOT_FOUND"
                };
                return ToolResult::failure(
                    code,
                    error.message,
                    context.started_at,
                    json!({"path":path_text}),
                );
            }
        };
        let metadata = match fs::metadata(&resolved).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                return ToolResult::failure(
                    "NOT_A_FILE",
                    format!("路径不是文件：{path_text}"),
                    context.started_at,
                    json!({"path":path_text}),
                );
            }
            Err(error) => {
                return ToolResult::failure(
                    "FILE_NOT_FOUND",
                    error.to_string(),
                    context.started_at,
                    json!({"path":path_text}),
                );
            }
        };
        let max_bytes = input
            .get("maxBytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES as u64) as usize;
        let file = match fs::File::open(&resolved).await {
            Ok(file) => file,
            Err(error) => {
                return ToolResult::failure(
                    "FILE_READ_ERROR",
                    error.to_string(),
                    context.started_at,
                    json!({"path":path_text}),
                );
            }
        };
        let mut bytes = Vec::with_capacity(max_bytes.min(metadata.len() as usize));
        if let Err(error) = file.take(max_bytes as u64).read_to_end(&mut bytes).await {
            return ToolResult::failure(
                "FILE_READ_ERROR",
                error.to_string(),
                context.started_at,
                json!({"path":path_text}),
            );
        }
        let hash = hex::encode(Sha256::digest(&bytes));
        let content = String::from_utf8_lossy(&bytes);
        let lines = content.split('\n').collect::<Vec<_>>();
        let start = input.get("startLine").and_then(Value::as_u64).unwrap_or(1) as usize;
        let requested_end = input
            .get("endLine")
            .and_then(Value::as_u64)
            .unwrap_or(lines.len() as u64) as usize;
        let end = requested_end.min(lines.len());
        let selected = if start > lines.len() {
            String::new()
        } else {
            lines[start - 1..end].join("\n")
        };
        ToolResult::success(
            json!({
                "path": path_text,
                "content": selected,
                "startLine": start,
                "endLine": end,
                "totalLines": lines.len(),
                "truncated": metadata.len() > max_bytes as u64,
                "sha256": hash,
            }),
            context.started_at,
            json!({"fileSize":metadata.len(),"resolvedPath":resolved}),
        )
    }
}

trait OptionValueExt {
    fn transpose_option(self, name: &str, issues: &mut Vec<String>) -> Option<Option<u64>>;
}

impl OptionValueExt for Option<Option<u64>> {
    fn transpose_option(self, name: &str, issues: &mut Vec<String>) -> Option<Option<u64>> {
        if matches!(self, Some(None)) {
            issues.push(format!("{name} 必须是非负整数。"));
        }
        self
    }
}
