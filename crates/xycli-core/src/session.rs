//! 本地 JSON 会话存储，采用同目录临时文件加原子重命名。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{fs, sync::Mutex};
use uuid::Uuid;

use crate::{
    approval::ApprovalRecord,
    error::{XycliError, XycliResult},
    provider::{MessageRole, ToolCall},
    redaction::{redact_text, redact_value},
};

const DEFAULT_SESSIONS_DIR: &str = ".xycli/sessions/json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    WaitingApproval,
    Completed,
    Incomplete,
    Error,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentLoopState {
    Idle,
    Planning,
    Acting,
    Observing,
    Reflecting,
    WaitingApproval,
    Incomplete,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: Uuid,
    pub role: MessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub sequence: usize,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallRecord {
    pub id: String,
    pub tool_name: String,
    pub input: Value,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub status: ToolCallStatus,
    pub duration_ms: Option<u64>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: Uuid,
    pub title: String,
    pub cwd: PathBuf,
    pub status: SessionStatus,
    pub current_state: AgentLoopState,
    #[serde(default)]
    pub plan: Value,
    pub provider_name: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, session: &Session) -> XycliResult<()>;
    async fn update(&self, session: &Session) -> XycliResult<()>;
    async fn get(&self, session_id: Uuid) -> XycliResult<Option<Session>>;
    async fn list(&self, limit: usize) -> XycliResult<Vec<Session>>;
}

/// 文件级互斥可避免同一进程并发更新时互相覆盖。
pub struct JsonSessionStore {
    sessions_dir: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl JsonSessionStore {
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self::with_dir(cwd.as_ref().join(DEFAULT_SESSIONS_DIR))
    }

    pub fn with_dir(sessions_dir: impl Into<PathBuf>) -> Self {
        Self {
            sessions_dir: sessions_dir.into(),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    fn session_path(&self, session_id: Uuid) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.json"))
    }

    async fn atomic_write(&self, path: &Path, data: &[u8]) -> XycliResult<()> {
        let _guard = self.write_lock.lock().await;
        fs::create_dir_all(&self.sessions_dir).await?;
        let tmp = path.with_extension(format!("json.{}.tmp", Uuid::new_v4()));
        if let Err(error) = async {
            fs::write(&tmp, data).await?;
            fs::rename(&tmp, path).await?;
            Ok::<_, std::io::Error>(())
        }
        .await
        {
            let _ = fs::remove_file(&tmp).await;
            return Err(XycliError::tool(format!("保存会话失败：{error}")));
        }
        Ok(())
    }

    async fn save(&self, session: &Session) -> XycliResult<()> {
        let mut sanitized = session.clone();
        sanitized.title = redact_text(&sanitized.title);
        for message in &mut sanitized.messages {
            message.content = redact_text(&message.content);
            for call in &mut message.tool_calls {
                call.input = redact_value(&call.input);
            }
        }
        for call in &mut sanitized.tool_calls {
            call.input = redact_value(&call.input);
            call.output = call.output.as_ref().map(redact_value);
            call.error = call.error.as_deref().map(redact_text);
            if let Some(approval) = &mut call.approval {
                approval.reason = redact_text(&approval.reason);
            }
        }
        let data = serde_json::to_vec_pretty(&sanitized)?;
        self.atomic_write(&self.session_path(session.id), &data)
            .await
    }
}

#[async_trait]
impl SessionStore for JsonSessionStore {
    async fn create(&self, session: &Session) -> XycliResult<()> {
        self.save(session).await
    }

    async fn update(&self, session: &Session) -> XycliResult<()> {
        self.save(session).await
    }

    async fn get(&self, session_id: Uuid) -> XycliResult<Option<Session>> {
        match fs::read(self.session_path(session_id)).await {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(XycliError::tool(format!("读取会话失败：{error}"))),
        }
    }

    async fn list(&self, limit: usize) -> XycliResult<Vec<Session>> {
        let mut entries = match fs::read_dir(&self.sessions_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(XycliError::tool(format!("列出会话失败：{error}"))),
        };
        let mut sessions = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Ok(data) = fs::read(entry.path()).await
                && let Ok(session) = serde_json::from_slice::<Session>(&data)
            {
                sessions.push(session);
            }
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        sessions.truncate(limit);
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_session(cwd: &Path) -> Session {
        let now = Utc::now();
        Session {
            id: Uuid::new_v4(),
            title: "测试会话".into(),
            cwd: cwd.to_path_buf(),
            status: SessionStatus::Running,
            current_state: AgentLoopState::Planning,
            plan: Value::Object(Default::default()),
            provider_name: "mock".into(),
            model: "test".into(),
            messages: Vec::new(),
            tool_calls: Vec::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            created_at: now,
            updated_at: now,
            completed_at: None,
        }
    }

    #[tokio::test]
    async fn 会话可以创建更新和读取() {
        let dir = tempdir().unwrap();
        let store = JsonSessionStore::new(dir.path());
        let mut session = sample_session(dir.path());
        store.create(&session).await.unwrap();
        session.status = SessionStatus::Completed;
        session.updated_at = Utc::now();
        store.update(&session).await.unwrap();
        let loaded = store.get(session.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, SessionStatus::Completed);
    }

    #[tokio::test]
    async fn 列表忽略损坏文件并按时间排序() {
        let dir = tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let store = JsonSessionStore::with_dir(&session_dir);
        let mut older = sample_session(dir.path());
        older.updated_at = Utc::now() - chrono::Duration::minutes(1);
        let newer = sample_session(dir.path());
        store.create(&older).await.unwrap();
        store.create(&newer).await.unwrap();
        fs::write(session_dir.join("broken.json"), b"not json")
            .await
            .unwrap();
        let sessions = store.list(10).await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, newer.id);
    }

    #[tokio::test]
    async fn 会话落盘前脱敏密钥() {
        let dir = tempdir().unwrap();
        let store = JsonSessionStore::with_dir(dir.path());
        let mut session = sample_session(dir.path());
        session.messages.push(Message {
            id: Uuid::new_v4(),
            role: MessageRole::User,
            content: "请使用 sk-abcdefghijklmnopqrstuvwxyz".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            sequence: 0,
            created_at: Utc::now(),
        });
        store.create(&session).await.unwrap();
        let raw = fs::read_to_string(store.session_path(session.id))
            .await
            .unwrap();
        assert!(!raw.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(raw.contains("[已脱敏]"));
    }
}
