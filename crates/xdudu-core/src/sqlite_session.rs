//! SQLite 会话存储、旧 JSON 迁移和工作区跨进程锁。

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use chrono::Utc;
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::{
    XduduError, XduduResult,
    provider::MessageRole,
    session::{
        AgentLoopState, Message, Session, SessionStatus, SessionStore, ToolCallStatus,
        sanitized_session,
    },
};

const DATABASE_PATH: &str = ".xdudu/xdudu.db";
const LOCK_PATH: &str = ".xdudu/workspace.lock";
const JSON_IMPORT: &str = "legacy_json_sessions_v1";
const LEGACY_DIRECTORIES: [&str; 2] = [".xdudu/sessions/json", ".xycli/sessions/json"];

/// 持有期间阻止其他 XDUDU 进程修改同一工作区。
///
/// 锁由操作系统管理；进程崩溃或退出时会自动释放。
pub struct WorkspaceLock {
    file: File,
}

impl WorkspaceLock {
    pub fn acquire(cwd: impl AsRef<Path>) -> XduduResult<Self> {
        let directory = cwd.as_ref().join(".xdudu");
        fs::create_dir_all(&directory)
            .map_err(|error| XduduError::tool(format!("创建 XDUDU 数据目录失败：{error}")))?;
        let path = cwd.as_ref().join(LOCK_PATH);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| XduduError::tool(format!("打开工作区锁失败：{error}")))?;
        set_private_permissions(&path)?;
        file.try_lock_exclusive().map_err(|error| {
            XduduError::tool(format!(
                "当前工作区已有 XDUDU 进程正在运行，请等待其结束后重试（{}）：{error}",
                path.display()
            ))
        })?;
        Ok(Self { file })
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// 以 SQLite 事务保存完整会话，同时保留可查询的核心元数据列。
pub struct SqliteSessionStore {
    database_path: PathBuf,
    _workspace_lock: WorkspaceLock,
}

impl SqliteSessionStore {
    pub fn new(cwd: impl AsRef<Path>) -> XduduResult<Self> {
        let cwd = cwd.as_ref();
        Self::with_database(cwd, cwd.join(DATABASE_PATH))
    }

