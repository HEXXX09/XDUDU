//! M7 显式计划的领域模型与持久化接口。
//!
//! Plan 用于跨步骤、可审批、可恢复的任务执行；它不保存模型的隐藏推理，
//! 也不替代单次 Agent 请求内部的 ReAct 循环。

use std::collections::{HashMap, HashSet, VecDeque};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{XduduError, XduduResult, redact_text};

pub const PLAN_SCHEMA_VERSION: u32 = 1;
const MAX_STEPS: usize = 100;
const MAX_DEPENDENCIES: usize = 50;
const MAX_CRITERIA: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    PendingApproval,
    Approved,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Ready,
    Running,
    Completed,
    Failed,
    Blocked,
    Skipped,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completion_criteria: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

impl PlanStep {
    pub fn new(title: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            description: description.into(),
            status: StepStatus::Pending,
            dependencies: Vec::new(),
            completion_criteria: Vec::new(),
            result: None,
            error: None,
            started_at: None,
            completed_at: None,
        }
    }

    pub fn with_dependencies(mut self, dependencies: impl IntoIterator<Item = Uuid>) -> Self {
        self.dependencies = dependencies.into_iter().collect();
        self
    }

    pub fn with_completion_criteria(mut self, criteria: impl IntoIterator<Item = String>) -> Self {
        self.completion_criteria = criteria.into_iter().collect();
        self
    }

    pub fn transition_to(&mut self, next: StepStatus) -> XduduResult<()> {
        if self.status == next {
            return Ok(());
        }
        let allowed = matches!(
            (self.status, next),
            (
                StepStatus::Pending,
                StepStatus::Ready | StepStatus::Blocked | StepStatus::Cancelled
            ) | (
                StepStatus::Ready,
                StepStatus::Running
                    | StepStatus::Blocked
                    | StepStatus::Skipped
                    | StepStatus::Cancelled
            ) | (
                StepStatus::Running,
                StepStatus::Completed
                    | StepStatus::Failed
                    | StepStatus::Blocked
                    | StepStatus::Cancelled
            ) | (
                StepStatus::Blocked | StepStatus::Failed,
                StepStatus::Ready | StepStatus::Cancelled
            )
        );
        if !allowed {
            return Err(XduduError::validation(format!(
                "步骤状态不能从 {:?} 迁移到 {:?}。",
                self.status, next
            )));
        }
        let now = Utc::now();
        if next == StepStatus::Running && self.started_at.is_none() {
            self.started_at = Some(now);
        }
        if matches!(
            next,
            StepStatus::Completed
                | StepStatus::Failed
                | StepStatus::Skipped
                | StepStatus::Cancelled
        ) {
            self.completed_at = Some(now);
        } else {
            self.completed_at = None;
        }
        self.status = next;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub schema_version: u32,
    pub id: Uuid,
    pub session_id: Uuid,
    pub goal: String,
    pub status: PlanStatus,
    pub steps: Vec<PlanStep>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

impl Plan {
    pub fn new(
        session_id: Uuid,
        goal: impl Into<String>,
        steps: Vec<PlanStep>,
    ) -> XduduResult<Self> {
        let now = Utc::now();
        let plan = Self {
            schema_version: PLAN_SCHEMA_VERSION,
            id: Uuid::new_v4(),
            session_id,
            goal: goal.into(),
            status: PlanStatus::Draft,
            steps,
            created_at: now,
            updated_at: now,
            approved_at: None,
            completed_at: None,
        };
        plan.validate()?;
        Ok(plan)
    }

    pub fn validate(&self) -> XduduResult<()> {
        if self.schema_version != PLAN_SCHEMA_VERSION {
            return Err(XduduError::validation(format!(
                "不支持的计划 Schema 版本：{}。",
                self.schema_version
            )));
        }
        validate_text("计划目标", &self.goal, 4096)?;
        if self.steps.is_empty() || self.steps.len() > MAX_STEPS {
            return Err(XduduError::validation(format!(
                "计划步骤数量必须为 1 到 {MAX_STEPS}。"
            )));
        }

        let mut ids = HashSet::with_capacity(self.steps.len());
        for step in &self.steps {
            if !ids.insert(step.id) {
                return Err(XduduError::validation(format!(
                    "计划包含重复步骤 ID：{}。",
                    step.id
                )));
            }
            validate_text("步骤标题", &step.title, 256)?;
            if step.description.len() > 4096 {
                return Err(XduduError::validation("步骤描述不能超过 4096 字节。"));
            }
            if step.dependencies.len() > MAX_DEPENDENCIES {
                return Err(XduduError::validation(format!(
                    "单个步骤最多依赖 {MAX_DEPENDENCIES} 个步骤。"
                )));
            }
            if step.completion_criteria.len() > MAX_CRITERIA {
                return Err(XduduError::validation(format!(
                    "单个步骤最多包含 {MAX_CRITERIA} 条完成条件。"
                )));
            }
            for criterion in &step.completion_criteria {
                validate_text("步骤完成条件", criterion, 1024)?;
            }
        }

        for step in &self.steps {
            let mut dependencies = HashSet::new();
            for dependency in &step.dependencies {
                if *dependency == step.id {
                    return Err(XduduError::validation(format!(
                        "步骤“{}”不能依赖自身。",
                        step.title
                    )));
                }
                if !ids.contains(dependency) {
                    return Err(XduduError::validation(format!(
                        "步骤“{}”引用了不存在的依赖：{dependency}。",
                        step.title
                    )));
                }
                if !dependencies.insert(*dependency) {
                    return Err(XduduError::validation(format!(
                        "步骤“{}”包含重复依赖：{dependency}。",
                        step.title
                    )));
                }
            }
        }
        validate_acyclic(&self.steps)
    }

    pub fn transition_to(&mut self, next: PlanStatus) -> XduduResult<()> {
        if self.status == next {
            return Ok(());
        }
        let allowed = matches!(
            (self.status, next),
            (
                PlanStatus::Draft,
                PlanStatus::PendingApproval | PlanStatus::Cancelled
            ) | (
                PlanStatus::PendingApproval,
                PlanStatus::Draft | PlanStatus::Approved | PlanStatus::Cancelled
            ) | (
                PlanStatus::Approved,
                PlanStatus::Running | PlanStatus::Cancelled
            ) | (
                PlanStatus::Running,
                PlanStatus::Completed | PlanStatus::Failed | PlanStatus::Cancelled
            )
        );
        if !allowed {
            return Err(XduduError::validation(format!(
                "计划状态不能从 {:?} 迁移到 {:?}。",
                self.status, next
            )));
        }
        if next == PlanStatus::Completed
            && self
                .steps
                .iter()
                .any(|step| !matches!(step.status, StepStatus::Completed | StepStatus::Skipped))
        {
            return Err(XduduError::validation(
                "仍有未完成步骤，不能把计划标记为完成。",
            ));
        }
        let now = Utc::now();
        if next == PlanStatus::Approved {
            self.approved_at = Some(now);
        }
        if matches!(
            next,
            PlanStatus::Completed | PlanStatus::Failed | PlanStatus::Cancelled
        ) {
            self.completed_at = Some(now);
        } else {
            self.completed_at = None;
        }
        self.status = next;
        self.updated_at = now;
        Ok(())
    }

    pub fn ready_step_ids(&self) -> Vec<Uuid> {
        let statuses = self
            .steps
            .iter()
            .map(|step| (step.id, step.status))
            .collect::<HashMap<_, _>>();
        self.steps
            .iter()
            .filter(|step| step.status == StepStatus::Pending)
            .filter(|step| {
                step.dependencies.iter().all(|dependency| {
                    statuses.get(dependency).is_some_and(|status| {
                        matches!(status, StepStatus::Completed | StepStatus::Skipped)
                    })
                })
            })
            .map(|step| step.id)
            .collect()
    }
}

fn validate_text(label: &str, value: &str, max_bytes: usize) -> XduduResult<()> {
    if value.trim().is_empty() {
        return Err(XduduError::validation(format!("{label}不能为空。")));
    }
    if value.len() > max_bytes {
        return Err(XduduError::validation(format!(
            "{label}不能超过 {max_bytes} 字节。"
        )));
    }
    Ok(())
}

fn validate_acyclic(steps: &[PlanStep]) -> XduduResult<()> {
    let mut indegree = steps
        .iter()
        .map(|step| (step.id, step.dependencies.len()))
        .collect::<HashMap<_, _>>();
    let mut dependents: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for step in steps {
        for dependency in &step.dependencies {
            dependents.entry(*dependency).or_default().push(step.id);
        }
    }
    let mut queue = indegree
        .iter()
        .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
        .collect::<VecDeque<_>>();
    let mut visited = 0;
    while let Some(id) = queue.pop_front() {
        visited += 1;
        for dependent in dependents.get(&id).into_iter().flatten() {
            let degree = indegree
                .get_mut(dependent)
                .expect("依赖图只包含已经校验的步骤");
            *degree -= 1;
            if *degree == 0 {
                queue.push_back(*dependent);
            }
        }
    }
    if visited != steps.len() {
        return Err(XduduError::validation("计划步骤依赖不能形成环。"));
    }
    Ok(())
}

pub(crate) fn sanitized_plan(plan: &Plan) -> Plan {
    let mut sanitized = plan.clone();
    sanitized.goal = redact_text(&sanitized.goal);
    for step in &mut sanitized.steps {
        step.title = redact_text(&step.title);
        step.description = redact_text(&step.description);
        step.completion_criteria = step
            .completion_criteria
            .iter()
            .map(|criterion| redact_text(criterion))
            .collect();
        step.result = step.result.as_deref().map(redact_text);
        step.error = step.error.as_deref().map(redact_text);
    }
    sanitized
}

#[async_trait]
pub trait PlanStore: Send + Sync {
    async fn create_plan(&self, plan: &Plan) -> XduduResult<()>;
    async fn update_plan(&self, plan: &Plan) -> XduduResult<()>;
    async fn get_plan(&self, plan_id: Uuid) -> XduduResult<Option<Plan>>;
    async fn latest_plan_for_session(&self, session_id: Uuid) -> XduduResult<Option<Plan>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dependency_plan() -> Plan {
        let first = PlanStep::new("检查代码", "读取相关实现")
            .with_completion_criteria(["定位相关模块".to_owned()]);
        let second = PlanStep::new("修改代码", "实现需求").with_dependencies([first.id]);
        Plan::new(Uuid::new_v4(), "完成可靠修改", vec![first, second]).unwrap()
    }

    #[test]
    fn 计划校验依赖并计算可执行步骤() {
        let mut plan = dependency_plan();
        assert_eq!(plan.ready_step_ids(), vec![plan.steps[0].id]);
        plan.steps[0].transition_to(StepStatus::Ready).unwrap();
        plan.steps[0].transition_to(StepStatus::Running).unwrap();
        plan.steps[0].transition_to(StepStatus::Completed).unwrap();
        assert_eq!(plan.ready_step_ids(), vec![plan.steps[1].id]);
    }

    #[test]
    fn 计划拒绝循环依赖() {
        let mut first = PlanStep::new("A", "");
        let mut second = PlanStep::new("B", "");
        first.dependencies.push(second.id);
        second.dependencies.push(first.id);
        let error = Plan::new(Uuid::new_v4(), "循环计划", vec![first, second]).unwrap_err();
        assert!(error.message.contains("形成环"));
    }

    #[test]
    fn 计划和步骤只允许显式状态迁移() {
        let mut plan = dependency_plan();
        assert!(plan.transition_to(PlanStatus::Running).is_err());
        plan.transition_to(PlanStatus::PendingApproval).unwrap();
        plan.transition_to(PlanStatus::Approved).unwrap();
        plan.transition_to(PlanStatus::Running).unwrap();
        assert!(plan.transition_to(PlanStatus::Completed).is_err());
    }

    #[test]
    fn 计划序列化包含版本且脱敏秘密() {
        let mut plan = dependency_plan();
        plan.goal = "不要泄漏 sk-abcdefghijklmnopqrstuvwxyz".into();
        let sanitized = sanitized_plan(&plan);
        assert!(!sanitized.goal.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        let value = serde_json::to_value(sanitized).unwrap();
        assert_eq!(value["schemaVersion"], PLAN_SCHEMA_VERSION);
    }
}
