//! 可审查记忆：存储、全文检索与删除。
//!
//! 记忆由用户显式确认后写入（任务完成后的建议流程），默认不自动写入。
//! 内容在落盘前统一脱敏，保留来源会话与时间供审计；检索复用 SQLite
//! FTS5 本地全文索引，不引入向量数据库。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{error::XduduResult, redaction::redact_text};

/// 单条记忆的最大字节数。
pub const MAX_MEMORY_BYTES: usize = 4096;

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

    #[test]
    fn 记忆内容脱敏且限制长度() {
        assert_eq!(
            sanitize_memory_content("用户偏好 sk-abcdefghijk"),
            Some("用户偏好 [已脱敏]".into())
        );
        assert!(sanitize_memory_content("   ").is_none());
        assert!(sanitize_memory_content(&"x".repeat(MAX_MEMORY_BYTES + 1)).is_none());
    }
}