    fn with_database(cwd: &Path, database_path: PathBuf) -> XduduResult<Self> {
        let workspace_lock = WorkspaceLock::acquire(cwd)?;
        if let Some(parent) = database_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| XduduError::tool(format!("创建数据库目录失败：{error}")))?;
        }
        let store = Self {
            database_path,
            _workspace_lock: workspace_lock,
        };
        store.initialize(cwd)?;
        Ok(store)
    }

    fn connection(path: &Path) -> XduduResult<Connection> {
        let connection = Connection::open(path)
            .map_err(|error| XduduError::tool(format!("打开会话数据库失败：{error}")))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| XduduError::tool(format!("配置数据库等待时间失败：{error}")))?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .map_err(|error| XduduError::tool(format!("配置会话数据库失败：{error}")))?;
        harden_database_files(path)?;
        Ok(connection)
    }

    fn initialize(&self, cwd: &Path) -> XduduResult<()> {
        let mut connection = Self::connection(&self.database_path)?;
        connection
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS schema_migrations (
                    version INTEGER PRIMARY KEY,
                    applied_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS data_migrations (
                    name TEXT PRIMARY KEY,
                    applied_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    status TEXT NOT NULL,
                    current_state TEXT NOT NULL,
                    provider_name TEXT NOT NULL,
                    model TEXT NOT NULL,
                    context_summary TEXT NOT NULL DEFAULT '',
                    summarized_message_count INTEGER NOT NULL DEFAULT 0,
                    session_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    completed_at TEXT
                );
                CREATE INDEX IF NOT EXISTS sessions_updated_at_idx
                    ON sessions(updated_at DESC);
                INSERT OR IGNORE INTO schema_migrations(version, applied_at)
                    VALUES (1, CURRENT_TIMESTAMP);
                ",
            )
            .map_err(|error| XduduError::tool(format!("初始化会话数据库失败：{error}")))?;
        self.import_json_sessions(&mut connection, cwd)?;
        self.recover_interrupted_sessions(&mut connection)?;
        Ok(())
    }

    fn import_json_sessions(&self, connection: &mut Connection, cwd: &Path) -> XduduResult<()> {
        let imported = connection
            .query_row(
                "SELECT 1 FROM data_migrations WHERE name = ?1",
                [JSON_IMPORT],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| XduduError::tool(format!("检查旧会话迁移状态失败：{error}")))?
            .is_some();
        if imported {
            return Ok(());
        }

        let mut sessions = Vec::new();
        for relative in LEGACY_DIRECTORIES {
            let directory = cwd.join(relative);
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(XduduError::tool(format!(
                        "读取旧会话目录 {} 失败：{error}",
                        directory.display()
                    )));
                }
            };
            for entry in entries {
                let entry = entry
                    .map_err(|error| XduduError::tool(format!("读取旧会话目录项失败：{error}")))?;
                if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let data = fs::read(entry.path())
                    .map_err(|error| XduduError::tool(format!("读取旧会话失败：{error}")))?;
                let session: Session = serde_json::from_slice(&data).map_err(|error| {
                    XduduError::validation(format!(
                        "旧会话 {} 无法迁移：{error}",
                        entry.path().display()
                    ))
                })?;
                if !sessions
                    .iter()
                    .any(|existing: &Session| existing.id == session.id)
                {
                    sessions.push(session);
                }
            }
        }

        let transaction = connection
            .transaction()
            .map_err(|error| XduduError::tool(format!("开始旧会话迁移失败：{error}")))?;
        for session in sessions {
            insert_session(&transaction, &session, true)?;
        }
        transaction
            .execute(
                "INSERT INTO data_migrations(name, applied_at) VALUES (?1, ?2)",
                params![JSON_IMPORT, Utc::now().to_rfc3339()],
            )
            .map_err(|error| XduduError::tool(format!("记录旧会话迁移状态失败：{error}")))?;
        transaction
            .commit()
            .map_err(|error| XduduError::tool(format!("提交旧会话迁移失败：{error}")))
    }

    fn recover_interrupted_sessions(&self, connection: &mut Connection) -> XduduResult<()> {
        let mut statement = connection
            .prepare(
                "SELECT session_json FROM sessions
                 WHERE status IN ('running', 'waiting_approval')",
            )
            .map_err(|error| XduduError::tool(format!("查询异常会话失败：{error}")))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| XduduError::tool(format!("读取异常会话失败：{error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| XduduError::tool(format!("读取异常会话失败：{error}")))?;
        drop(statement);

        let transaction = connection
            .transaction()
            .map_err(|error| XduduError::tool(format!("开始崩溃恢复失败：{error}")))?;
        for raw in rows {
            let mut session: Session = serde_json::from_str(&raw)?;
            let recovered_at = Utc::now();
            let interrupted_calls = session
                .tool_calls
                .iter_mut()
                .filter(|record| {
                    matches!(
                        record.status,
                        ToolCallStatus::Pending | ToolCallStatus::Running
                    )
                })
                .map(|record| {
                    record.status = ToolCallStatus::Cancelled;
                    record.error = Some(
                        "上次进程在工具完成前退出，副作用结果未知，XDUDU 不会自动重试。".into(),
                    );
                    record.ended_at = Some(recovered_at);
                    record.id.clone()
                })
                .collect::<Vec<_>>();
            for call_id in interrupted_calls {
                if !session
                    .messages
                    .iter()
                    .any(|message| message.tool_call_id.as_deref() == Some(&call_id))
                {
                    session.messages.push(Message {
                        id: Uuid::new_v4(),
                        role: MessageRole::Tool,
                        content: "Error: 上次进程在工具完成前退出，执行结果未知，不会自动重试。"
                            .into(),
                        tool_calls: Vec::new(),
                        tool_call_id: Some(call_id),
                        sequence: session.messages.len(),
                        created_at: recovered_at,
                    });
                }
            }
            session.status = SessionStatus::Interrupted;
            session.current_state = AgentLoopState::Error;
            session.updated_at = recovered_at;
            session.completed_at = Some(session.updated_at);
            update_session(&transaction, &session)?;
        }
        transaction
            .commit()
            .map_err(|error| XduduError::tool(format!("提交崩溃恢复失败：{error}")))
    }

    async fn run_blocking<T, F>(&self, operation: F) -> XduduResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Connection) -> XduduResult<T> + Send + 'static,
    {
        let path = self.database_path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = Self::connection(&path)?;
            operation(connection)
        })
        .await
        .map_err(|error| XduduError::tool(format!("数据库任务异常结束：{error}")))?
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> XduduResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| XduduError::tool(format!("收紧本地数据文件权限失败：{error}")))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> XduduResult<()> {
    Ok(())
}

