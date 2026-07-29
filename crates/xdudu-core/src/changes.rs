//! 文件变更事务、崩溃恢复与基于哈希保护的整批撤销。

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

use crate::error::{ErrorKind, XduduError, XduduResult};

const DEFAULT_CHANGES_DIR: &str = ".xdudu/changes/json";

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

/// v1 单文件记录。保留反序列化能力，保证旧账本仍可撤销。
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOperation {
    Created,
    Modified,
    Deleted,
}

impl FileOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChangeSetFileDraft {
    pub path: PathBuf,
    pub operation: FileOperation,
    pub pre_image: Option<Vec<u8>>,
    pub post_image: Option<Vec<u8>>,
    pub pre_image_sha256: Option<String>,
    pub post_image_sha256: Option<String>,
    pub pre_mode: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ChangeSetDraft {
    pub session_id: Uuid,
    pub tool_call_id: Uuid,
    pub files: Vec<ChangeSetFileDraft>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSetStatus {
    Prepared,
    Applying,
    Applied,
    RolledBack,
    Undone,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSetFileRecord {
    pub path: PathBuf,
    pub operation: FileOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_image_hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_image_hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_image_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_image_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSetRecord {
    pub schema_version: u32,
    pub id: Uuid,
    pub session_id: Uuid,
    pub tool_call_id: Uuid,
    pub files: Vec<ChangeSetFileRecord>,
    pub status: ChangeSetStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undone_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct UndoResult {
    pub change_id: Uuid,
    pub paths: Vec<PathBuf>,
    pub removed_created_files: usize,
}

#[async_trait]
pub trait ChangeLedger: Send + Sync {
    async fn prepare_change_set(&self, draft: ChangeSetDraft) -> XduduResult<Option<Uuid>>;
    async fn set_change_set_status(&self, id: Uuid, status: ChangeSetStatus) -> XduduResult<()>;

    async fn record_file_change(&self, draft: FileChangeDraft) -> XduduResult<Option<Uuid>> {
        let id = self
            .prepare_change_set(ChangeSetDraft {
                session_id: draft.session_id,
                tool_call_id: draft.tool_call_id,
                files: vec![ChangeSetFileDraft {
                    path: draft.path,
                    operation: if draft.pre_image.is_some() {
                        FileOperation::Modified
                    } else {
                        FileOperation::Created
                    },
                    pre_image: draft.pre_image,
                    post_image: None,
                    pre_image_sha256: draft.pre_image_sha256,
                    post_image_sha256: Some(draft.post_image_sha256),
                    pre_mode: None,
                }],
            })
            .await?;
        if let Some(id) = id {
            self.set_change_set_status(id, ChangeSetStatus::Applied)
                .await?;
        }
        Ok(id)
    }
}

#[derive(Debug, Default)]
pub struct NoopChangeLedger;

#[async_trait]
impl ChangeLedger for NoopChangeLedger {
    async fn prepare_change_set(&self, _draft: ChangeSetDraft) -> XduduResult<Option<Uuid>> {
        Ok(None)
    }

    async fn set_change_set_status(&self, _id: Uuid, _status: ChangeSetStatus) -> XduduResult<()> {
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordVersion {
    #[serde(default)]
    schema_version: u32,
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

    async fn atomic_write<T: Serialize>(&self, id: Uuid, record: &T) -> XduduResult<()> {
        let _guard = self.write_lock.lock().await;
        fs::create_dir_all(&self.changes_dir).await?;
        let path = self.record_path(id);
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
            return Err(XduduError::tool(format!("写入变更账本失败：{error}")));
        }
        Ok(())
    }

    async fn read_bytes(&self, id: Uuid) -> XduduResult<Option<Vec<u8>>> {
        match fs::read(self.record_path(id)).await {
            Ok(data) => Ok(Some(data)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(XduduError::tool(format!("读取变更记录失败：{error}"))),
        }
    }

    async fn read_change_set(&self, id: Uuid) -> XduduResult<Option<ChangeSetRecord>> {
        let Some(data) = self.read_bytes(id).await? else {
            return Ok(None);
        };
        let version: RecordVersion = serde_json::from_slice(&data)?;
        if version.schema_version != 2 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&data)?))
    }

    async fn read_legacy(&self, id: Uuid) -> XduduResult<Option<FileChangeRecord>> {
        let Some(data) = self.read_bytes(id).await? else {
            return Ok(None);
        };
        let version: RecordVersion = serde_json::from_slice(&data)?;
        if version.schema_version == 2 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&data)?))
    }

    async fn all_record_bytes(&self) -> XduduResult<Vec<Vec<u8>>> {
        let mut entries = match fs::read_dir(&self.changes_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(XduduError::tool(format!("读取变更账本失败：{error}"))),
        };
        let mut records = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Ok(data) = fs::read(entry.path()).await {
                records.push(data);
            }
        }
        Ok(records)
    }

