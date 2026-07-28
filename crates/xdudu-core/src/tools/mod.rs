//! 内置工具、输入校验、权限检查与统一执行入口。

mod file_read;
mod file_write;
mod path_policy;
mod terminal_exec;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    approval::{
        ApprovalGate, ApprovalRecord, ApprovalRequest, DenyAllApprovalGate, SideEffectKind,
    },
    changes::{ChangeLedger, NoopChangeLedger},
    error::{XduduError, XduduResult},
    permission::{PermissionLevel, PermissionMode},
    provider::ProviderToolDefinition,
};

pub use file_read::FileReadTool;
pub use file_write::FileWriteTool;
pub use terminal_exec::TerminalExecTool;

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub permission_level: PermissionLevel,
    pub side_effect: SideEffectKind,
    pub default_timeout: Duration,
}

impl ToolDefinition {
    pub fn provider_definition(&self) -> ProviderToolDefinition {
        ProviderToolDefinition {
            name: self.name.to_owned(),
            description: self.description.to_owned(),
            input_schema: self.input_schema.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub session_id: Uuid,
    pub call_id: Uuid,
    pub cwd: PathBuf,
    pub permission_mode: PermissionMode,
    pub cancellation: CancellationToken,
    pub started_at: DateTime<Utc>,
    pub change_ledger: Arc<dyn ChangeLedger>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolErrorPayload {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub success: bool,
    pub output: Option<Value>,
    pub error: Option<ToolErrorPayload>,
    pub duration_ms: u64,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub metadata: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Box<ApprovalRecord>>,
}

impl ToolResult {
    pub fn failure(
        code: impl Into<String>,
        message: impl Into<String>,
        started_at: DateTime<Utc>,
        details: Value,
    ) -> Self {
        Self {
            success: false,
            output: None,
            error: Some(ToolErrorPayload {
                code: code.into(),
                message: message.into(),
                retryable: false,
                details,
            }),
            duration_ms: elapsed_ms(started_at),
            started_at,
            ended_at: Utc::now(),
            metadata: Value::Object(Default::default()),
            approval: None,
        }
    }

    pub fn success(output: Value, started_at: DateTime<Utc>, metadata: Value) -> Self {
        Self {
            success: true,
            output: Some(output),
            error: None,
            duration_ms: elapsed_ms(started_at),
            started_at,
            ended_at: Utc::now(),
            metadata,
            approval: None,
        }
    }
}

fn elapsed_ms(started_at: DateTime<Utc>) -> u64 {
    Utc::now()
        .signed_duration_since(started_at)
        .num_milliseconds()
        .max(0) as u64
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn validate(&self, input: &Value) -> Result<(), Vec<String>>;
    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult;
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    approval_gate: Arc<dyn ApprovalGate>,
    change_ledger: Arc<dyn ChangeLedger>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            tools: HashMap::new(),
            approval_gate: Arc::new(DenyAllApprovalGate),
            change_ledger: Arc::new(NoopChangeLedger),
        }
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_approval_gate(approval_gate: Arc<dyn ApprovalGate>) -> Self {
        Self {
            tools: HashMap::new(),
            approval_gate,
            change_ledger: Arc::new(NoopChangeLedger),
        }
    }

    pub fn with_runtime(
        approval_gate: Arc<dyn ApprovalGate>,
        change_ledger: Arc<dyn ChangeLedger>,
    ) -> Self {
        Self {
            tools: HashMap::new(),
            approval_gate,
            change_ledger,
        }
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> XduduResult<()> {
        let name = tool.definition().name.to_owned();
        if self.tools.contains_key(&name) {
            return Err(XduduError::tool(format!("工具“{name}”已经注册。")));
        }
        self.tools.insert(name, Arc::new(tool));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .values()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        definitions.sort_by_key(|definition| definition.name);
        definitions
    }

    pub async fn execute(
        &self,
        name: &str,
        input: Value,
        session_id: Uuid,
        cwd: impl AsRef<Path>,
        permission_mode: PermissionMode,
        cancellation: CancellationToken,
    ) -> ToolResult {
        let started_at = Utc::now();
        let Some(tool) = self.tools.get(name) else {
            return ToolResult::failure(
                "TOOL_NOT_FOUND",
                format!("工具“{name}”尚未注册。"),
                started_at,
                json!({ "availableTools": self.tools.keys().collect::<Vec<_>>() }),
            );
        };
        let definition = tool.definition();
        if !permission_mode.allows(definition.permission_level) {
            return ToolResult::failure(
                "PERMISSION_DENIED",
                format!(
                    "权限不足：工具“{name}”需要“{}”，当前模式“{}”不允许。",
                    definition.permission_level.as_str(),
                    permission_mode.as_str()
                ),
                started_at,
                json!({
                    "toolName": name,
                    "requiredLevel": definition.permission_level.as_str(),
                    "permissionMode": permission_mode.as_str(),
                }),
            );
        }
        if let Err(issues) = tool.validate(&input) {
            return ToolResult::failure(
                "INVALID_TOOL_INPUT",
                format!("工具“{name}”的输入参数无效。"),
                started_at,
                json!({ "issues": issues }),
            );
        }
        if cancellation.is_cancelled() {
            return ToolResult::failure(
                "TOOL_ABORTED",
                "工具调用已中断。",
                started_at,
                Value::Object(Default::default()),
            );
        }
        let context = ToolContext {
            session_id,
            call_id: Uuid::new_v4(),
            cwd: cwd.as_ref().to_path_buf(),
            permission_mode,
            cancellation: cancellation.clone(),
            started_at,
            change_ledger: Arc::clone(&self.change_ledger),
        };
        let mut approval = None;
        if definition.side_effect.requires_approval() {
            let request = ApprovalRequest {
                id: Uuid::new_v4(),
                session_id,
                tool_name: name.to_owned(),
                input: input.clone(),
                permission_level: definition.permission_level,
                side_effect: definition.side_effect,
                requested_at: Utc::now(),
            };
            let decision = self.approval_gate.review(&request).await;
            let record = ApprovalRecord {
                id: request.id,
                approved: decision.approved,
                reason: decision.reason.clone(),
                side_effect: request.side_effect,
                requested_at: request.requested_at,
                decided_at: Utc::now(),
            };
            if !decision.approved {
                let mut result = ToolResult::failure(
                    "APPROVAL_DENIED",
                    format!("工具“{name}”未获批准：{}", decision.reason),
                    started_at,
                    json!({
                        "toolName": name,
                        "sideEffect": definition.side_effect.as_str(),
                    }),
                );
                result.approval = Some(Box::new(record));
                return result;
            }
            approval = Some(Box::new(record));
        }
        let started = Instant::now();
        match tokio::time::timeout(definition.default_timeout, tool.execute(input, context)).await {
            Ok(mut result) => {
                result.duration_ms = started.elapsed().as_millis() as u64;
                result.approval = approval;
                result
            }
            Err(_) => {
                cancellation.cancel();
                ToolResult::failure(
                    "TOOL_TIMEOUT",
                    format!("工具“{name}”执行超时。"),
                    started_at,
                    json!({ "timeoutMs": definition.default_timeout.as_millis() }),
                )
            }
        }
    }
}

pub fn register_builtins(registry: &mut ToolRegistry) -> XduduResult<()> {
    registry.register(FileReadTool)?;
    registry.register(FileWriteTool)?;
    registry.register(TerminalExecTool)?;
    Ok(())
}

fn object(input: &Value) -> Result<&serde_json::Map<String, Value>, Vec<String>> {
    input
        .as_object()
        .ok_or_else(|| vec!["输入必须是 JSON 对象。".to_owned()])
}

fn reject_unknown_fields(
    map: &serde_json::Map<String, Value>,
    allowed: &[&str],
    issues: &mut Vec<String>,
) {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            issues.push(format!("不支持字段：{key}"));
        }
    }
}

fn required_string<'a>(
    map: &'a serde_json::Map<String, Value>,
    key: &str,
    max_len: usize,
    issues: &mut Vec<String>,
) -> Option<&'a str> {
    match map.get(key).and_then(Value::as_str) {
        Some(value) if !value.is_empty() && value.len() <= max_len => Some(value),
        _ => {
            issues.push(format!("{key} 必须是 1 到 {max_len} 字节的字符串。"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn 注册中心拒绝重复工具和未知工具() {
        let mut registry = ToolRegistry::new();
        registry.register(FileReadTool).unwrap();
        assert!(registry.register(FileReadTool).is_err());
        let result = registry
            .execute(
                "missing",
                json!({}),
                Uuid::new_v4(),
                tempdir().unwrap().path(),
                PermissionMode::AutoSafe,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.error.unwrap().code, "TOOL_NOT_FOUND");
    }

    #[tokio::test]
    async fn 注册中心强制执行权限() {
        let mut registry = ToolRegistry::new();
        registry.register(FileWriteTool).unwrap();
        let dir = tempdir().unwrap();
        let result = registry
            .execute(
                "file_write",
                json!({"path":"blocked.txt","content":"blocked"}),
                Uuid::new_v4(),
                dir.path(),
                PermissionMode::ReadOnly,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.error.unwrap().code, "PERMISSION_DENIED");
        assert!(!dir.path().join("blocked.txt").exists());
    }
}