fn harden_database_files(path: &Path) -> XduduResult<()> {
    let mut candidates = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        candidates.push(PathBuf::from(name));
    }
    for candidate in candidates {
        match set_private_permissions(&candidate) {
            Ok(()) => {}
            Err(_) if !candidate.exists() => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn enum_name<T: serde::Serialize>(value: T) -> XduduResult<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| XduduError::tool("无法序列化会话状态。"))
}

fn session_values(session: &Session) -> XduduResult<(Session, String)> {
    let sanitized = sanitized_session(session);
    let raw = serde_json::to_string(&sanitized)?;
    Ok((sanitized, raw))
}

fn insert_session(
    connection: &Connection,
    session: &Session,
    ignore_existing: bool,
) -> XduduResult<()> {
    let (session, raw) = session_values(session)?;
    let sql = if ignore_existing {
        "INSERT OR IGNORE INTO sessions (
            id, title, cwd, status, current_state, provider_name, model,
            context_summary, summarized_message_count, session_json,
            created_at, updated_at, completed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
    } else {
        "INSERT INTO sessions (
            id, title, cwd, status, current_state, provider_name, model,
            context_summary, summarized_message_count, session_json,
            created_at, updated_at, completed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
    };
    connection
        .execute(
            sql,
            params![
                session.id.to_string(),
                session.title,
                session.cwd.to_string_lossy(),
                enum_name(session.status)?,
                enum_name(session.current_state)?,
                session.provider_name,
                session.model,
                session.context_summary,
                session.summarized_message_count as i64,
                raw,
                session.created_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
                session.completed_at.map(|value| value.to_rfc3339()),
            ],
        )
        .map_err(|error| XduduError::tool(format!("创建会话失败：{error}")))?;
    Ok(())
}

