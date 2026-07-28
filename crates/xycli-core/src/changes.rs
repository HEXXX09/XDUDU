//! 文件变更账本与基于哈希保护的安全撤销。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{fs, sync::Mutex};
use uuid::Uuid;

use crate::error::{ErrorKind, XycliError, XycliResult};

const DEFAULT_CHANGES_DIR: &str = ".xycli/changes/json";

fn sha256(content: &[u8]) -> String {
    hex::encode(Sha256::digest(content))
}

#[derive(Debug, Clone)]
pub struct FileChangeDraft {
    pub session_id: Uuid,
    pub tool_call_id: Uuid,
    pub path: PathBuf,
    pub pre_image: Option<Vec<u8>>,
    pub pre_image_sha256: Option<String>,
    pub post_image_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeStatus {
    Applied,
    Undone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub tool_call_id: Uuid,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_image_hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_image_sha256: Option<String>,
    pub post_image_sha256: String,
    pub status: FileChangeStatus,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undone_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct UndoResult {
    pub change_id: Uuid,
    pub path: PathBuf,
    pub removed_created_file: bool,
}

#[async_trait]
pub trait ChangeLedger: Send + Sync {
    async fn record_file_change(&self, draft: FileChangeDraft) -> XycliResult<Option<Uuid>>;
}

#[derive(Debug, Default)]
pub struct NoopChangeLedger;

#[async_trait]
impl ChangeLedger for NoopChangeLedger {
    async fn record_file_change(&self, _draft: FileChangeDraft) -> XycliResult<Option<Uuid>> {
        Ok(None)
    }
}

pub struct JsonChangeLedger {
    cwd: PathBuf,
    changes_dir: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl JsonChangeLedger {
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        let cwd = cwd.as_ref().to_path_buf();
        Self {
            changes_dir: cwd.join(DEFAULT_CHANGES_DIR),
            cwd,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn with_dir(cwd: impl AsRef<Path>, changes_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.as_ref().to_path_buf(),
            changes_dir: changes_dir.into(),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    fn record_path(&self, id: Uuid) -> PathBuf {
        self.changes_dir.join(format!("{id}.json"))
    }

    async fn atomic_write_record(&self, record: &FileChangeRecord) -> XycliResult<()> {
        let _guard = self.write_lock.lock().await;
        fs::create_dir_all(&self.changes_dir).await?;
        let path = self.record_path(record.id);
        let temporary = path.with_extension(format!("json.{}.tmp", Uuid::new_v4()));
        let data = serde_json::to_vec_pretty(record)?;
        if let Err(error) = async {
            fs::write(&temporary, data).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
            }
            fs::rename(&temporary, &path).await
        }
        .await
        {
            let _ = fs::remove_file(&temporary).await;
            return Err(XycliError::tool(format!("写入变更账本失败：{error}")));
        }
        Ok(())
    }

    async fn read_record(&self, id: Uuid) -> XycliResult<Option<FileChangeRecord>> {
        match fs::read(self.record_path(id)).await {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(XycliError::tool(format!("读取变更记录失败：{error}"))),
        }
    }

    pub async fn list(&self, session_id: Option<Uuid>) -> XycliResult<Vec<FileChangeRecord>> {
        let mut entries = match fs::read_dir(&self.changes_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(XycliError::tool(format!("读取变更账本失败：{error}"))),
        };
        let mut records = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Ok(data) = fs::read(entry.path()).await
                && let Ok(record) = serde_json::from_slice::<FileChangeRecord>(&data)
                && session_id.is_none_or(|session_id| record.session_id == session_id)
            {
                records.push(record);
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        Ok(records)
    }

    async fn resolve_current_file(&self, relative: &Path) -> XycliResult<PathBuf> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(XycliError::new(
                ErrorKind::PermissionDenied,
                "变更记录包含不安全路径，拒绝撤销。",
            ));
        }
        let root = fs::canonicalize(&self.cwd).await?;
        let target = fs::canonicalize(self.cwd.join(relative))
            .await
            .map_err(|error| XycliError::tool(format!("撤销目标不存在或不可访问：{error}")))?;
        if !target.starts_with(&root) {
            return Err(XycliError::new(
                ErrorKind::PermissionDenied,
                "撤销目标已经指向工作区外部，拒绝操作。",
            ));
        }
        Ok(target)
    }

    pub async fn undo(
        &self,
        change_id: Option<Uuid>,
        session_id: Option<Uuid>,
    ) -> XycliResult<UndoResult> {
        let mut record = if let Some(change_id) = change_id {
            self.read_record(change_id)
                .await?
                .ok_or_else(|| XycliError::validation(format!("找不到变更记录：{change_id}")))?
        } else {
            self.list(session_id)
                .await?
                .into_iter()
                .find(|record| record.status == FileChangeStatus::Applied)
                .ok_or_else(|| XycliError::validation("没有可撤销的文件变更。"))?
        };
        if let Some(session_id) = session_id
            && record.session_id != session_id
        {
            return Err(XycliError::validation(
                "指定变更不属于要求的会话，拒绝撤销。",
            ));
        }
        if record.status != FileChangeStatus::Applied {
            return Err(XycliError::validation("该变更已经撤销。"));
        }
        let target = self.resolve_current_file(&record.path).await?;
        let current = fs::read(&target).await?;
        let actual_hash = sha256(&current);
        if actual_hash != record.post_image_sha256 {
            return Err(XycliError::tool(format!(
                "文件在 Agent 写入后又发生变化，拒绝撤销：{}；记录哈希 {}，当前哈希 {}。",
                record.path.display(),
                record.post_image_sha256,
                actual_hash
            )));
        }
        let removed_created_file = if let Some(pre_image_hex) = &record.pre_image_hex {
            let pre_image = hex::decode(pre_image_hex)
                .map_err(|error| XycliError::tool(format!("变更前镜像损坏：{error}")))?;
            let pre_image_hash = sha256(&pre_image);
            if record.pre_image_sha256.as_deref() != Some(pre_image_hash.as_str()) {
                return Err(XycliError::tool("变更前镜像哈希不匹配，拒绝撤销。"));
            }
            let temporary = target.with_extension(format!("xycli-undo-{}", Uuid::new_v4()));
            if let Err(error) = async {
                fs::write(&temporary, pre_image).await?;
                #[cfg(windows)]
                fs::remove_file(&target).await?;
                fs::rename(&temporary, &target).await
            }
            .await
            {
                let _ = fs::remove_file(&temporary).await;
                return Err(XycliError::tool(format!("恢复文件失败：{error}")));
            }
            false
        } else {
            fs::remove_file(&target).await?;
            true
        };
        record.status = FileChangeStatus::Undone;
        record.undone_at = Some(Utc::now());
        self.atomic_write_record(&record).await?;
        Ok(UndoResult {
            change_id: record.id,
            path: record.path,
            removed_created_file,
        })
    }
}

#[async_trait]
impl ChangeLedger for JsonChangeLedger {
    async fn record_file_change(&self, draft: FileChangeDraft) -> XycliResult<Option<Uuid>> {
        let id = Uuid::new_v4();
        let record = FileChangeRecord {
            id,
            session_id: draft.session_id,
            tool_call_id: draft.tool_call_id,
            path: draft.path,
            pre_image_hex: draft.pre_image.map(hex::encode),
            pre_image_sha256: draft.pre_image_sha256,
            post_image_sha256: draft.post_image_sha256,
            status: FileChangeStatus::Applied,
            created_at: Utc::now(),
            undone_at: None,
        };
        self.atomic_write_record(&record).await?;
        Ok(Some(id))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    async fn record(
        ledger: &JsonChangeLedger,
        path: &str,
        before: Option<&[u8]>,
        after: &[u8],
    ) -> Uuid {
        ledger
            .record_file_change(FileChangeDraft {
                session_id: Uuid::new_v4(),
                tool_call_id: Uuid::new_v4(),
                path: PathBuf::from(path),
                pre_image: before.map(ToOwned::to_owned),
                pre_image_sha256: before.map(sha256),
                post_image_sha256: sha256(after),
            })
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn 撤销恢复旧文件并删除新文件() {
        let dir = tempdir().unwrap();
        let ledger = JsonChangeLedger::with_dir(dir.path(), dir.path().join("ledger"));
        fs::write(dir.path().join("existing.txt"), b"after")
            .await
            .unwrap();
        let existing = record(&ledger, "existing.txt", Some(b"before"), b"after").await;
        ledger.undo(Some(existing), None).await.unwrap();
        assert_eq!(
            fs::read(dir.path().join("existing.txt")).await.unwrap(),
            b"before"
        );

        fs::write(dir.path().join("created.txt"), b"created")
            .await
            .unwrap();
        let created = record(&ledger, "created.txt", None, b"created").await;
        ledger.undo(Some(created), None).await.unwrap();
        assert!(!dir.path().join("created.txt").exists());
    }

    #[tokio::test]
    async fn 文件被用户修改后拒绝撤销() {
        let dir = tempdir().unwrap();
        let ledger = JsonChangeLedger::with_dir(dir.path(), dir.path().join("ledger"));
        fs::write(dir.path().join("a.txt"), b"agent").await.unwrap();
        let id = record(&ledger, "a.txt", Some(b"before"), b"agent").await;
        fs::write(dir.path().join("a.txt"), b"user").await.unwrap();
        assert!(ledger.undo(Some(id), None).await.is_err());
        assert_eq!(fs::read(dir.path().join("a.txt")).await.unwrap(), b"user");
    }
}
