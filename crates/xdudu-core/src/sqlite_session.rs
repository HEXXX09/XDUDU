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
    memories::{MAX_MEMORY_BYTES, MemoryRecord, MemoryStore, sanitize_memory_content},
    plan::{
        Plan, PlanRevision, PlanStatus, PlanStore, deserialize_plan_compatible, sanitized_plan,
        sanitized_plan_revision,
    },
    provider::MessageRole,
    session::{
        AgentLoopState, Message, Session, SessionStatus, SessionStore, ToolCallStatus,
        sanitized_session,
    },
};
use chrono::DateTime;

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
                CREATE TABLE IF NOT EXISTS plans (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    schema_version INTEGER NOT NULL,
                    revision INTEGER NOT NULL DEFAULT 1,
                    execution_version INTEGER NOT NULL DEFAULT 0,
                    plan_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    completed_at TEXT,
                    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS plans_session_updated_idx
                    ON plans(session_id, updated_at DESC);
                INSERT OR IGNORE INTO schema_migrations(version, applied_at)
                    VALUES (1, CURRENT_TIMESTAMP);
                INSERT OR IGNORE INTO schema_migrations(version, applied_at)
                    VALUES (2, CURRENT_TIMESTAMP);
                ",
            )
            .map_err(|error| XduduError::tool(format!("初始化会话数据库失败：{error}")))?;
        migrate_plan_schema_v3(&mut connection)?;
        migrate_plan_schema_v4(&mut connection)?;
        migrate_memory_schema_v5(&mut connection)?;
        migrate_reasoning_schema_v6(&mut connection)?;
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
            if session.status == SessionStatus::WaitingApproval {
                let waiting_for_plan = transaction
                    .query_row(
                        "SELECT 1 FROM plans
                         WHERE session_id = ?1 AND status = 'pending_approval'
                         LIMIT 1",
                        [session.id.to_string()],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(|error| {
                        XduduError::tool(format!("检查待审批计划恢复状态失败：{error}"))
                    })?
                    .is_some();
                if waiting_for_plan {
                    continue;
                }
            }
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
                        reasoning: None,
                        tool_calls: Vec::new(),
                        tool_call_id: Some(call_id),
                        sequence: session.messages.len(),
                        created_at: recovered_at,
                    });
                }
            }
            let running_plan = transaction
                .query_row(
                    "SELECT plan_json FROM plans
                     WHERE session_id = ?1 AND status = 'running'
                     ORDER BY updated_at DESC LIMIT 1",
                    [session.id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| XduduError::tool(format!("读取运行中计划失败：{error}")))?;
            if let Some(raw_plan) = running_plan {
                let value = serde_json::from_str(&raw_plan)?;
                let mut plan = deserialize_plan_compatible(value)?;
                if let Some(step_id) = plan.current_step_id
                    && let Some(step) = plan.steps.iter_mut().find(|step| step.id == step_id)
                    && step.status == crate::StepStatus::Running
                {
                    if let Some(attempt) = step.attempts.last_mut()
                        && attempt.status == crate::StepAttemptStatus::Running
                    {
                        attempt.status = crate::StepAttemptStatus::Interrupted;
                        attempt.error =
                            Some("上次进程在步骤完成前退出，工具副作用结果可能未知。".into());
                        attempt.ended_at = Some(recovered_at);
                    }
                    step.error =
                        Some("上次进程在步骤完成前退出，XDUDU 不会自动重放该步骤。".into());
                    step.transition_to(crate::StepStatus::Blocked)?;
                }
                plan.transition_to(PlanStatus::Paused)?;
                plan.paused_reason =
                    Some("检测到上次执行异常中断；现场已保留，结果未知的工具不会自动重放。".into());
                plan.execution_version = plan.execution_version.saturating_add(1);
                plan.updated_at = recovered_at;
                update_plan(&transaction, &plan)?;
            }
            session.status = SessionStatus::Interrupted;
            session.current_state = AgentLoopState::Interrupted;
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

fn migrate_plan_schema_v4(connection: &mut Connection) -> XduduResult<()> {
    let applied = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = 4",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| XduduError::tool(format!("检查执行迁移状态失败：{error}")))?
        .is_some();
    if applied {
        return Ok(());
    }
    let has_column = {
        let mut statement = connection
            .prepare("PRAGMA table_info(plans)")
            .map_err(|error| XduduError::tool(format!("检查 plans 表结构失败：{error}")))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| XduduError::tool(format!("读取 plans 表结构失败：{error}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| XduduError::tool(format!("读取 plans 列名失败：{error}")))?
            .iter()
            .any(|name| name == "execution_version")
    };
    let transaction = connection
        .transaction()
        .map_err(|error| XduduError::tool(format!("开始计划 Schema v4 迁移失败：{error}")))?;
    if !has_column {
        transaction
            .execute(
                "ALTER TABLE plans ADD COLUMN execution_version INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| XduduError::tool(format!("增加执行版本列失败：{error}")))?;
    }
    let plans = {
        let mut statement = transaction
            .prepare("SELECT id, plan_json FROM plans")
            .map_err(|error| XduduError::tool(format!("准备执行迁移查询失败：{error}")))?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| XduduError::tool(format!("查询执行迁移计划失败：{error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| XduduError::tool(format!("读取执行迁移计划失败：{error}")))?
    };
    for (id, raw) in plans {
        let value = serde_json::from_str(&raw)?;
        let plan = deserialize_plan_compatible(value)?;
        let raw = serde_json::to_string(&sanitized_plan(&plan))?;
        transaction
            .execute(
                "UPDATE plans SET schema_version = ?2, execution_version = ?3,
                 plan_json = ?4 WHERE id = ?1",
                params![
                    id,
                    i64::from(plan.schema_version),
                    plan.execution_version as i64,
                    raw
                ],
            )
            .map_err(|error| XduduError::tool(format!("迁移当前计划失败：{error}")))?;
    }
    let revisions = {
        let mut statement = transaction
            .prepare("SELECT plan_id, revision, revision_json FROM plan_revisions")
            .map_err(|error| XduduError::tool(format!("准备修订迁移查询失败：{error}")))?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| XduduError::tool(format!("查询修订迁移失败：{error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| XduduError::tool(format!("读取修订迁移失败：{error}")))?
    };
    for (plan_id, revision, raw) in revisions {
        let mut value: serde_json::Value = serde_json::from_str(&raw)?;
        value["schemaVersion"] = serde_json::Value::from(crate::PLAN_SCHEMA_VERSION);
        let snapshot: PlanRevision = serde_json::from_value(value)?;
        snapshot.validate()?;
        transaction
            .execute(
                "UPDATE plan_revisions SET revision_json = ?3
                 WHERE plan_id = ?1 AND revision = ?2",
                params![plan_id, revision, serde_json::to_string(&snapshot)?],
            )
            .map_err(|error| XduduError::tool(format!("迁移计划修订失败：{error}")))?;
    }
    transaction
        .execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (4, CURRENT_TIMESTAMP)",
            [],
        )
        .map_err(|error| XduduError::tool(format!("记录计划 Schema v4 失败：{error}")))?;
    transaction
        .commit()
        .map_err(|error| XduduError::tool(format!("提交计划 Schema v4 迁移失败：{error}")))
}

/// Schema v5：记忆表与 FTS5 全文索引。
fn migrate_memory_schema_v5(connection: &mut Connection) -> XduduResult<()> {
    let applied = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = 5",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| XduduError::tool(format!("检查记忆迁移状态失败：{error}")))?
        .is_some();
    if applied {
        return Ok(());
    }
    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                source_session_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS memories_created_idx
                ON memories(created_at DESC);
            DROP TABLE IF EXISTS memories_fts;
            CREATE VIRTUAL TABLE memories_fts
                USING fts5(content, tokenize='trigram');
            INSERT INTO schema_migrations(version, applied_at)
                VALUES (5, CURRENT_TIMESTAMP);
            ",
        )
        .map_err(|error| XduduError::tool(format!("初始化记忆表失败：{error}")))?;
    Ok(())
}

/// Schema v6：内部推理（reasoning）字段。
///
/// 无需新增表或列：`sessions.session_json` 内嵌 `Message` 序列化增加
/// `reasoning` 字段（`#[serde(default, skip_serializing_if)]`），旧 JSON 与
/// 旧会话可无缝读取。迁移仅记录版本号。
fn migrate_reasoning_schema_v6(connection: &mut Connection) -> XduduResult<()> {
    let applied = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = 6",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| XduduError::tool(format!("检查推理迁移状态失败：{error}")))?
        .is_some();
    if applied {
        return Ok(());
    }
    connection
        .execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (6, CURRENT_TIMESTAMP)",
            [],
        )
        .map_err(|error| XduduError::tool(format!("记录推理 Schema v6 迁移失败：{error}")))?;
    Ok(())
}

fn migrate_plan_schema_v3(connection: &mut Connection) -> XduduResult<()> {
    let already_applied = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = 3",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| XduduError::tool(format!("检查计划迁移状态失败：{error}")))?
        .is_some();
    if already_applied {
        return Ok(());
    }

    let has_revision = {
        let mut statement = connection
            .prepare("PRAGMA table_info(plans)")
            .map_err(|error| XduduError::tool(format!("检查 plans 表结构失败：{error}")))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| XduduError::tool(format!("读取 plans 表结构失败：{error}")))?;
        let mut found = false;
        for column in columns {
            if column.map_err(|error| XduduError::tool(format!("读取 plans 列名失败：{error}")))?
                == "revision"
            {
                found = true;
                break;
            }
        }
        found
    };

    let transaction = connection
        .transaction()
        .map_err(|error| XduduError::tool(format!("开始计划 Schema v3 迁移失败：{error}")))?;
    if !has_revision {
        transaction
            .execute(
                "ALTER TABLE plans ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
                [],
            )
            .map_err(|error| XduduError::tool(format!("增加计划 revision 列失败：{error}")))?;
    }
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS plan_revisions (
                plan_id TEXT NOT NULL,
                revision INTEGER NOT NULL,
                revision_json TEXT NOT NULL,
                change_request TEXT,
                created_at TEXT NOT NULL,
                PRIMARY KEY(plan_id, revision),
                FOREIGN KEY(plan_id) REFERENCES plans(id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS plan_revisions_created_idx
                ON plan_revisions(plan_id, revision ASC);",
        )
        .map_err(|error| XduduError::tool(format!("创建计划修订表失败：{error}")))?;

    let stored_plans = {
        let mut statement = transaction
            .prepare("SELECT id, plan_json FROM plans ORDER BY created_at ASC")
            .map_err(|error| XduduError::tool(format!("准备计划迁移查询失败：{error}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| XduduError::tool(format!("查询待迁移计划失败：{error}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| XduduError::tool(format!("读取待迁移计划失败：{error}")))?
    };

    for (stored_id, raw) in stored_plans {
        let value = serde_json::from_str(&raw)
            .map_err(|error| XduduError::tool(format!("计划 {stored_id} JSON 损坏：{error}")))?;
        let plan = deserialize_plan_compatible(value).map_err(|error| {
            XduduError::tool(format!("计划 {stored_id} 无法迁移：{}", error.message))
        })?;
        if plan.id.to_string() != stored_id {
            return Err(XduduError::tool(format!(
                "计划 {stored_id} 的 JSON ID 不一致，拒绝迁移。"
            )));
        }
        let sanitized = sanitized_plan(&plan);
        let plan_raw = serde_json::to_string(&sanitized)?;
        transaction
            .execute(
                "UPDATE plans SET status = ?2, schema_version = ?3,
                    revision = ?4, plan_json = ?5 WHERE id = ?1",
                params![
                    stored_id,
                    enum_name(sanitized.status)?,
                    i64::from(sanitized.schema_version),
                    i64::from(sanitized.revision),
                    plan_raw,
                ],
            )
            .map_err(|error| XduduError::tool(format!("更新迁移计划失败：{error}")))?;
        let revision = PlanRevision::from_plan(&sanitized, None)?;
        insert_plan_revision(&transaction, &revision)?;
    }
    transaction
        .execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (3, CURRENT_TIMESTAMP)",
            [],
        )
        .map_err(|error| XduduError::tool(format!("记录计划 Schema v3 失败：{error}")))?;
    transaction
        .commit()
        .map_err(|error| XduduError::tool(format!("提交计划 Schema v3 迁移失败：{error}")))
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

fn plan_values(plan: &Plan) -> XduduResult<(Plan, String)> {
    plan.validate()?;
    let sanitized = sanitized_plan(plan);
    let raw = serde_json::to_string(&sanitized)?;
    Ok((sanitized, raw))
}

fn revision_values(revision: &PlanRevision) -> XduduResult<(PlanRevision, String)> {
    revision.validate()?;
    let sanitized = sanitized_plan_revision(revision);
    let raw = serde_json::to_string(&sanitized)?;
    Ok((sanitized, raw))
}

fn insert_plan_revision(connection: &Connection, revision: &PlanRevision) -> XduduResult<()> {
    let (revision, raw) = revision_values(revision)?;
    connection
        .execute(
            "INSERT INTO plan_revisions (
                plan_id, revision, revision_json, change_request, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                revision.plan_id.to_string(),
                i64::from(revision.revision),
                raw,
                revision.change_request,
                revision.created_at.to_rfc3339(),
            ],
        )
        .map_err(|error| XduduError::tool(format!("保存计划修订快照失败：{error}")))?;
    Ok(())
}

fn insert_plan(connection: &Connection, plan: &Plan) -> XduduResult<()> {
    let (plan, raw) = plan_values(plan)?;
    connection
        .execute(
            "INSERT INTO plans (
                id, session_id, status, schema_version, revision, execution_version, plan_json,
                created_at, updated_at, completed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                plan.id.to_string(),
                plan.session_id.to_string(),
                enum_name(plan.status)?,
                i64::from(plan.schema_version),
                i64::from(plan.revision),
                plan.execution_version as i64,
                raw,
                plan.created_at.to_rfc3339(),
                plan.updated_at.to_rfc3339(),
                plan.completed_at.map(|value| value.to_rfc3339()),
            ],
        )
        .map_err(|error| XduduError::tool(format!("创建计划失败：{error}")))?;
    Ok(())
}

fn update_plan(connection: &Connection, plan: &Plan) -> XduduResult<()> {
    let (plan, raw) = plan_values(plan)?;
    let changed = connection
        .execute(
            "UPDATE plans SET
                status = ?3, schema_version = ?4, revision = ?5,
                execution_version = ?6, plan_json = ?7,
                created_at = ?8, updated_at = ?9, completed_at = ?10
             WHERE id = ?1 AND session_id = ?2",
            params![
                plan.id.to_string(),
                plan.session_id.to_string(),
                enum_name(plan.status)?,
                i64::from(plan.schema_version),
                i64::from(plan.revision),
                plan.execution_version as i64,
                raw,
                plan.created_at.to_rfc3339(),
                plan.updated_at.to_rfc3339(),
                plan.completed_at.map(|value| value.to_rfc3339()),
            ],
        )
        .map_err(|error| XduduError::tool(format!("更新计划失败：{error}")))?;
    if changed == 0 {
        return Err(XduduError::validation(format!(
            "找不到要更新的计划：{}",
            plan.id
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

#[async_trait]
impl PlanStore for SqliteSessionStore {
    async fn create_plan(&self, plan: &Plan) -> XduduResult<()> {
        let plan = plan.clone();
        self.run_blocking(move |mut connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| XduduError::tool(format!("开始创建计划事务失败：{error}")))?;
            insert_plan(&transaction, &plan)?;
            let revision = PlanRevision::from_plan(&plan, None)?;
            insert_plan_revision(&transaction, &revision)?;
            transaction
                .commit()
                .map_err(|error| XduduError::tool(format!("提交创建计划事务失败：{error}")))
        })
        .await
    }

    async fn update_plan(&self, plan: &Plan) -> XduduResult<()> {
        let plan = plan.clone();
        self.run_blocking(move |mut connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| XduduError::tool(format!("开始更新计划事务失败：{error}")))?;
            update_plan(&transaction, &plan)?;
            transaction
                .commit()
                .map_err(|error| XduduError::tool(format!("提交更新计划事务失败：{error}")))
        })
        .await
    }

    async fn get_plan(&self, plan_id: Uuid) -> XduduResult<Option<Plan>> {
        self.run_blocking(move |connection| {
            let raw = connection
                .query_row(
                    "SELECT plan_json FROM plans WHERE id = ?1",
                    [plan_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| XduduError::tool(format!("读取计划失败：{error}")))?;
            raw.map(|raw| {
                let value = serde_json::from_str(&raw)?;
                deserialize_plan_compatible(value)
            })
            .transpose()
        })
        .await
    }

    async fn latest_plan_for_session(&self, session_id: Uuid) -> XduduResult<Option<Plan>> {
        self.run_blocking(move |connection| {
            let raw = connection
                .query_row(
                    "SELECT plan_json FROM plans
                     WHERE session_id = ?1
                     ORDER BY updated_at DESC LIMIT 1",
                    [session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| XduduError::tool(format!("读取会话计划失败：{error}")))?;
            raw.map(|raw| {
                let value = serde_json::from_str(&raw)?;
                deserialize_plan_compatible(value)
            })
            .transpose()
        })
        .await
    }

    async fn list_plans(&self, limit: usize) -> XduduResult<Vec<Plan>> {
        self.run_blocking(move |connection| {
            let mut statement = connection
                .prepare("SELECT plan_json FROM plans ORDER BY updated_at DESC LIMIT ?1")
                .map_err(|error| XduduError::tool(format!("准备计划列表查询失败：{error}")))?;
            let rows = statement
                .query_map([limit as i64], |row| row.get::<_, String>(0))
                .map_err(|error| XduduError::tool(format!("列出计划失败：{error}")))?;
            rows.map(|row| {
                let raw =
                    row.map_err(|error| XduduError::tool(format!("读取计划失败：{error}")))?;
                deserialize_plan_compatible(serde_json::from_str(&raw)?)
            })
            .collect()
        })
        .await
    }

    async fn update_plan_if_current(
        &self,
        plan: &Plan,
        expected_revision: u32,
        expected_status: PlanStatus,
    ) -> XduduResult<bool> {
        let plan = plan.clone();
        self.run_blocking(move |mut connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| XduduError::tool(format!("开始计划并发更新事务失败：{error}")))?;
            let (plan, raw) = plan_values(&plan)?;
            let changed = transaction
                .execute(
                    "UPDATE plans SET
                        status = ?3, schema_version = ?4, revision = ?5,
                        execution_version = ?6, plan_json = ?7,
                        created_at = ?8, updated_at = ?9, completed_at = ?10
                     WHERE id = ?1 AND session_id = ?2
                       AND revision = ?11 AND status = ?12",
                    params![
                        plan.id.to_string(),
                        plan.session_id.to_string(),
                        enum_name(plan.status)?,
                        i64::from(plan.schema_version),
                        i64::from(plan.revision),
                        plan.execution_version as i64,
                        raw,
                        plan.created_at.to_rfc3339(),
                        plan.updated_at.to_rfc3339(),
                        plan.completed_at.map(|value| value.to_rfc3339()),
                        i64::from(expected_revision),
                        enum_name(expected_status)?,
                    ],
                )
                .map_err(|error| XduduError::tool(format!("并发更新计划失败：{error}")))?;
            transaction
                .commit()
                .map_err(|error| XduduError::tool(format!("提交计划并发更新失败：{error}")))?;
            Ok(changed == 1)
        })
        .await
    }

    async fn append_revision_if_current(
        &self,
        plan: &Plan,
        revision: &PlanRevision,
        expected_revision: u32,
        expected_status: PlanStatus,
    ) -> XduduResult<bool> {
        let plan = plan.clone();
        let revision = revision.clone();
        self.run_blocking(move |mut connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| XduduError::tool(format!("开始追加计划修订事务失败：{error}")))?;
            let (plan, raw) = plan_values(&plan)?;
            let changed = transaction
                .execute(
                    "UPDATE plans SET
                        status = ?3, schema_version = ?4, revision = ?5,
                        execution_version = ?6, plan_json = ?7,
                        created_at = ?8, updated_at = ?9, completed_at = ?10
                     WHERE id = ?1 AND session_id = ?2
                       AND revision = ?11 AND status = ?12",
                    params![
                        plan.id.to_string(),
                        plan.session_id.to_string(),
                        enum_name(plan.status)?,
                        i64::from(plan.schema_version),
                        i64::from(plan.revision),
                        plan.execution_version as i64,
                        raw,
                        plan.created_at.to_rfc3339(),
                        plan.updated_at.to_rfc3339(),
                        plan.completed_at.map(|value| value.to_rfc3339()),
                        i64::from(expected_revision),
                        enum_name(expected_status)?,
                    ],
                )
                .map_err(|error| XduduError::tool(format!("并发更新计划修订失败：{error}")))?;
            if changed == 0 {
                transaction.rollback().map_err(|error| {
                    XduduError::tool(format!("回滚陈旧计划修订事务失败：{error}"))
                })?;
                return Ok(false);
            }
            insert_plan_revision(&transaction, &revision)?;
            transaction
                .commit()
                .map_err(|error| XduduError::tool(format!("提交计划修订事务失败：{error}")))?;
            Ok(true)
        })
        .await
    }

    async fn list_plan_revisions(&self, plan_id: Uuid) -> XduduResult<Vec<PlanRevision>> {
        self.run_blocking(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT revision_json FROM plan_revisions
                     WHERE plan_id = ?1 ORDER BY revision ASC",
                )
                .map_err(|error| XduduError::tool(format!("准备计划修订列表查询失败：{error}")))?;
            let rows = statement
                .query_map([plan_id.to_string()], |row| row.get::<_, String>(0))
                .map_err(|error| XduduError::tool(format!("列出计划修订失败：{error}")))?;
            rows.map(|row| {
                let raw =
                    row.map_err(|error| XduduError::tool(format!("读取计划修订失败：{error}")))?;
                let revision: PlanRevision = serde_json::from_str(&raw)?;
                revision.validate()?;
                Ok(revision)
            })
            .collect()
        })
        .await
    }

    async fn checkpoint_plan_execution(
        &self,
        plan: &Plan,
        session: &Session,
        expected_execution_version: u64,
        expected_status: PlanStatus,
    ) -> XduduResult<bool> {
        let plan = plan.clone();
        let session = session.clone();
        self.run_blocking(move |mut connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| XduduError::tool(format!("开始计划检查点事务失败：{error}")))?;
            let (plan, raw) = plan_values(&plan)?;
            let changed = transaction
                .execute(
                    "UPDATE plans SET status = ?3, schema_version = ?4,
                       revision = ?5, execution_version = ?6, plan_json = ?7,
                       updated_at = ?8, completed_at = ?9
                     WHERE id = ?1 AND session_id = ?2
                       AND revision = ?10 AND execution_version = ?11 AND status = ?12",
                    params![
                        plan.id.to_string(),
                        plan.session_id.to_string(),
                        enum_name(plan.status)?,
                        i64::from(plan.schema_version),
                        i64::from(plan.revision),
                        plan.execution_version as i64,
                        raw,
                        plan.updated_at.to_rfc3339(),
                        plan.completed_at.map(|value| value.to_rfc3339()),
                        i64::from(plan.revision),
                        expected_execution_version as i64,
                        enum_name(expected_status)?,
                    ],
                )
                .map_err(|error| XduduError::tool(format!("保存计划检查点失败：{error}")))?;
            if changed == 0 {
                transaction
                    .rollback()
                    .map_err(|error| XduduError::tool(format!("回滚计划检查点失败：{error}")))?;
                return Ok(false);
            }
            update_session(&transaction, &session)?;
            transaction
                .commit()
                .map_err(|error| XduduError::tool(format!("提交计划检查点失败：{error}")))?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl MemoryStore for SqliteSessionStore {
    async fn add_memory(
        &self,
        content: &str,
        source_session_id: Option<Uuid>,
    ) -> XduduResult<MemoryRecord> {
        let content = sanitize_memory_content(content).ok_or_else(|| {
            XduduError::validation(format!(
                "记忆内容不能为空且不超过 {MAX_MEMORY_BYTES} 字节。"
            ))
        })?;
        let now = Utc::now();
        let id = Uuid::new_v4();
        let record = MemoryRecord {
            id,
            content: content.clone(),
            source_session_id,
            created_at: now,
            updated_at: now,
        };
        let content = record.content.clone();
        let source = record.source_session_id.map(|id| id.to_string());
        let id_text = record.id.to_string();
        let created = record.created_at.to_rfc3339();
        let updated = record.updated_at.to_rfc3339();
        self.run_blocking(move |mut connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| XduduError::tool(format!("开始写入记忆事务失败：{error}")))?;
            transaction
                .execute(
                    "INSERT INTO memories(id, content, source_session_id, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![id_text, content, source, created, updated],
                )
                .map_err(|error| XduduError::tool(format!("写入记忆失败：{error}")))?;
            let rowid = transaction.last_insert_rowid();
            transaction
                .execute(
                    "INSERT INTO memories_fts(rowid, content) VALUES (?1, ?2)",
                    rusqlite::params![rowid, content],
                )
                .map_err(|error| XduduError::tool(format!("建立记忆全文索引失败：{error}")))?;
            transaction
                .commit()
                .map_err(|error| XduduError::tool(format!("提交记忆事务失败：{error}")))?;
            Ok(record)
        })
        .await
    }

    async fn list_memories(&self, limit: usize) -> XduduResult<Vec<MemoryRecord>> {
        self.run_blocking(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, content, source_session_id, created_at, updated_at
                     FROM memories ORDER BY created_at DESC LIMIT ?1",
                )
                .map_err(|error| XduduError::tool(format!("准备记忆列表失败：{error}")))?;
            let rows = statement
                .query_map(rusqlite::params![limit as i64], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .map_err(|error| XduduError::tool(format!("读取记忆列表失败：{error}")))?;
            rows.map(|row| {
                let (id, content, source, created, updated) =
                    row.map_err(|error| XduduError::tool(format!("解析记忆失败：{error}")))?;
                Ok(MemoryRecord {
                    id: Uuid::parse_str(&id)
                        .map_err(|error| XduduError::tool(format!("记忆 ID 无效：{error}")))?,
                    content,
                    source_session_id: source
                        .map(|value| {
                            Uuid::parse_str(&value).map_err(|error| {
                                XduduError::tool(format!("记忆来源会话 ID 无效：{error}"))
                            })
                        })
                        .transpose()?,
                    created_at: DateTime::parse_from_rfc3339(&created)
                        .map_err(|error| XduduError::tool(format!("记忆时间无效：{error}")))?
                        .with_timezone(&Utc),
                    updated_at: DateTime::parse_from_rfc3339(&updated)
                        .map_err(|error| XduduError::tool(format!("记忆时间无效：{error}")))?
                        .with_timezone(&Utc),
                })
            })
            .collect()
        })
        .await
    }

    async fn remove_memory(&self, id: Uuid) -> XduduResult<bool> {
        let id_text = id.to_string();
        self.run_blocking(move |mut connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| XduduError::tool(format!("开始删除记忆事务失败：{error}")))?;
            let rowid: Option<i64> = transaction
                .query_row(
                    "SELECT rowid FROM memories WHERE id = ?1",
                    rusqlite::params![id_text],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| XduduError::tool(format!("查询记忆失败：{error}")))?;
            let Some(rowid) = rowid else {
                return Ok(false);
            };
            transaction
                .execute(
                    "DELETE FROM memories_fts WHERE rowid = ?1",
                    rusqlite::params![rowid],
                )
                .map_err(|error| XduduError::tool(format!("删除记忆索引失败：{error}")))?;
            transaction
                .execute(
                    "DELETE FROM memories WHERE id = ?1",
                    rusqlite::params![id_text],
                )
                .map_err(|error| XduduError::tool(format!("删除记忆失败：{error}")))?;
            transaction
                .commit()
                .map_err(|error| XduduError::tool(format!("提交删除记忆事务失败：{error}")))?;
            Ok(true)
        })
        .await
    }

    async fn search_memories(&self, query: &str, limit: usize) -> XduduResult<Vec<MemoryRecord>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let fts_query = query
            .split_whitespace()
            .map(|token| format!("\"{}\"", token.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ");
        let limit = limit as i64;
        self.run_blocking(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT m.id, m.content, m.source_session_id, m.created_at, m.updated_at
                     FROM memories_fts f
                     JOIN memories m ON m.rowid = f.rowid
                     WHERE memories_fts MATCH ?1
                     ORDER BY rank LIMIT ?2",
                )
                .map_err(|error| XduduError::tool(format!("准备记忆检索失败：{error}")))?;
            let rows = statement
                .query_map(rusqlite::params![fts_query, limit], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .map_err(|error| XduduError::tool(format!("执行记忆检索失败：{error}")))?;
            rows.map(|row| {
                let (id, content, source, created, updated) =
                    row.map_err(|error| XduduError::tool(format!("解析记忆失败：{error}")))?;
                Ok(MemoryRecord {
                    id: Uuid::parse_str(&id)
                        .map_err(|error| XduduError::tool(format!("记忆 ID 无效：{error}")))?,
                    content,
                    source_session_id: source
                        .map(|value| {
                            Uuid::parse_str(&value).map_err(|error| {
                                XduduError::tool(format!("记忆来源会话 ID 无效：{error}"))
                            })
                        })
                        .transpose()?,
                    created_at: DateTime::parse_from_rfc3339(&created)
                        .map_err(|error| XduduError::tool(format!("记忆时间无效：{error}")))?
                        .with_timezone(&Utc),
                    updated_at: DateTime::parse_from_rfc3339(&updated)
                        .map_err(|error| XduduError::tool(format!("记忆时间无效：{error}")))?
                        .with_timezone(&Utc),
                })
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

    fn sample_plan(session_id: Uuid) -> Plan {
        Plan::new(
            session_id,
            "完成 SQLite 计划测试",
            vec![crate::plan::PlanStep::new("验证计划", "检查持久化结果")],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn 记忆_写入列表脱敏检索与删除() {
        let dir = tempdir().unwrap();
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let session = sample_session(dir.path());
        let session_id = session.id;
        store.create(&session).await.unwrap();

        // 写入两条记忆，内容自动脱敏。
        let first = store
            .add_memory("用户偏好：审批后运行测试 sk-abcdefghijk", Some(session_id))
            .await
            .unwrap();
        assert!(!first.content.contains("sk-abcdefghijk"));
        assert!(first.content.contains("[已脱敏]"));
        let second = store
            .add_memory("项目约定：命令执行前不需要逐条确认", Some(session_id))
            .await
            .unwrap();

        // 列表按创建时间倒序。
        let listed = store.list_memories(10).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second.id);
        assert_eq!(listed[0].source_session_id, Some(session_id));

        // FTS5 相关性检索命中第二条。
        let found = store.search_memories("命令执行前", 5).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, second.id);
        // 不相关查询无结果。
        assert!(
            store
                .search_memories("完全无关词", 5)
                .await
                .unwrap()
                .is_empty()
        );

        // 删除生效：列表与检索都不再返回，重复删除返回 false。
        assert!(store.remove_memory(first.id).await.unwrap());
        assert!(!store.remove_memory(first.id).await.unwrap());
        let listed = store.list_memories(10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, second.id);

        // 空与超长内容拒绝。
        assert!(store.add_memory("   ", None).await.is_err());
        assert!(
            store
                .add_memory(&"x".repeat(MAX_MEMORY_BYTES + 1), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn 记忆_删除后不再被全文检索命中() {
        let dir = tempdir().unwrap();
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let memory = store
            .add_memory("临时测试记忆：端口 8080", None)
            .await
            .unwrap();
        assert_eq!(store.search_memories("8080", 5).await.unwrap().len(), 1);
        store.remove_memory(memory.id).await.unwrap();
        assert!(store.search_memories("8080", 5).await.unwrap().is_empty());
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
    async fn sqlite_创建更新并按会话读取计划() {
        let dir = tempdir().unwrap();
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let session = sample_session(dir.path());
        store.create(&session).await.unwrap();
        let mut plan = sample_plan(session.id);
        store.create_plan(&plan).await.unwrap();
        plan.transition_to(crate::plan::PlanStatus::PendingApproval)
            .unwrap();
        store.update_plan(&plan).await.unwrap();

        assert_eq!(
            store.get_plan(plan.id).await.unwrap().unwrap().status,
            crate::plan::PlanStatus::PendingApproval
        );
        assert_eq!(
            store
                .latest_plan_for_session(session.id)
                .await
                .unwrap()
                .unwrap()
                .id,
            plan.id
        );

        plan.session_id = Uuid::new_v4();
        assert!(store.update_plan(&plan).await.is_err());
    }

    #[tokio::test]
    async fn sqlite_计划快照与乐观并发更新保持原子() {
        let dir = tempdir().unwrap();
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let session = sample_session(dir.path());
        store.create(&session).await.unwrap();
        let mut plan = sample_plan(session.id);
        store.create_plan(&plan).await.unwrap();
        assert_eq!(store.list_plan_revisions(plan.id).await.unwrap().len(), 1);

        plan.transition_to(PlanStatus::PendingApproval).unwrap();
        assert!(
            store
                .update_plan_if_current(&plan, 1, PlanStatus::Draft)
                .await
                .unwrap()
        );
        assert!(
            !store
                .update_plan_if_current(&plan, 1, PlanStatus::Draft)
                .await
                .unwrap()
        );

        let old_revision = plan.revision;
        plan.replace_for_revision(
            plan.goal.clone(),
            vec![
                crate::plan::PlanStep::new("新步骤", "完整修订")
                    .with_completion_criteria(["已完成".to_owned()]),
            ],
            "精简计划".into(),
        )
        .unwrap();
        let snapshot = PlanRevision::from_plan(&plan, Some("精简计划".into())).unwrap();
        assert!(
            store
                .append_revision_if_current(
                    &plan,
                    &snapshot,
                    old_revision,
                    PlanStatus::PendingApproval,
                )
                .await
                .unwrap()
        );
        let revisions = store.list_plan_revisions(plan.id).await.unwrap();
        assert_eq!(
            revisions
                .iter()
                .map(|item| item.revision)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(revisions[1].change_request.as_deref(), Some("精简计划"));
    }

    #[tokio::test]
    async fn 删除会话会级联删除计划修订() {
        let dir = tempdir().unwrap();
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let session = sample_session(dir.path());
        store.create(&session).await.unwrap();
        let plan = sample_plan(session.id);
        store.create_plan(&plan).await.unwrap();
        drop(store);

        let connection = Connection::open(dir.path().join(DATABASE_PATH)).unwrap();
        connection.execute("PRAGMA foreign_keys = ON", []).unwrap();
        connection
            .execute(
                "DELETE FROM sessions WHERE id = ?1",
                [session.id.to_string()],
            )
            .unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM plan_revisions WHERE plan_id = ?1",
                [plan.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn sqlite_计划落盘前脱敏秘密() {
        let dir = tempdir().unwrap();
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let session = sample_session(dir.path());
        store.create(&session).await.unwrap();
        let mut plan = sample_plan(session.id);
        plan.goal = "使用 sk-abcdefghijklmnopqrstuvwxyz 完成任务".into();
        store.create_plan(&plan).await.unwrap();
        let loaded = store.get_plan(plan.id).await.unwrap().unwrap();
        assert!(!loaded.goal.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(loaded.goal.contains("[已脱敏]"));
    }

    #[tokio::test]
    async fn sqlite_v1_数据库会自动补建计划表() {
        let dir = tempdir().unwrap();
        {
            let _store = SqliteSessionStore::new(dir.path()).unwrap();
        }
        {
            let connection = Connection::open(dir.path().join(DATABASE_PATH)).unwrap();
            connection.execute("DROP TABLE plans", []).unwrap();
            connection
                .execute("DELETE FROM schema_migrations WHERE version = 2", [])
                .unwrap();
        }

        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let session = sample_session(dir.path());
        store.create(&session).await.unwrap();
        let plan = sample_plan(session.id);
        store.create_plan(&plan).await.unwrap();
        assert_eq!(store.get_plan(plan.id).await.unwrap().unwrap().id, plan.id);
    }

    #[tokio::test]
    async fn sqlite_v6_推理字段迁移仅记录版本且旧会话可读() {
        let dir = tempdir().unwrap();
        let (session, plan) = {
            let store = SqliteSessionStore::new(dir.path()).unwrap();
            let session = sample_session(dir.path());
            store.create(&session).await.unwrap();
            let plan = sample_plan(session.id);
            store.create_plan(&plan).await.unwrap();
            (session, plan)
        };
        {
            let connection = Connection::open(dir.path().join(DATABASE_PATH)).unwrap();
            connection
                .execute("DELETE FROM schema_migrations WHERE version = 6", [])
                .unwrap();
        }
        {
            // 重新打开触发 v6 迁移；无表结构变化，会话 JSON 可继续读取。
            let store = SqliteSessionStore::new(dir.path()).unwrap();
            let loaded = store.get(session.id).await.unwrap().unwrap();
            assert_eq!(loaded.id, session.id);
            assert_eq!(store.get_plan(plan.id).await.unwrap().unwrap().id, plan.id);
            let connection = Connection::open(dir.path().join(DATABASE_PATH)).unwrap();
            let applied: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM schema_migrations WHERE version = 6",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(applied, 1);
        }
    }

    #[tokio::test]
    async fn sqlite_v2_计划自动升级并回填_revision_1() {
        let dir = tempdir().unwrap();
        let (session, plan) = {
            let store = SqliteSessionStore::new(dir.path()).unwrap();
            let session = sample_session(dir.path());
            store.create(&session).await.unwrap();
            let plan = sample_plan(session.id);
            store.create_plan(&plan).await.unwrap();
            (session, plan)
        };
        {
            let connection = Connection::open(dir.path().join(DATABASE_PATH)).unwrap();
            let mut value = serde_json::to_value(&plan).unwrap();
            let object = value.as_object_mut().unwrap();
            object.insert("schemaVersion".into(), serde_json::Value::from(1));
            object.remove("revision");
            object.remove("submittedAt");
            object.remove("reviewHistory");
            connection
                .execute("DELETE FROM schema_migrations WHERE version = 3", [])
                .unwrap();
            connection.execute("DROP TABLE plan_revisions", []).unwrap();
            connection
                .execute(
                    "UPDATE plans SET schema_version = 1, plan_json = ?2 WHERE id = ?1",
                    params![plan.id.to_string(), serde_json::to_string(&value).unwrap()],
                )
                .unwrap();
        }

        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let migrated = store
            .latest_plan_for_session(session.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(migrated.schema_version, crate::PLAN_SCHEMA_VERSION);
        assert_eq!(migrated.revision, 1);
        assert_eq!(store.list_plan_revisions(plan.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sqlite_v3_迁移遇到损坏计划会整体回滚() {
        let dir = tempdir().unwrap();
        let plan_id = {
            let store = SqliteSessionStore::new(dir.path()).unwrap();
            let session = sample_session(dir.path());
            store.create(&session).await.unwrap();
            let plan = sample_plan(session.id);
            store.create_plan(&plan).await.unwrap();
            plan.id
        };
        {
            let connection = Connection::open(dir.path().join(DATABASE_PATH)).unwrap();
            connection
                .execute("DELETE FROM schema_migrations WHERE version = 3", [])
                .unwrap();
            connection.execute("DROP TABLE plan_revisions", []).unwrap();
            connection
                .execute(
                    "UPDATE plans SET plan_json = 'not json' WHERE id = ?1",
                    [plan_id.to_string()],
                )
                .unwrap();
        }
        assert!(SqliteSessionStore::new(dir.path()).is_err());
        let connection = Connection::open(dir.path().join(DATABASE_PATH)).unwrap();
        let marker: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, 0);
        let revision_table: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'plan_revisions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revision_table, 0);
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
            reasoning: None,
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
        assert_eq!(recovered.current_state, AgentLoopState::Interrupted);
        assert_eq!(recovered.tool_calls[0].status, ToolCallStatus::Cancelled);
        assert!(
            recovered
                .messages
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some("pending-call"))
        );
    }

    #[tokio::test]
    async fn 重开数据库会保留等待计划审批的会话() {
        let dir = tempdir().unwrap();
        let mut session = sample_session(dir.path());
        session.status = SessionStatus::WaitingApproval;
        session.current_state = AgentLoopState::WaitingApproval;
        let mut plan = sample_plan(session.id);
        plan.transition_to(PlanStatus::PendingApproval).unwrap();
        {
            let store = SqliteSessionStore::new(dir.path()).unwrap();
            store.create(&session).await.unwrap();
            store.create_plan(&plan).await.unwrap();
        }
        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let recovered = store.get(session.id).await.unwrap().unwrap();
        assert_eq!(recovered.status, SessionStatus::WaitingApproval);
        assert_eq!(recovered.current_state, AgentLoopState::WaitingApproval);
    }

    #[tokio::test]
    async fn 重开数据库会暂停运行中计划且不自动重放步骤() {
        let dir = tempdir().unwrap();
        let session = sample_session(dir.path());
        let mut plan = sample_plan(session.id);
        plan.transition_to(PlanStatus::PendingApproval).unwrap();
        plan.transition_to(PlanStatus::Approved).unwrap();
        plan.transition_to(PlanStatus::Running).unwrap();
        plan.steps[0]
            .transition_to(crate::StepStatus::Ready)
            .unwrap();
        plan.steps[0]
            .transition_to(crate::StepStatus::Running)
            .unwrap();
        plan.current_step_id = Some(plan.steps[0].id);
        plan.steps[0].attempts.push(crate::PlanStepAttempt {
            id: Uuid::new_v4(),
            attempt: 1,
            status: crate::StepAttemptStatus::Running,
            summary: None,
            evidence: Vec::new(),
            error: None,
            tool_call_ids: vec!["unknown-call".into()],
            started_at: Utc::now(),
            ended_at: None,
        });
        {
            let store = SqliteSessionStore::new(dir.path()).unwrap();
            store.create(&session).await.unwrap();
            store.create_plan(&plan).await.unwrap();
        }

        let store = SqliteSessionStore::new(dir.path()).unwrap();
        let recovered = store.get_plan(plan.id).await.unwrap().unwrap();
        assert_eq!(recovered.status, PlanStatus::Paused);
        assert_eq!(recovered.steps[0].status, crate::StepStatus::Blocked);
        assert_eq!(
            recovered.steps[0].attempts[0].status,
            crate::StepAttemptStatus::Interrupted
        );
        assert_eq!(recovered.steps[0].attempts.len(), 1);
        assert!(recovered.paused_reason.unwrap().contains("不会自动重放"));
    }
}
