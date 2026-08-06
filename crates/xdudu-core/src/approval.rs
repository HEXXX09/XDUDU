//! 工具副作用分类与执行前审批接口。

use std::{
    collections::BTreeSet,
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
    error::{ErrorKind, XduduError, XduduResult},
    permission::PermissionLevel,
};

const APPROVAL_RULES_SCHEMA_VERSION: u32 = 1;
const MAX_APPROVAL_RULES_BYTES: u64 = 64 * 1024;
const MAX_APPROVAL_RULES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    Ask,
    Never,
    /// Claude Code acceptEdits 式：自动接受工作区文件编辑，
    /// 命令与网络访问仍按 ask 流程询问。
    AcceptEdits,
    Always,
}

impl ApprovalMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
            Self::AcceptEdits => "accept-edits",
            Self::Always => "always",
        }
    }
}

impl std::str::FromStr for ApprovalMode {
    type Err = XduduError;

    fn from_str(value: &str) -> XduduResult<Self> {
        match value {
            "ask" => Ok(Self::Ask),
            "never" => Ok(Self::Never),
            "accept-edits" => Ok(Self::AcceptEdits),
            "always" => Ok(Self::Always),
            _ => Err(XduduError::validation(format!(
                "非法审批模式：{value}。可选值：ask、never、accept-edits、always。"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SideEffectKind {
    None,
    WorkspaceWrite,
    ProcessExecution,
    NetworkAccess,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalScope {
    #[default]
    Once,
    Session,
    Always,
}

impl ApprovalScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Session => "session",
            Self::Always => "always",
        }
    }
}

impl SideEffectKind {
    pub const fn requires_approval(self) -> bool {
        !matches!(self, Self::None)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WorkspaceWrite => "workspace-write",
            Self::ProcessExecution => "process-execution",
            Self::NetworkAccess => "network-access",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub id: Uuid,
    pub session_id: Uuid,
    pub tool_name: String,
    pub input: Value,
    pub permission_level: PermissionLevel,
    pub side_effect: SideEffectKind,
    pub requested_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ApprovalDecision {
    pub approved: bool,
    pub reason: String,
    pub scope: ApprovalScope,
}

impl ApprovalDecision {
    pub fn approve(reason: impl Into<String>) -> Self {
        Self::approve_with_scope(reason, ApprovalScope::Once)
    }

    pub fn approve_with_scope(reason: impl Into<String>, scope: ApprovalScope) -> Self {
        Self {
            approved: true,
            reason: reason.into(),
            scope,
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            approved: false,
            reason: reason.into(),
            scope: ApprovalScope::Once,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRecord {
    pub id: Uuid,
    pub approved: bool,
    pub reason: String,
    pub side_effect: SideEffectKind,
    #[serde(default)]
    pub scope: ApprovalScope,
    pub requested_at: DateTime<Utc>,
    pub decided_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRule {
    pub tool_name: String,
    pub side_effect: SideEffectKind,
}

impl ApprovalRule {
    pub fn from_request(request: &ApprovalRequest) -> Self {
        Self {
            tool_name: request.tool_name.clone(),
            side_effect: request.side_effect,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalRulesFile {
    schema_version: u32,
    rules: BTreeSet<ApprovalRule>,
}

#[derive(Debug, Clone)]
pub struct JsonApprovalRuleStore {
    path: PathBuf,
    rules: Arc<Mutex<BTreeSet<ApprovalRule>>>,
}

impl JsonApprovalRuleStore {
    pub async fn open(path: impl Into<PathBuf>) -> XduduResult<Self> {
        let path = path.into();
        let rules = match fs::metadata(&path).await {
            Ok(metadata) if metadata.len() > MAX_APPROVAL_RULES_BYTES => {
                return Err(XduduError::new(
                    ErrorKind::ConfigError,
                    "永久审批规则文件超过 64 KiB，拒绝加载。",
                ));
            }
            Ok(_) => {
                let data = fs::read(&path).await?;
                let file: ApprovalRulesFile = serde_json::from_slice(&data).map_err(|error| {
                    XduduError::new(
                        ErrorKind::ConfigError,
                        format!("永久审批规则文件无效：{error}"),
                    )
                })?;
                if file.schema_version != APPROVAL_RULES_SCHEMA_VERSION
                    || file.rules.len() > MAX_APPROVAL_RULES
                {
                    return Err(XduduError::new(
                        ErrorKind::ConfigError,
                        "永久审批规则版本或数量无效。",
                    ));
                }
                file.rules
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            rules: Arc::new(Mutex::new(rules)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn contains(&self, rule: &ApprovalRule) -> bool {
        self.rules.lock().await.contains(rule)
    }

    pub async fn list(&self) -> Vec<ApprovalRule> {
        self.rules.lock().await.iter().cloned().collect()
    }

    pub async fn allow(&self, rule: ApprovalRule) -> XduduResult<()> {
        let mut rules = self.rules.lock().await;
        if rules.contains(&rule) {
            return Ok(());
        }
        if rules.len() >= MAX_APPROVAL_RULES {
            return Err(XduduError::validation(format!(
                "永久审批规则不能超过 {MAX_APPROVAL_RULES} 条。"
            )));
        }
        rules.insert(rule.clone());
        if let Err(error) = self.write_locked(&rules).await {
            rules.remove(&rule);
            return Err(error);
        }
        Ok(())
    }

    pub async fn revoke(&self, tool_name: &str) -> XduduResult<usize> {
        let mut rules = self.rules.lock().await;
        let original = rules.clone();
        let before = rules.len();
        rules.retain(|rule| rule.tool_name != tool_name);
        let removed = before - rules.len();
        if removed > 0 {
            if let Err(error) = self.write_locked(&rules).await {
                *rules = original;
                return Err(error);
            }
        }
        Ok(removed)
    }

    pub async fn clear(&self) -> XduduResult<usize> {
        let mut rules = self.rules.lock().await;
        let original = rules.clone();
        let removed = rules.len();
        rules.clear();
        match fs::remove_file(&self.path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                *rules = original;
                return Err(error.into());
            }
        }
        Ok(removed)
    }

    async fn write_locked(&self, rules: &BTreeSet<ApprovalRule>) -> XduduResult<()> {
        let parent = self.path.parent().ok_or_else(|| {
            XduduError::new(ErrorKind::ConfigError, "永久审批规则路径缺少父目录。")
        })?;
        fs::create_dir_all(parent).await?;
        let temporary = self
            .path
            .with_extension(format!("json.{}.tmp", Uuid::new_v4()));
        let file = ApprovalRulesFile {
            schema_version: APPROVAL_RULES_SCHEMA_VERSION,
            rules: rules.clone(),
        };
        let data = serde_json::to_vec_pretty(&file)?;
        if let Err(error) = async {
            fs::write(&temporary, data).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
            }
            #[cfg(windows)]
            if fs::try_exists(&self.path).await.unwrap_or(false) {
                fs::remove_file(&self.path).await?;
            }
            fs::rename(&temporary, &self.path).await
        }
        .await
        {
            let _ = fs::remove_file(&temporary).await;
            return Err(XduduError::new(
                ErrorKind::ConfigError,
                format!("写入永久审批规则失败：{error}"),
            ));
        }
        Ok(())
    }
}

#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn review(&self, request: &ApprovalRequest) -> ApprovalDecision;
}

#[derive(Debug, Default)]
pub struct AllowAllApprovalGate;

#[async_trait]
impl ApprovalGate for AllowAllApprovalGate {
    async fn review(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::approve_with_scope("已通过显式自动审批策略。", ApprovalScope::Always)
    }
}

#[derive(Debug, Default)]
pub struct DenyAllApprovalGate;

#[async_trait]
impl ApprovalGate for DenyAllApprovalGate {
    async fn review(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::deny("当前运行方式不允许自动执行副作用工具。")
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn rule(tool_name: &str) -> ApprovalRule {
        ApprovalRule {
            tool_name: tool_name.into(),
            side_effect: SideEffectKind::NetworkAccess,
        }
    }

    #[tokio::test]
    async fn 永久审批规则可以保存重载和撤销() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("approval-rules.json");
        let store = JsonApprovalRuleStore::open(&path).await.unwrap();
        store.allow(rule("web_fetch")).await.unwrap();
        assert!(store.contains(&rule("web_fetch")).await);

        let reopened = JsonApprovalRuleStore::open(&path).await.unwrap();
        assert_eq!(reopened.list().await, vec![rule("web_fetch")]);
        assert_eq!(reopened.revoke("web_fetch").await.unwrap(), 1);
        assert!(reopened.list().await.is_empty());
    }

    #[tokio::test]
    async fn 清除规则会删除持久化文件() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("approval-rules.json");
        let store = JsonApprovalRuleStore::open(&path).await.unwrap();
        store.allow(rule("web_fetch")).await.unwrap();
        assert_eq!(store.clear().await.unwrap(), 1);
        assert!(!path.exists());
    }

    #[test]
    fn 旧审批记录缺少_scope_时默认_once() {
        let value = serde_json::json!({
            "id": Uuid::new_v4(),
            "approved": true,
            "reason": "旧记录",
            "sideEffect": "workspace-write",
            "requestedAt": Utc::now(),
            "decidedAt": Utc::now()
        });
        let record: ApprovalRecord = serde_json::from_value(value).unwrap();
        assert_eq!(record.scope, ApprovalScope::Once);
    }

    #[test]
    fn accept_edits_模式自动接受编辑但命令仍需审批() {
        // 纯枚举语义验证：accept-edits 严格位于 ask 与 always 之间。
        let rank = |mode: ApprovalMode| match mode {
            ApprovalMode::Never => 0,
            ApprovalMode::Ask => 1,
            ApprovalMode::AcceptEdits => 2,
            ApprovalMode::Always => 3,
        };
        assert!(rank(ApprovalMode::Ask) < rank(ApprovalMode::AcceptEdits));
        assert!(rank(ApprovalMode::AcceptEdits) < rank(ApprovalMode::Always));
        assert_eq!(ApprovalMode::AcceptEdits.as_str(), "accept-edits");
        assert_eq!(
            "accept-edits".parse::<ApprovalMode>().unwrap(),
            ApprovalMode::AcceptEdits
        );
        assert!("accept_edits".parse::<ApprovalMode>().is_err());
    }
}
