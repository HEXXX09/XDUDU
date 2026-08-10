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
    error::{XduduError, XduduResult},
    provider::{MessageRole, ToolCall},
    redaction::{redact_text, redact_value},
};

const DEFAULT_SESSIONS_DIR: &str = ".xdudu/sessions/json";
const LEGACY_SESSIONS_DIR: &str = ".xycli/sessions/json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    WaitingApproval,
    PlanReady,
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
    Interrupted,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: Uuid,
    pub role: MessageRole,
    pub content: String,
    /// 内部推理内容（仅思考路径启用时填充）。从不进入公开输出，
    /// 仅用于本会话内回传给 Provider 以维持思考闭环。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub sequence: usize,
    pub created_at: DateTime<Utc>,
}

impl Message {
    pub fn text(role: MessageRole, content: impl Into<String>, sequence: usize) -> Self {
        Self {
            id: Uuid::new_v4(),
            role,
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            sequence,
            created_at: Utc::now(),
        }
    }
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
    /// 较早消息的本地压缩摘要；原始消息仍完整保存在会话中。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context_summary: String,
    /// 已纳入 `context_summary` 的消息数量。
    #[serde(default)]
    pub summarized_message_count: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl Session {
    pub fn new(
        cwd: PathBuf,
        provider_name: impl Into<String>,
        model: impl Into<String>,
        first_user_message: impl Into<String>,
    ) -> Self {
        let message = first_user_message.into();
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            title: message.chars().take(80).collect(),
            cwd,
            status: SessionStatus::Running,
            current_state: AgentLoopState::Idle,
            plan: Value::Object(Default::default()),
            provider_name: provider_name.into(),
            model: model.into(),
            messages: vec![Message::text(MessageRole::User, message, 0)],
            tool_calls: Vec::new(),
            context_summary: String::new(),
            summarized_message_count: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            created_at: now,
            updated_at: now,
            completed_at: None,
        }
    }

    pub fn append_user_message(&mut self, content: impl Into<String>) {
        self.messages.push(Message::text(
            MessageRole::User,
            content,
            self.messages.len(),
        ));
        self.updated_at = Utc::now();
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    /// 返回可公开展示的会话副本。内部推理只用于 Provider 工具闭环，
    /// 不进入 session show、导出或其他面向用户的结构化输出。
    pub fn public_snapshot(&self) -> Self {
        let mut public = self.clone();
        for message in &mut public.messages {
            message.reasoning = None;
        }
        public
    }
}

pub(crate) fn sanitized_session(session: &Session) -> Session {
    let mut sanitized = session.clone();
    sanitized.title = redact_text(&sanitized.title);
    sanitized.context_summary = redact_text(&sanitized.context_summary);
    for message in &mut sanitized.messages {
        message.content = redact_text(&message.content);
        message.reasoning = message.reasoning.as_deref().map(redact_text);
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
    sanitized
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, session: &Session) -> XduduResult<()>;
    async fn update(&self, session: &Session) -> XduduResult<()>;
    async fn get(&self, session_id: Uuid) -> XduduResult<Option<Session>>;
    async fn list(&self, limit: usize) -> XduduResult<Vec<Session>>;
}

/// 文件级互斥可避免同一进程并发更新时互相覆盖。
pub struct JsonSessionStore {
    sessions_dir: PathBuf,
    legacy_sessions_dir: Option<PathBuf>,
    write_lock: Arc<Mutex<()>>,
}

impl JsonSessionStore {
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            sessions_dir: cwd.as_ref().join(DEFAULT_SESSIONS_DIR),
            legacy_sessions_dir: Some(cwd.as_ref().join(LEGACY_SESSIONS_DIR)),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn with_dir(sessions_dir: impl Into<PathBuf>) -> Self {
        Self {
            sessions_dir: sessions_dir.into(),
            legacy_sessions_dir: None,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    fn session_path(&self, session_id: Uuid) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.json"))
    }

    async fn atomic_write(&self, path: &Path, data: &[u8]) -> XduduResult<()> {
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
            return Err(XduduError::tool(format!("保存会话失败：{error}")));
        }
        Ok(())
    }

    async fn save(&self, session: &Session) -> XduduResult<()> {
        let sanitized = sanitized_session(session);
        let data = serde_json::to_vec_pretty(&sanitized)?;
        self.atomic_write(&self.session_path(session.id), &data)
            .await
    }
}

#[async_trait]
impl SessionStore for JsonSessionStore {
    async fn create(&self, session: &Session) -> XduduResult<()> {
        self.save(session).await
    }

    async fn update(&self, session: &Session) -> XduduResult<()> {
        self.save(session).await
    }

    async fn get(&self, session_id: Uuid) -> XduduResult<Option<Session>> {
        match fs::read(self.session_path(session_id)).await {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(legacy_dir) = &self.legacy_sessions_dir else {
                    return Ok(None);
                };
                match fs::read(legacy_dir.join(format!("{session_id}.json"))).await {
                    Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(XduduError::tool(format!("读取旧会话失败：{error}"))),
                }
            }
            Err(error) => Err(XduduError::tool(format!("读取会话失败：{error}"))),
        }
    }

    async fn list(&self, limit: usize) -> XduduResult<Vec<Session>> {
        let mut sessions = Vec::new();
        let mut directories = vec![self.sessions_dir.clone()];
        if let Some(legacy_dir) = &self.legacy_sessions_dir {
            directories.push(legacy_dir.clone());
        }
        for directory in directories {
            let mut entries = match fs::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(XduduError::tool(format!("列出会话失败：{error}"))),
            };
            while let Some(entry) = entries.next_entry().await? {
                if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(data) = fs::read(entry.path()).await
                    && let Ok(session) = serde_json::from_slice::<Session>(&data)
                    && !sessions
                        .iter()
                        .any(|existing: &Session| existing.id == session.id)
                {
                    sessions.push(session);
                }
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
            context_summary: String::new(),
            summarized_message_count: 0,
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
            reasoning: Some("内部也出现 sk-abcdefghijklmnopqrstuvwxyz".into()),
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

    #[test]
    fn 公开会话快照移除内部推理() {
        let dir = tempdir().unwrap();
        let mut session = sample_session(dir.path());
        session.messages.push(Message {
            id: Uuid::new_v4(),
            role: MessageRole::Assistant,
            content: "公开结论".into(),
            reasoning: Some("不得公开的内部推理".into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            sequence: 1,
            created_at: Utc::now(),
        });
        let public = session.public_snapshot();
        let encoded = serde_json::to_string(&public).unwrap();
        assert!(encoded.contains("公开结论"));
        assert!(!encoded.contains("不得公开的内部推理"));
        assert!(
            public
                .messages
                .iter()
                .all(|message| message.reasoning.is_none())
        );
    }

    #[tokio::test]
    async fn 新存储可以读取旧目录中的会话并写入新目录() {
        let dir = tempdir().unwrap();
        let legacy_dir = dir.path().join(LEGACY_SESSIONS_DIR);
        fs::create_dir_all(&legacy_dir).await.unwrap();
        let session = sample_session(dir.path());
        fs::write(
            legacy_dir.join(format!("{}.json", session.id)),
            serde_json::to_vec(&session).unwrap(),
        )
        .await
        .unwrap();

        let store = JsonSessionStore::new(dir.path());
        assert_eq!(store.get(session.id).await.unwrap().unwrap().id, session.id);
        store.update(&session).await.unwrap();
        assert!(store.session_path(session.id).exists());
        assert_eq!(store.list(10).await.unwrap().len(), 1);
    }
}