    async fn safe_target(&self, relative: &Path) -> XduduResult<PathBuf> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(XduduError::new(
                ErrorKind::PermissionDenied,
                "变更记录包含不安全路径，拒绝操作。",
            ));
        }
        let root = fs::canonicalize(&self.cwd).await?;
        let candidate = root.join(relative);
        let mut ancestor = candidate.as_path();
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| XduduError::tool("变更路径缺少安全父目录。"))?;
        }
        let real_ancestor = fs::canonicalize(ancestor).await?;
        if !real_ancestor.starts_with(&root) {
            return Err(XduduError::new(
                ErrorKind::PermissionDenied,
                "变更目标已经指向工作区外部，拒绝操作。",
            ));
        }
        let suffix = candidate
            .strip_prefix(ancestor)
            .map_err(|_| XduduError::tool("无法解析变更目标路径。"))?;
        if suffix.as_os_str().is_empty() {
            Ok(real_ancestor)
        } else {
            Ok(real_ancestor.join(suffix))
        }
    }

    async fn current_hash(&self, path: &Path) -> XduduResult<Option<String>> {
        match fs::read(path).await {
            Ok(bytes) => Ok(Some(sha256(&bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(XduduError::tool(format!(
                "读取变更目标失败 {}：{error}",
                path.display()
            ))),
        }
    }

    async fn write_state(
        &self,
        path: &Path,
        image_hex: Option<&str>,
        mode: Option<u32>,
    ) -> XduduResult<()> {
        if let Some(image_hex) = image_hex {
            let bytes = hex::decode(image_hex)
                .map_err(|error| XduduError::tool(format!("变更镜像损坏：{error}")))?;
            let parent = path
                .parent()
                .ok_or_else(|| XduduError::tool("变更目标缺少父目录。"))?;
            fs::create_dir_all(parent).await?;
            let temporary = path.with_extension(format!("xdudu-restore-{}", Uuid::new_v4()));
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
            fs::rename(&temporary, path).await?;
        } else {
            match fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    async fn set_record_status(
        &self,
        mut record: ChangeSetRecord,
        status: ChangeSetStatus,
    ) -> XduduResult<()> {
        record.status = status;
        record.updated_at = Utc::now();
        if status == ChangeSetStatus::Undone {
            record.undone_at = Some(Utc::now());
        }
        self.atomic_write(record.id, &record).await
    }

    pub async fn recover_incomplete(&self) -> XduduResult<usize> {
        let records = self.all_record_bytes().await?;
        let mut recovered = 0;
        let mut conflicts = Vec::new();
        for data in records {
            let Ok(version) = serde_json::from_slice::<RecordVersion>(&data) else {
                continue;
            };
            if version.schema_version != 2 {
                continue;
            }
            let mut record: ChangeSetRecord = serde_json::from_slice(&data)?;
            if !matches!(
                record.status,
                ChangeSetStatus::Prepared | ChangeSetStatus::Applying
            ) {
                continue;
            }
            let mut safe = true;
            for file in &record.files {
                let target = self.safe_target(&file.path).await?;
                let current = self.current_hash(&target).await?;
                if current != file.pre_image_sha256 && current != file.post_image_sha256 {
                    safe = false;
                    break;
                }
            }
            if !safe {
                record.status = ChangeSetStatus::Conflict;
                record.updated_at = Utc::now();
                self.atomic_write(record.id, &record).await?;
                conflicts.push(record.id);
                continue;
            }
            let mut restore_error = None;
            for file in &record.files {
                let target = self.safe_target(&file.path).await?;
                if let Err(error) = self
                    .write_state(&target, file.pre_image_hex.as_deref(), file.pre_mode)
                    .await
                {
                    restore_error = Some(error);
                    break;
                }
            }
            if let Some(error) = restore_error {
                record.status = ChangeSetStatus::Conflict;
                record.updated_at = Utc::now();
                self.atomic_write(record.id, &record).await?;
                return Err(XduduError::tool(format!(
                    "恢复未完成事务 {} 失败，已标记为冲突：{}",
                    record.id, error.message
                )));
            }
            record.status = ChangeSetStatus::RolledBack;
            record.updated_at = Utc::now();
            self.atomic_write(record.id, &record).await?;
            recovered += 1;
        }
        if !conflicts.is_empty() {
            return Err(XduduError::tool(format!(
                "发现无法自动恢复的变更事务，已标记为冲突：{}。请先检查相关文件。",
                conflicts
                    .iter()
                    .map(Uuid::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(recovered)
    }

    async fn latest_candidate(
        &self,
        session_id: Option<Uuid>,
    ) -> XduduResult<Option<(Uuid, DateTime<Utc>, bool)>> {
        let mut candidates = Vec::new();
        for data in self.all_record_bytes().await? {
            let version: RecordVersion = match serde_json::from_slice(&data) {
                Ok(version) => version,
                Err(_) => continue,
            };
            if version.schema_version == 2 {
                if let Ok(record) = serde_json::from_slice::<ChangeSetRecord>(&data)
                    && record.status == ChangeSetStatus::Applied
                    && session_id.is_none_or(|id| id == record.session_id)
                {
                    candidates.push((record.id, record.created_at, true));
                }
            } else if let Ok(record) = serde_json::from_slice::<FileChangeRecord>(&data)
                && record.status == FileChangeStatus::Applied
                && session_id.is_none_or(|id| id == record.session_id)
            {
                candidates.push((record.id, record.created_at, false));
            }
        }
        candidates.sort_by_key(|(_, created_at, _)| std::cmp::Reverse(*created_at));
        Ok(candidates.into_iter().next())
    }

    async fn undo_change_set(
        &self,
        mut record: ChangeSetRecord,
        session_id: Option<Uuid>,
    ) -> XduduResult<UndoResult> {
        if session_id.is_some_and(|id| id != record.session_id) {
            return Err(XduduError::validation(
                "指定变更不属于要求的会话，拒绝撤销。",
            ));
        }
        if record.status != ChangeSetStatus::Applied {
            return Err(XduduError::validation("该变更事务当前不可撤销。"));
        }
        let mut targets = Vec::new();
        for file in &record.files {
            let target = self.safe_target(&file.path).await?;
            let current = self.current_hash(&target).await?;
            if current != file.post_image_sha256 {
                return Err(XduduError::tool(format!(
                    "文件在 Agent 写入后又发生变化，拒绝整批撤销：{}。",
                    file.path.display()
                )));
            }
            targets.push(target);
        }
        let mut restored: Vec<usize> = Vec::new();
        for (index, file) in record.files.iter().enumerate() {
            if let Err(error) = self
                .write_state(
                    &targets[index],
                    file.pre_image_hex.as_deref(),
                    file.pre_mode,
                )
                .await
            {
                for previous in restored.into_iter().rev() {
                    let previous_file: &ChangeSetFileRecord = &record.files[previous];
                    let _ = self
                        .write_state(
                            &targets[previous],
                            previous_file.post_image_hex.as_deref(),
                            previous_file.pre_mode,
                        )
                        .await;
                }
                return Err(XduduError::tool(format!(
                    "撤销事务失败，已尽力恢复撤销前状态：{error}"
                )));
            }
            restored.push(index);
        }
        record.status = ChangeSetStatus::Undone;
        record.updated_at = Utc::now();
        record.undone_at = Some(Utc::now());
        self.atomic_write(record.id, &record).await?;
        Ok(UndoResult {
            change_id: record.id,
            paths: record.files.iter().map(|file| file.path.clone()).collect(),
            removed_created_files: record
                .files
                .iter()
                .filter(|file| file.operation == FileOperation::Created)
                .count(),
        })
    }

    async fn undo_legacy(
        &self,
        mut record: FileChangeRecord,
        session_id: Option<Uuid>,
    ) -> XduduResult<UndoResult> {
        if session_id.is_some_and(|id| id != record.session_id) {
            return Err(XduduError::validation(
                "指定变更不属于要求的会话，拒绝撤销。",
            ));
        }
        if record.status != FileChangeStatus::Applied {
            return Err(XduduError::validation("该变更已经撤销。"));
        }
        let target = self.safe_target(&record.path).await?;
        if self.current_hash(&target).await?.as_deref() != Some(&record.post_image_sha256) {
            return Err(XduduError::tool(format!(
                "文件在 Agent 写入后又发生变化，拒绝撤销：{}。",
                record.path.display()
            )));
        }
        self.write_state(&target, record.pre_image_hex.as_deref(), None)
            .await?;
        record.status = FileChangeStatus::Undone;
        record.undone_at = Some(Utc::now());
        self.atomic_write(record.id, &record).await?;
        Ok(UndoResult {
            change_id: record.id,
            paths: vec![record.path],
            removed_created_files: usize::from(record.pre_image_hex.is_none()),
        })
    }

    pub async fn undo(
        &self,
        change_id: Option<Uuid>,
        session_id: Option<Uuid>,
    ) -> XduduResult<UndoResult> {
        let (id, is_v2) = if let Some(id) = change_id {
            let Some(data) = self.read_bytes(id).await? else {
                return Err(XduduError::validation(format!("找不到变更记录：{id}")));
            };
            let version: RecordVersion = serde_json::from_slice(&data)?;
            (id, version.schema_version == 2)
        } else {
            let Some((id, _, is_v2)) = self.latest_candidate(session_id).await? else {
                return Err(XduduError::validation("没有可撤销的文件变更。"));
            };
            (id, is_v2)
        };
        if is_v2 {
            let record = self
                .read_change_set(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到变更事务：{id}")))?;
            self.undo_change_set(record, session_id).await
        } else {
            let record = self
                .read_legacy(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到变更记录：{id}")))?;
            self.undo_legacy(record, session_id).await
        }
    }
}

#[async_trait]
impl ChangeLedger for JsonChangeLedger {
    async fn prepare_change_set(&self, draft: ChangeSetDraft) -> XduduResult<Option<Uuid>> {
        if draft.files.is_empty() {
            return Err(XduduError::tool("变更事务不能为空。"));
        }
        let id = Uuid::new_v4();
        let now = Utc::now();
        let record = ChangeSetRecord {
            schema_version: 2,
            id,
            session_id: draft.session_id,
            tool_call_id: draft.tool_call_id,
            files: draft
                .files
                .into_iter()
                .map(|file| ChangeSetFileRecord {
                    path: file.path,
                    operation: file.operation,
                    pre_image_hex: file.pre_image.map(hex::encode),
                    post_image_hex: file.post_image.map(hex::encode),
                    pre_image_sha256: file.pre_image_sha256,
                    post_image_sha256: file.post_image_sha256,
                    pre_mode: file.pre_mode,
                })
                .collect(),
            status: ChangeSetStatus::Prepared,
            created_at: now,
            updated_at: now,
            undone_at: None,
        };
        self.atomic_write(id, &record).await?;
        Ok(Some(id))
    }

    async fn set_change_set_status(&self, id: Uuid, status: ChangeSetStatus) -> XduduResult<()> {
        let record = self
            .read_change_set(id)
            .await?
            .ok_or_else(|| XduduError::tool(format!("找不到变更事务：{id}")))?;
        self.set_record_status(record, status).await
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn v2_事务可以整批撤销() {
        let dir = tempdir().unwrap();
        let ledger = JsonChangeLedger::with_dir(dir.path(), dir.path().join("ledger"));
        fs::write(dir.path().join("a.txt"), b"after").await.unwrap();
        fs::write(dir.path().join("b.txt"), b"created")
            .await
            .unwrap();
        let id = ledger
            .prepare_change_set(ChangeSetDraft {
                session_id: Uuid::new_v4(),
                tool_call_id: Uuid::new_v4(),
                files: vec![
                    ChangeSetFileDraft {
                        path: "a.txt".into(),
                        operation: FileOperation::Modified,
                        pre_image: Some(b"before".to_vec()),
                        post_image: Some(b"after".to_vec()),
                        pre_image_sha256: Some(sha256(b"before")),
                        post_image_sha256: Some(sha256(b"after")),
                        pre_mode: None,
                    },
                    ChangeSetFileDraft {
                        path: "b.txt".into(),
                        operation: FileOperation::Created,
                        pre_image: None,
                        post_image: Some(b"created".to_vec()),
                        pre_image_sha256: None,
                        post_image_sha256: Some(sha256(b"created")),
                        pre_mode: None,
                    },
                ],
            })
            .await
            .unwrap()
            .unwrap();
        ledger
            .set_change_set_status(id, ChangeSetStatus::Applied)
            .await
            .unwrap();
        let result = ledger.undo(Some(id), None).await.unwrap();
        assert_eq!(result.paths.len(), 2);
        assert_eq!(fs::read(dir.path().join("a.txt")).await.unwrap(), b"before");
        assert!(!dir.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn 任一文件冲突时整批不撤销() {
        let dir = tempdir().unwrap();
        let ledger = JsonChangeLedger::with_dir(dir.path(), dir.path().join("ledger"));
        fs::write(dir.path().join("a.txt"), b"after").await.unwrap();
        let id = ledger
            .prepare_change_set(ChangeSetDraft {
                session_id: Uuid::new_v4(),
                tool_call_id: Uuid::new_v4(),
                files: vec![ChangeSetFileDraft {
                    path: "a.txt".into(),
                    operation: FileOperation::Modified,
                    pre_image: Some(b"before".to_vec()),
                    post_image: Some(b"after".to_vec()),
                    pre_image_sha256: Some(sha256(b"before")),
                    post_image_sha256: Some(sha256(b"after")),
                    pre_mode: None,
                }],
            })
            .await
            .unwrap()
            .unwrap();
        ledger
            .set_change_set_status(id, ChangeSetStatus::Applied)
            .await
            .unwrap();
        fs::write(dir.path().join("a.txt"), b"user").await.unwrap();
        assert!(ledger.undo(Some(id), None).await.is_err());
        assert_eq!(fs::read(dir.path().join("a.txt")).await.unwrap(), b"user");
    }

    #[tokio::test]
    async fn 启动时回滚未完成事务() {
        let dir = tempdir().unwrap();
        let ledger = JsonChangeLedger::with_dir(dir.path(), dir.path().join("ledger"));
        fs::write(dir.path().join("a.txt"), b"after").await.unwrap();
        let id = ledger
            .prepare_change_set(ChangeSetDraft {
                session_id: Uuid::new_v4(),
                tool_call_id: Uuid::new_v4(),
                files: vec![ChangeSetFileDraft {
                    path: "a.txt".into(),
                    operation: FileOperation::Modified,
                    pre_image: Some(b"before".to_vec()),
                    post_image: Some(b"after".to_vec()),
                    pre_image_sha256: Some(sha256(b"before")),
                    post_image_sha256: Some(sha256(b"after")),
                    pre_mode: None,
                }],
            })
            .await
            .unwrap()
            .unwrap();
        ledger
            .set_change_set_status(id, ChangeSetStatus::Applying)
            .await
            .unwrap();

        assert_eq!(ledger.recover_incomplete().await.unwrap(), 1);
        assert_eq!(fs::read(dir.path().join("a.txt")).await.unwrap(), b"before");
        assert_eq!(
            ledger.read_change_set(id).await.unwrap().unwrap().status,
            ChangeSetStatus::RolledBack
        );
    }

    #[tokio::test]
    async fn 启动恢复遇到用户修改时标记冲突并阻止继续() {
        let dir = tempdir().unwrap();
        let ledger = JsonChangeLedger::with_dir(dir.path(), dir.path().join("ledger"));
        fs::write(dir.path().join("a.txt"), b"user").await.unwrap();
        let id = ledger
            .prepare_change_set(ChangeSetDraft {
                session_id: Uuid::new_v4(),
                tool_call_id: Uuid::new_v4(),
                files: vec![ChangeSetFileDraft {
                    path: "a.txt".into(),
                    operation: FileOperation::Modified,
                    pre_image: Some(b"before".to_vec()),
                    post_image: Some(b"after".to_vec()),
                    pre_image_sha256: Some(sha256(b"before")),
                    post_image_sha256: Some(sha256(b"after")),
                    pre_mode: None,
                }],
            })
            .await
            .unwrap()
            .unwrap();

        assert!(ledger.recover_incomplete().await.is_err());
        assert_eq!(fs::read(dir.path().join("a.txt")).await.unwrap(), b"user");
        assert_eq!(
            ledger.read_change_set(id).await.unwrap().unwrap().status,
            ChangeSetStatus::Conflict
        );
    }

    #[tokio::test]
    async fn v1_旧账本仍可撤销() {
        let dir = tempdir().unwrap();
        let ledger = JsonChangeLedger::with_dir(dir.path(), dir.path().join("ledger"));
        fs::create_dir_all(dir.path().join("ledger")).await.unwrap();
        fs::write(dir.path().join("legacy.txt"), b"after")
            .await
            .unwrap();
        let id = Uuid::new_v4();
        let record = FileChangeRecord {
            id,
            session_id: Uuid::new_v4(),
            tool_call_id: Uuid::new_v4(),
            path: "legacy.txt".into(),
            pre_image_hex: Some(hex::encode(b"before")),
            pre_image_sha256: Some(sha256(b"before")),
            post_image_sha256: sha256(b"after"),
            status: FileChangeStatus::Applied,
            created_at: Utc::now(),
            undone_at: None,
        };
        fs::write(
            ledger.record_path(id),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .await
        .unwrap();

        ledger.undo(Some(id), None).await.unwrap();
        assert_eq!(
            fs::read(dir.path().join("legacy.txt")).await.unwrap(),
            b"before"
        );
    }
}
