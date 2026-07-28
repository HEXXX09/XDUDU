//! 工具副作用分类与执行前审批接口。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    error::{XduduError, XduduResult},
    permission::PermissionLevel,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    Ask,
    Never,
    Always,
}

impl ApprovalMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
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
            "always" => Ok(Self::Always),
            _ => Err(XduduError::validation(format!(
                "非法审批模式：{value}。可选值：ask、never、always。"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SideEffectKind {
    None,
    WorkspaceWrite,
    ProcessExecution,
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
}

impl ApprovalDecision {
    pub fn approve(reason: impl Into<String>) -> Self {
        Self {
            approved: true,
            reason: reason.into(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            approved: false,
            reason: reason.into(),
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
    pub requested_at: DateTime<Utc>,
    pub decided_at: DateTime<Utc>,
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
        ApprovalDecision::approve("已通过显式自动审批策略。")
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
