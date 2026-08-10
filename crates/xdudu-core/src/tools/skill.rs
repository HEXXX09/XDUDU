//! `skill` 工具：按名称加载本地技能（SKILL.md），正文注入当前轮系统提示词。
//!
//! 加载本身无副作用；`agent.skills = ask` 时进入审批门，`deny` 时索引
//! 不出现在工具描述且加载被拒绝。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    SideEffectKind,
    config::SkillMode,
    permission::PermissionLevel,
    skills::{Skill, find_skill},
};

use super::{
    Tool, ToolContext, ToolDefinition, ToolResult, object, reject_unknown_fields, required_string,
};

pub struct SkillTool {
    skills: Vec<Skill>,
    mode: SkillMode,
}

impl SkillTool {
    pub fn new(skills: Vec<Skill>, mode: SkillMode) -> Self {
        Self { skills, mode }
    }

    fn index(&self) -> String {
        self.skills
            .iter()
            .map(|skill| format!("- {}：{}", skill.name, skill.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        let description = if self.mode == SkillMode::Deny {
            "技能加载已被禁用。".to_owned()
        } else {
            format!(
                "按名称加载本地技能（SKILL.md），正文会注入系统提示词指导工作方式。可用技能：\n{}",
                self.index()
            )
        };
        ToolDefinition {
            name: "skill".into(),
            description,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "minLength": 1, "maxLength": 64 }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
            permission_level: PermissionLevel::ReadOnly,
            side_effect: SideEffectKind::None,
            default_timeout: Duration::from_secs(10),
        }
    }

    async fn needs_approval(&self, input: &Value, _context: &ToolContext) -> bool {
        if self.mode != SkillMode::Ask {
            return false;
        }
        let Some(name) = input.get("name").and_then(Value::as_str) else {
            return true;
        };
        find_skill(&self.skills, name).is_some()
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let map = object(input)?;
        let mut issues = Vec::new();
        reject_unknown_fields(map, &["name"], &mut issues);
        match required_string(map, "name", 64, &mut issues) {
            Some(name) if name.trim().is_empty() => {
                issues.push("name 不能为空。".into());
            }
            _ => {}
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        if self.mode == SkillMode::Deny {
            return ToolResult::failure(
                "SKILL_DENIED",
                "技能加载已被 agent.skills=deny 禁用。",
                context.started_at,
                json!({ "toolName": "skill" }),
            );
        }
        let name = input["name"].as_str().unwrap_or_default();
        match find_skill(&self.skills, name) {
            Some(skill) => ToolResult::success(
                json!({
                    "name": skill.name,
                    "description": skill.description,
                    "source": skill.source_label,
                    "loaded": true,
                }),
                context.started_at,
                json!({ "name": skill.name }),
            ),
            None => {
                let available = self
                    .skills
                    .iter()
                    .map(|skill| skill.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                ToolResult::failure(
                    "TOOL_NOT_FOUND",
                    format!("技能“{name}”不存在。可用技能：{available}"),
                    context.started_at,
                    json!({ "name": name, "availableSkills": self.skills.iter().map(|skill| skill.name.clone()).collect::<Vec<_>>() }),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use chrono::Utc;
    use tokio_util::sync::CancellationToken;

    use crate::changes::NoopChangeLedger;
    use crate::tools::ToolRegistry;

    fn make_tool(mode: SkillMode) -> (SkillTool, ToolRegistry) {
        let skills = vec![
            Skill {
                name: "probe".into(),
                description: "代码库探索".into(),
                body: "使用 search_text 与 file_read 调查。".into(),
                source_label: "project".into(),
                path: "/tmp/probe/SKILL.md".into(),
            },
            Skill {
                name: "review".into(),
                description: "代码审查".into(),
                body: "只读审查，不做修改。".into(),
                source_label: "user".into(),
                path: "/tmp/review/SKILL.md".into(),
            },
        ];
        (SkillTool::new(skills, mode), ToolRegistry::new())
    }

    fn context(cwd: &std::path::Path) -> ToolContext {
        use crate::permission::PermissionMode;
        ToolContext {
            session_id: uuid::Uuid::new_v4(),
            call_id: uuid::Uuid::new_v4(),
            cwd: cwd.to_path_buf(),
            permission_mode: PermissionMode::AutoSafe,
            cancellation: CancellationToken::new(),
            started_at: Utc::now(),
            change_ledger: Arc::new(NoopChangeLedger),
            progress: None,
            command_rules: Default::default(),
        }
    }

    #[tokio::test]
    async fn 加载命中技能并返回正文() {
        let (tool, _) = make_tool(SkillMode::Allow);
        let result = tool
            .execute(
                json!({ "name": "probe" }),
                context(std::path::Path::new("/tmp")),
            )
            .await;
        assert!(result.error.is_none());
        let output = result.output.unwrap();
        assert_eq!(output["name"], "probe");
        assert_eq!(output["loaded"], true);
        assert!(output.get("content").is_none());
    }

    #[tokio::test]
    async fn 未命中返回工具不存在() {
        let (tool, _) = make_tool(SkillMode::Allow);
        let result = tool
            .execute(
                json!({ "name": "missing" }),
                context(std::path::Path::new("/tmp")),
            )
            .await;
        assert_eq!(result.error.unwrap().code, "TOOL_NOT_FOUND");
    }

    #[tokio::test]
    async fn deny_模式拒绝加载且索引为空() {
        let (tool, _) = make_tool(SkillMode::Deny);
        let result = tool
            .execute(
                json!({ "name": "probe" }),
                context(std::path::Path::new("/tmp")),
            )
            .await;
        assert_eq!(result.error.unwrap().code, "SKILL_DENIED");
        assert!(tool.definition().description.contains("禁用"));
        assert!(!tool.definition().description.contains("probe"));
    }

    #[tokio::test]
    async fn ask_模式仅对命中技能请求审批() {
        let (tool, _) = make_tool(SkillMode::Ask);
        let ctx = context(std::path::Path::new("/tmp"));
        assert!(tool.needs_approval(&json!({ "name": "probe" }), &ctx).await);
        assert!(
            !tool
                .needs_approval(&json!({ "name": "missing" }), &ctx)
                .await
        );
        let (allow_tool, _) = make_tool(SkillMode::Allow);
        assert!(
            !allow_tool
                .needs_approval(&json!({ "name": "probe" }), &ctx)
                .await
        );
    }
}