fn update_session(connection: &Connection, session: &Session) -> XduduResult<()> {
    let (session, raw) = session_values(session)?;
    let changed = connection
        .execute(
            "UPDATE sessions SET
                title = ?2, cwd = ?3, status = ?4, current_state = ?5,
                provider_name = ?6, model = ?7, context_summary = ?8,
                summarized_message_count = ?9, session_json = ?10,
                created_at = ?11, updated_at = ?12, completed_at = ?13
             WHERE id = ?1",
            params![
                session.id.to_string(),
                session.title,
                session.cwd.to_string_lossy(),
                enum_name(session.status)?,
                enum_name(session.current_state)?,
                session.provider_name,
                session.model,
                session.context_summary,
                session.summarized_message_count as i64,
                raw,
                session.created_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
                session.completed_at.map(|value| value.to_rfc3339()),
            ],
        )
        .map_err(|error| XduduError::tool(format!("更新会话失败：{error}")))?;
    if changed == 0 {
        return Err(XduduError::validation(format!(
            "找不到要更新的会话：{}",
            session.id
        )));
    }
    Ok(())
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn create(&self, session: &Session) -> XduduResult<()> {
        let session = session.clone();
        self.run_blocking(move |mut connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| XduduError::tool(format!("开始创建会话事务失败：{error}")))?;
            insert_session(&transaction, &session, false)?;
            transaction
                .commit()
                .map_err(|error| XduduError::tool(format!("提交创建会话事务失败：{error}")))
        })
        .await
    }

    async fn update(&self, session: &Session) -> XduduResult<()> {
        let session = session.clone();
        self.run_blocking(move |mut connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| XduduError::tool(format!("开始更新会话事务失败：{error}")))?;
            update_session(&transaction, &session)?;
            transaction
                .commit()
                .map_err(|error| XduduError::tool(format!("提交更新会话事务失败：{error}")))
        })
        .await
    }

    async fn get(&self, session_id: Uuid) -> XduduResult<Option<Session>> {
        self.run_blocking(move |connection| {
            let raw = connection
                .query_row(
                    "SELECT session_json FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| XduduError::tool(format!("读取会话失败：{error}")))?;
            raw.map(|value| serde_json::from_str(&value).map_err(XduduError::from))
                .transpose()
        })
        .await
    }

    async fn list(&self, limit: usize) -> XduduResult<Vec<Session>> {
        self.run_blocking(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT session_json FROM sessions
                     ORDER BY updated_at DESC LIMIT ?1",
                )
                .map_err(|error| XduduError::tool(format!("准备会话列表查询失败：{error}")))?;
            let rows = statement
                .query_map([limit as i64], |row| row.get::<_, String>(0))
                .map_err(|error| XduduError::tool(format!("列出会话失败：{error}")))?;
            rows.map(|row| {
                let raw =
                    row.map_err(|error| XduduError::tool(format!("读取会话失败：{error}")))?;
                serde_json::from_str(&raw).map_err(XduduError::from)
            })
            .collect()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Message;
    use tempfile::tempdir;

    fn sample_session(cwd: &Path) -> Session {
        let now = Utc::now();
        Session {
            id: Uuid::new_v4(),
            title: "SQLite 测试".into(),
            cwd: cwd.to_path_buf(),
            status: SessionStatus::Running,
            current_state: AgentLoopState::Planning,
            plan: serde_json::json!({"goal":"完成测试"}),
            provider_name: "deepseek".into(),
            model: "deepseek-chat".into(),
            messages: Vec::<Message>::new(),
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
    async fn sqlite_创建更新查询和列表() {
        let dir = tempdir().unwrap();
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let mut session = sample_session(dir.path());
        store.create(&session).await.unwrap();
        session.status = SessionStatus::Completed;
        session.updated_at = Utc::now();
        store.update(&session).await.unwrap();
        assert_eq!(
            store.get(session.id).await.unwrap().unwrap().status,
            SessionStatus::Completed
        );
        assert_eq!(store.list(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn 旧_json_会话事务导入且原文件保留() {
        let dir = tempdir().unwrap();
        let legacy = dir.path().join(".xdudu/sessions/json");
        fs::create_dir_all(&legacy).unwrap();
        let session = sample_session(dir.path());
        let path = legacy.join(format!("{}.json", session.id));
        fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();

        let store = SqliteSessionStore::new(dir.path()).unwrap();
        assert_eq!(store.get(session.id).await.unwrap().unwrap().id, session.id);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn 损坏旧会话不会留下部分迁移结果() {
        let dir = tempdir().unwrap();
        let legacy = dir.path().join(".xdudu/sessions/json");
        fs::create_dir_all(&legacy).unwrap();
        let session = sample_session(dir.path());
        fs::write(
            legacy.join(format!("{}.json", session.id)),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
        let broken = legacy.join("broken.json");
        fs::write(&broken, b"not json").unwrap();

        assert!(SqliteSessionStore::new(dir.path()).is_err());
        fs::remove_file(broken).unwrap();
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        assert_eq!(store.list(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sqlite_落盘前统一脱敏() {
        let dir = tempdir().unwrap();
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let mut session = sample_session(dir.path());
        session.messages.push(Message {
            id: Uuid::new_v4(),
            role: MessageRole::User,
            content: "secret sk-abcdefghijklmnopqrstuvwxyz".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            sequence: 0,
            created_at: Utc::now(),
        });
        store.create(&session).await.unwrap();
        let loaded = store.get(session.id).await.unwrap().unwrap();
        assert!(
            !loaded.messages[0]
                .content
                .contains("sk-abcdefghijklmnopqrstuvwxyz")
        );
        assert!(loaded.messages[0].content.contains("[已脱敏]"));
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_和锁文件仅允许当前用户访问() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let _store = SqliteSessionStore::new(dir.path()).unwrap();
        for path in [dir.path().join(DATABASE_PATH), dir.path().join(LOCK_PATH)] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn 工作区锁拒绝第二个进程实例() {
        let dir = tempdir().unwrap();
        let first = WorkspaceLock::acquire(dir.path()).unwrap();
        let error = WorkspaceLock::acquire(dir.path()).err().unwrap();
        assert!(error.message.contains("已有 XDUDU 进程"));
        drop(first);
        WorkspaceLock::acquire(dir.path()).unwrap();
    }

    #[tokio::test]
    async fn 重开数据库会把运行中会话标记为中断() {
        let dir = tempdir().unwrap();
        let mut session = sample_session(dir.path());
        session.tool_calls.push(crate::session::ToolCallRecord {
            id: "pending-call".into(),
            tool_name: "file_write".into(),
            input: serde_json::json!({"path":"a.txt"}),
            output: None,
            error: None,
            status: ToolCallStatus::Pending,
            duration_ms: None,
            started_at: Utc::now(),
            ended_at: None,
            approval: None,
        });
        {
            let store = SqliteSessionStore::new(dir.path()).unwrap();
            store.create(&session).await.unwrap();
        }
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let recovered = store.get(session.id).await.unwrap().unwrap();
        assert_eq!(recovered.status, SessionStatus::Interrupted);
        assert_eq!(recovered.current_state, AgentLoopState::Error);
        assert_eq!(recovered.tool_calls[0].status, ToolCallStatus::Cancelled);
        assert!(
            recovered
                .messages
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some("pending-call"))
        );
    }
}
