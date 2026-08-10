//! 可审查记忆：存储、编辑、全文检索与删除。
//!
//! Agent 在任务完成后可以自主提炼可长期复用的信息，也可以返回空集合。
//! 自动记忆不弹出逐条审批，但内容始终脱敏并保留来源与时间；用户可随时
//! 列出、修改或删除。检索复用 SQLite FTS5，不引入向量数据库。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{XduduError, error::XduduResult, redaction::redact_text};

/// 单条记忆的最大字节数。
pub const MAX_MEMORY_BYTES: usize = 4096;
pub const MAX_MEMORY_DOCUMENT_BYTES: usize = 32 * 1024;
pub const MEMORY_DOCUMENT_PATH: &str = ".xdudu/memories/MEMORY.md";

pub fn memory_document_path(cwd: &Path) -> PathBuf {
    cwd.join(MEMORY_DOCUMENT_PATH)
}

pub fn read_memory_document(cwd: &Path) -> XduduResult<Option<String>> {
    let path = memory_document_path(cwd);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(XduduError::validation(
            "长期记忆文件必须是普通文件，不能是符号链接。",
        ));
    }
    if metadata.len() > MAX_MEMORY_DOCUMENT_BYTES as u64 {
        return Err(XduduError::validation("长期记忆文件超过 32 KiB 上限。"));
    }
    Ok(Some(fs::read_to_string(path)?))
}

pub fn write_memory_document(cwd: &Path, content: &str) -> XduduResult<PathBuf> {
    let content = redact_text(content).trim().to_owned();
    if content.is_empty() || content.len() > MAX_MEMORY_DOCUMENT_BYTES {
        return Err(XduduError::validation(
            "长期记忆文档不能为空且不能超过 32 KiB。",
        ));
    }
    let path = memory_document_path(cwd);
    let parent = path
        .parent()
        .ok_or_else(|| XduduError::validation("记忆路径无效。"))?;
    fs::create_dir_all(parent)?;
    if fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(XduduError::validation("长期记忆目录不能是符号链接。"));
    }
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(XduduError::validation("长期记忆文件不能是符号链接。"));
    }
    let temporary = parent.join(format!(".MEMORY.{}.tmp", Uuid::new_v4()));
    fs::write(&temporary, format!("{content}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&temporary, &path)?;
    Ok(path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecord {
    pub id: Uuid,
    pub content: String,
    pub source_session_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 记忆存储接口；SqliteSessionStore 实现。
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// 写入一条记忆；内容先脱敏，超长或空内容拒绝。
    async fn add_memory(
        &self,
        content: &str,
        source_session_id: Option<Uuid>,
    ) -> XduduResult<MemoryRecord>;
    /// 列出记忆，按创建时间倒序。
    async fn list_memories(&self, limit: usize) -> XduduResult<Vec<MemoryRecord>>;
    /// 按 ID 修改记忆，并原子同步全文索引；不存在时返回 None。
    async fn update_memory(&self, id: Uuid, content: &str) -> XduduResult<Option<MemoryRecord>>;
    /// 按 ID 删除记忆（硬删除并同步全文索引）。
    async fn remove_memory(&self, id: Uuid) -> XduduResult<bool>;
    /// 用 FTS5 全文检索相关记忆，按相关性排序。
    async fn search_memories(&self, query: &str, limit: usize) -> XduduResult<Vec<MemoryRecord>>;
}

pub(crate) fn sanitize_memory_content(content: &str) -> Option<String> {
    let redacted = redact_text(content);
    let trimmed = redacted.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_MEMORY_BYTES {
        return None;
    }
    Some(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn 记忆内容脱敏且限制长度() {
        assert_eq!(
            sanitize_memory_content("用户偏好 sk-abcdefghijk"),
            Some("用户偏好 [已脱敏]".into())
        );
        assert!(sanitize_memory_content("   ").is_none());
        assert!(sanitize_memory_content(&"x".repeat(MAX_MEMORY_BYTES + 1)).is_none());
    }

    #[test]
    fn memory文档原子写入读取并脱敏() {
        let directory = tempdir().unwrap();
        let path = write_memory_document(
            directory.path(),
            "# XDUDU 长期记忆\n\n- 密钥 sk-abcdefghijk",
        )
        .unwrap();
        assert_eq!(path, memory_document_path(directory.path()));
        let content = read_memory_document(directory.path()).unwrap().unwrap();
        assert!(content.starts_with("# XDUDU 长期记忆"));
        assert!(content.contains("[已脱敏]"));
        assert!(!content.contains("sk-abcdefghijk"));
    }
}
