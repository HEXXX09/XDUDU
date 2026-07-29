//! `file_write`：带并发哈希检查和原子落盘的工作区文件写入。

use std::{path::Path, time::Duration};

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::fs;
use uuid::Uuid;

use crate::{
    ChangeSetDraft, ChangeSetFileDraft, ChangeSetStatus, FileOperation, SideEffectKind,
    permission::PermissionLevel,
};

use super::path_policy::resolve_writable;
use super::{
    Tool, ToolContext, ToolDefinition, ToolResult, object, reject_unknown_fields, required_string,
};

const MAX_CONTENT_LENGTH: usize = 2 * 1024 * 1024;

pub struct FileWriteTool;

fn sha256(content: &[u8]) -> String {
    hex::encode(Sha256::digest(content))
}

fn unified_diff(path: &str, old: Option<&str>, new: &str) -> String {
    let new_lines = new.split('\n').collect::<Vec<_>>();
    let Some(old) = old else {
        let additions = new_lines
            .iter()
            .map(|line| format!("+{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        return format!(
            "--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n{additions}\n",
            new_lines.len()
        );
    };
    let old_lines = old.split('\n').collect::<Vec<_>>();
    if old == new {
        return format!(
            "--- a/{path}\n+++ b/{path}\n@@ -1,{} +1,{} @@\n (no changes)\n",
            old_lines.len(),
            new_lines.len()
        );
    }
    let mut common_start = 0;
    while common_start < old_lines.len()
        && common_start < new_lines.len()
        && old_lines[common_start] == new_lines[common_start]
    {
        common_start += 1;
    }
    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > common_start
        && new_end > common_start
        && old_lines[old_end - 1] == new_lines[new_end - 1]
    {
        old_end -= 1;
        new_end -= 1;
    }
    let mut body = Vec::new();
    body.extend(
        old_lines[..common_start]
            .iter()
            .map(|line| format!(" {line}")),
    );
    body.extend(
        old_lines[common_start..old_end]
            .iter()
            .map(|line| format!("-{line}")),
    );
    body.extend(
        new_lines[common_start..new_end]
            .iter()
            .map(|line| format!("+{line}")),
    );
    body.extend(old_lines[old_end..].iter().map(|line| format!(" {line}")));
    format!(
        "--- a/{path}\n+++ b/{path}\n@@ -1,{} +1,{} @@\n{}\n",
        old_lines.len(),
        new_lines.len(),
        body.join("\n")
    )
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn read_optional(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

async fn write_image(path: &Path, image: Option<&[u8]>, mode: Option<u32>) -> std::io::Result<()> {
    if let Some(bytes) = image {
        let temporary = path.with_extension(format!("xdudu-write-{}", Uuid::new_v4()));
        fs::write(&temporary, bytes).await?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode)).await?;
        }
        #[cfg(windows)]
        if fs::try_exists(path).await.unwrap_or(false) {
            fs::remove_file(path).await?;
        }
        fs::rename(temporary, path).await
    } else {
        match fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_write",
            description: "创建或覆盖工作区内文件，返回前后哈希和 unified diff。建议先读取文件并提供 expectedSha256。",
            input_schema: json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","minLength":1,"maxLength":4096},
                    "content":{"type":"string","maxLength":MAX_CONTENT_LENGTH},
                    "createIfMissing":{"type":"boolean"},
                    "expectedSha256":{"type":"string","pattern":"^[a-fA-F0-9]{64}$"}
                },
                "required":["path","content"],
                "additionalProperties":false
            }),
            permission_level: PermissionLevel::WriteFiles,
            side_effect: SideEffectKind::WorkspaceWrite,
            default_timeout: Duration::from_secs(30),
        }
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let map = object(input)?;
        let mut issues = Vec::new();
        reject_unknown_fields(
            map,
            &["path", "content", "createIfMissing", "expectedSha256"],
            &mut issues,
        );
        required_string(map, "path", 4096, &mut issues);
        match map.get("content").and_then(Value::as_str) {
            Some(content) if content.len() <= MAX_CONTENT_LENGTH => {}
            _ => issues.push(format!(
                "content 必须是长度不超过 {MAX_CONTENT_LENGTH} 字节的字符串。"
            )),
        }
        if map
            .get("createIfMissing")
            .is_some_and(|value| !value.is_boolean())
        {
            issues.push("createIfMissing 必须是布尔值。".into());
        }
        if let Some(value) = map.get("expectedSha256") {
            if !value.as_str().is_some_and(valid_hash) {
                issues.push("expectedSha256 必须是 64 位十六进制 SHA-256。".into());
            }
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        let path_text = input["path"].as_str().unwrap();
        let content = input["content"].as_str().unwrap();
        let create_if_missing = input
            .get("createIfMissing")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let expected_hash = input.get("expectedSha256").and_then(Value::as_str);
        let resolved = match resolve_writable(Path::new(path_text), &context.cwd).await {
            Ok(path) => path,
            Err(error) => {
                let code = if error.message.contains("超出工作区") {
                    "PATH_OUTSIDE_WORKSPACE"
                } else {
                    "FILE_WRITE_ERROR"
                };
                return ToolResult::failure(
                    code,
                    error.message,
                    context.started_at,
                    json!({"path":path_text}),
                );
            }
        };
        let old_bytes = match read_optional(&resolved).await {
            Ok(Some(bytes)) => Some(bytes),
            Ok(None) => {
                if !create_if_missing {
                    return ToolResult::failure(
                        "FILE_NOT_FOUND",
                        format!("文件不存在且 createIfMissing 为 false：{path_text}"),
                        context.started_at,
                        json!({"path":path_text}),
                    );
                }
                None
            }
            Err(error) => {
                return ToolResult::failure(
                    "FILE_WRITE_ERROR",
                    error.to_string(),
                    context.started_at,
                    json!({"path":path_text}),
                );
            }
        };
        let pre_hash = old_bytes.as_deref().map(sha256);
        if let Some(expected_hash) = expected_hash
            && pre_hash.as_deref() != Some(expected_hash)
        {
            return ToolResult::failure(
                "HASH_MISMATCH",
                format!(
                    "文件哈希不匹配。预期 {expected_hash}，实际 {}。",
                    pre_hash.as_deref().unwrap_or("<missing>")
                ),
                context.started_at,
                json!({"path":path_text,"expectedSha256":expected_hash,"actualSha256":pre_hash}),
            );
        }
        let Some(parent) = resolved.parent() else {
            return ToolResult::failure(
                "FILE_WRITE_ERROR",
                "目标文件缺少父目录。",
                context.started_at,
                json!({"path":path_text}),
            );
        };
        if let Err(error) = fs::create_dir_all(parent).await {
            return ToolResult::failure(
                "FILE_WRITE_ERROR",
                error.to_string(),
                context.started_at,
                json!({"path":path_text}),
            );
        }
        let post_hash = sha256(content.as_bytes());
        #[cfg(unix)]
        let pre_mode = match fs::metadata(&resolved).await {
            Ok(metadata) => {
                use std::os::unix::fs::PermissionsExt;
                Some(metadata.permissions().mode())
            }
            Err(_) => None,
        };
        #[cfg(not(unix))]
        let pre_mode = None;
        let transaction_id = match context
            .change_ledger
            .prepare_change_set(ChangeSetDraft {
                session_id: context.session_id,
                tool_call_id: context.call_id,
                files: vec![ChangeSetFileDraft {
                    path: Path::new(path_text).to_path_buf(),
                    operation: if old_bytes.is_some() {
                        FileOperation::Modified
                    } else {
                        FileOperation::Created
                    },
                    pre_image: old_bytes.clone(),
                    post_image: Some(content.as_bytes().to_vec()),
                    pre_image_sha256: pre_hash.clone(),
                    post_image_sha256: Some(post_hash.clone()),
                    pre_mode,
                }],
            })
            .await
        {
            Ok(id) => id,
            Err(error) => {
                return ToolResult::failure(
                    "CHANGE_LEDGER_ERROR",
                    error.message,
                    context.started_at,
                    json!({"path":path_text}),
                );
            }
        };
        if let Some(id) = transaction_id
            && let Err(error) = context
                .change_ledger
                .set_change_set_status(id, ChangeSetStatus::Applying)
                .await
        {
            return ToolResult::failure(
                "CHANGE_LEDGER_ERROR",
                error.message,
                context.started_at,
                json!({"path":path_text}),
            );
        }
        let current = match read_optional(&resolved).await {
            Ok(current) => current,
            Err(error) => {
                if let Some(id) = transaction_id {
                    let _ = context
                        .change_ledger
                        .set_change_set_status(id, ChangeSetStatus::RolledBack)
                        .await;
                }
                return ToolResult::failure(
                    "FILE_WRITE_ERROR",
                    format!("提交前无法再次读取文件：{error}"),
                    context.started_at,
                    json!({"path":path_text}),
                );
            }
        };
        if current.as_deref().map(sha256) != pre_hash {
            if let Some(id) = transaction_id {
                let _ = context
                    .change_ledger
                    .set_change_set_status(id, ChangeSetStatus::RolledBack)
                    .await;
            }
            return ToolResult::failure(
                "HASH_MISMATCH",
                "文件在提交前发生变化，拒绝覆盖。",
                context.started_at,
                json!({"path":path_text}),
            );
        }
        if let Err(error) = write_image(&resolved, Some(content.as_bytes()), pre_mode).await {
            if let Some(id) = transaction_id {
                let _ = context
                    .change_ledger
                    .set_change_set_status(id, ChangeSetStatus::RolledBack)
                    .await;
            }
            return ToolResult::failure(
                "FILE_WRITE_ERROR",
                error.to_string(),
                context.started_at,
                json!({"path":path_text}),
            );
        }
        let old_text = old_bytes.as_deref().map(String::from_utf8_lossy);
        if let Some(id) = transaction_id
            && let Err(error) = context
                .change_ledger
                .set_change_set_status(id, ChangeSetStatus::Applied)
                .await
        {
            let rollback = write_image(&resolved, old_bytes.as_deref(), pre_mode).await;
            let message = match rollback {
                Ok(()) => {
                    let _ = context
                        .change_ledger
                        .set_change_set_status(id, ChangeSetStatus::RolledBack)
                        .await;
                    format!("记录变更失败，已恢复原文件：{}", error.message)
                }
                Err(rollback_error) => {
                    let _ = context
                        .change_ledger
                        .set_change_set_status(id, ChangeSetStatus::Conflict)
                        .await;
                    format!(
                        "记录变更失败且恢复原文件失败：{}；恢复错误：{rollback_error}",
                        error.message
                    )
                }
            };
            return ToolResult::failure(
                "CHANGE_LEDGER_ERROR",
                message,
                context.started_at,
                json!({"path":path_text}),
            );
        }
        ToolResult::success(
            json!({
                "path": path_text,
                "created": old_bytes.is_none(),
                "preImageSha256": pre_hash,
                "postImageSha256": post_hash,
                "unifiedDiff": unified_diff(path_text, old_text.as_deref(), content),
                "changeId": transaction_id,
            }),
            context.started_at,
            json!({"resolvedPath":resolved,"contentLength":content.len()}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 差异包含删除和新增行() {
        let diff = unified_diff("a.txt", Some("old"), "new");
        assert!(diff.contains("-old"));
        assert!(diff.contains("+new"));
    }
}
