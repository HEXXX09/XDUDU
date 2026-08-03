//! M7 显式计划的领域模型与持久化接口。
//!
//! Plan 用于跨步骤、可审批、可恢复的任务执行；它不保存模型的隐藏推理，
//! 也不替代单次 Agent 请求内部的 ReAct 循环。

use std::collections::{HashMap, HashSet, VecDeque};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{XduduError, XduduResult, redact_text};

pub const PLAN_SCHEMA_VERSION: u32 = 3;
pub const MAX_PLAN_REVISIONS: u32 = 100;
pub const MAX_STEP_ATTEMPTS: usize = 20;
const MAX_STEPS: usize = 100;
const MAX_DEPENDENCIES: usize = 50;
const MAX_CRITERIA: usize = 20;
const MAX_REVIEW_HISTORY: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    PendingApproval,
    Approved,
    Running,
    Paused,
    Completed,
    Failed,
    Rejected,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReviewDecision {
    Approved,
    ChangesRequested,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReviewRecord {
    pub id: Uuid,
    pub revision: u32,
    pub decision: PlanReviewDecision,
    pub reason: String,
    pub decided_at: DateTime<Utc>,
}

impl PlanReviewRecord {
    pub fn new(
        revision: u32,
        decision: PlanReviewDecision,
        reason: impl Into<String>,
    ) -> XduduResult<Self> {
        let reason = reason.into();
        validate_text("计划审阅原因", &reason, 4096)?;
        Ok(Self {
            id: Uuid::new_v4(),
            revision,
            decision,
            reason,
            decided_at: Utc::now(),
        })
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepAttemptStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionEvidence {
    pub criterion_index: u32,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStepAttempt {
    pub id: Uuid,
    pub attempt: u32,
    pub status: StepAttemptStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<CompletionEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_call_ids: Vec<String>,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<PlanStepAttempt>,
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
            attempts: Vec::new(),
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
    pub revision: u32,
    #[serde(default)]
    pub execution_version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_step_id: Option<Uuid>,
    pub steps: Vec<PlanStep>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_history: Vec<PlanReviewRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRevision {
    pub schema_version: u32,
    pub plan_id: Uuid,
    pub revision: u32,
    pub goal: String,
    pub steps: Vec<PlanStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_request: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl PlanRevision {
    pub fn from_plan(plan: &Plan, change_request: Option<String>) -> XduduResult<Self> {
        if let Some(request) = &change_request {
            validate_text("计划修改要求", request, 4096)?;
        }
        let revision = Self {
            schema_version: PLAN_SCHEMA_VERSION,
            plan_id: plan.id,
            revision: plan.revision,
            goal: plan.goal.clone(),
            steps: plan.steps.clone(),
            change_request,
            created_at: Utc::now(),
        };
        revision.validate()?;
        Ok(revision)
    }

    pub fn validate(&self) -> XduduResult<()> {
        if self.schema_version != PLAN_SCHEMA_VERSION {
            return Err(XduduError::validation(format!(
                "不支持的计划修订 Schema 版本：{}。",
                self.schema_version
            )));
        }
        if !(1..=MAX_PLAN_REVISIONS).contains(&self.revision) {
            return Err(XduduError::validation(format!(
                "计划修订版本必须为 1 到 {MAX_PLAN_REVISIONS}。"
            )));
        }
        if let Some(request) = &self.change_request {
            validate_text("计划修改要求", request, 4096)?;
        }
        validate_plan_content(&self.goal, &self.steps)
    }
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
            revision: 1,
            execution_version: 0,
            current_step_id: None,
            steps,
            created_at: now,
            updated_at: now,
            started_at: None,
            paused_reason: None,
            submitted_at: None,
            approved_at: None,
            completed_at: None,
            review_history: Vec::new(),
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
        if !(1..=MAX_PLAN_REVISIONS).contains(&self.revision) {
            return Err(XduduError::validation(format!(
                "计划修订版本必须为 1 到 {MAX_PLAN_REVISIONS}。"
            )));
        }
        if self.review_history.len() > MAX_REVIEW_HISTORY {
            return Err(XduduError::validation(format!(
                "计划审阅记录不能超过 {MAX_REVIEW_HISTORY} 条。"
            )));
        }
        for review in &self.review_history {
            if review.revision == 0 || review.revision > self.revision {
                return Err(XduduError::validation("计划审阅记录引用了无效修订版本。"));
            }
            validate_text("计划审阅原因", &review.reason, 4096)?;
        }
        if let Some(reason) = &self.paused_reason {
            validate_text("计划暂停原因", reason, 4096)?;
        }
        validate_plan_content(&self.goal, &self.steps)
    }

    pub fn add_review(
        &mut self,
        decision: PlanReviewDecision,
        reason: impl Into<String>,
    ) -> XduduResult<()> {
        if self.review_history.len() >= MAX_REVIEW_HISTORY {
            return Err(XduduError::validation(format!(
                "计划审阅记录不能超过 {MAX_REVIEW_HISTORY} 条。"
            )));
        }
        self.review_history
            .push(PlanReviewRecord::new(self.revision, decision, reason)?);
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn replace_for_revision(
        &mut self,
        goal: String,
        steps: Vec<PlanStep>,
        change_request: String,
    ) -> XduduResult<()> {
        if self.status != PlanStatus::PendingApproval {
            return Err(XduduError::validation("只有等待审批的计划可以请求修改。"));
        }
        if self.revision >= MAX_PLAN_REVISIONS {
            return Err(XduduError::validation(format!(
                "计划修订版本不能超过 {MAX_PLAN_REVISIONS}。"
            )));
        }
        validate_text("计划修改要求", &change_request, 4096)?;
        validate_plan_content(&goal, &steps)?;
        self.add_review(PlanReviewDecision::ChangesRequested, change_request)?;
        self.goal = goal;
        self.steps = steps;
        self.revision += 1;
        self.status = PlanStatus::PendingApproval;
        self.submitted_at = Some(Utc::now());
        self.approved_at = None;
        self.completed_at = None;
        self.execution_version = 0;
        self.current_step_id = None;
        self.started_at = None;
        self.paused_reason = None;
        self.updated_at = Utc::now();
        self.validate()
    }

    pub fn validate_content(&self) -> XduduResult<()> {
        validate_plan_content(&self.goal, &self.steps)
    }
}

fn validate_plan_content(goal: &str, steps: &[PlanStep]) -> XduduResult<()> {
    validate_text("计划目标", goal, 4096)?;
    if steps.is_empty() || steps.len() > MAX_STEPS {
        return Err(XduduError::validation(format!(
            "计划步骤数量必须为 1 到 {MAX_STEPS}。"
        )));
    }

    let mut ids = HashSet::with_capacity(steps.len());
    for step in steps {
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
        if step.attempts.len() > MAX_STEP_ATTEMPTS {
            return Err(XduduError::validation(format!(
                "单个步骤最多保留 {MAX_STEP_ATTEMPTS} 次执行尝试。"
            )));
        }
        for (index, attempt) in step.attempts.iter().enumerate() {
            if attempt.attempt != index as u32 + 1 {
                return Err(XduduError::validation("步骤执行尝试序号不连续。"));
            }
            if let Some(summary) = &attempt.summary {
                validate_text("步骤执行摘要", summary, 4096)?;
            }
            if let Some(error) = &attempt.error {
                validate_text("步骤执行错误", error, 4096)?;
            }
            validate_completion_evidence(&step.completion_criteria, &attempt.evidence, false)?;
        }
    }

    for step in steps {
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
    validate_acyclic(steps)
}

impl Plan {
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
                PlanStatus::Draft
                    | PlanStatus::Approved
                    | PlanStatus::Rejected
                    | PlanStatus::Cancelled
            ) | (
                PlanStatus::Approved,
                PlanStatus::Running | PlanStatus::Cancelled
            ) | (
                PlanStatus::Running,
                PlanStatus::Completed
                    | PlanStatus::Paused
                    | PlanStatus::Failed
                    | PlanStatus::Cancelled
            ) | (
                PlanStatus::Paused,
                PlanStatus::Running | PlanStatus::Failed | PlanStatus::Cancelled
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
        if next == PlanStatus::PendingApproval {
            self.submitted_at = Some(now);
        }
        if next == PlanStatus::Approved {
            self.approved_at = Some(now);
        }
        if next == PlanStatus::Running && self.started_at.is_none() {
            self.started_at = Some(now);
        }
        if next != PlanStatus::Paused {
            self.paused_reason = None;
        }
        if matches!(
            next,
            PlanStatus::Completed
                | PlanStatus::Failed
                | PlanStatus::Rejected
                | PlanStatus::Cancelled
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

    pub fn refresh_ready_steps(&mut self) {
        let ready = self.ready_step_ids().into_iter().collect::<HashSet<_>>();
        for step in &mut self.steps {
            if ready.contains(&step.id) {
                let _ = step.transition_to(StepStatus::Ready);
            }
        }
    }
}

pub fn validate_completion_evidence(
    criteria: &[String],
    evidence: &[CompletionEvidence],
    require_complete: bool,
) -> XduduResult<()> {
    if require_complete && evidence.len() != criteria.len() {
        return Err(XduduError::validation("步骤完成证据必须覆盖全部完成条件。"));
    }
    let mut indexes = HashSet::new();
    for item in evidence {
        if item.criterion_index == 0 || item.criterion_index as usize > criteria.len() {
            return Err(XduduError::validation("步骤完成证据索引超出范围。"));
        }
        if !indexes.insert(item.criterion_index) {
            return Err(XduduError::validation("步骤完成证据索引不能重复。"));
        }
        validate_text("步骤完成证据", &item.evidence, 2048)?;
    }
    Ok(())
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
    for review in &mut sanitized.review_history {
        review.reason = redact_text(&review.reason);
    }
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
        sanitize_attempts(step);
    }
    sanitized
}

pub(crate) fn sanitized_plan_revision(revision: &PlanRevision) -> PlanRevision {
    let mut sanitized = revision.clone();
    sanitized.goal = redact_text(&sanitized.goal);
    sanitized.change_request = sanitized.change_request.as_deref().map(redact_text);
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
        sanitize_attempts(step);
    }
    sanitized
}

fn sanitize_attempts(step: &mut PlanStep) {
    for attempt in &mut step.attempts {
        attempt.summary = attempt.summary.as_deref().map(redact_text);
        attempt.error = attempt.error.as_deref().map(redact_text);
        for evidence in &mut attempt.evidence {
            evidence.evidence = redact_text(&evidence.evidence);
        }
    }
}

pub(crate) fn deserialize_plan_compatible(mut value: Value) -> XduduResult<Plan> {
    let version = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| XduduError::validation("计划缺少 schemaVersion。"))?;
    if version == 1 {
        let object = value
            .as_object_mut()
            .ok_or_else(|| XduduError::validation("计划必须是 JSON 对象。"))?;
        object.insert("schemaVersion".into(), Value::from(PLAN_SCHEMA_VERSION));
        object.insert("revision".into(), Value::from(1));
        object.insert("submittedAt".into(), Value::Null);
        object.insert("reviewHistory".into(), Value::Array(Vec::new()));
    }
    if version <= 2 {
        let object = value
            .as_object_mut()
            .ok_or_else(|| XduduError::validation("计划必须是 JSON 对象。"))?;
        object.insert("schemaVersion".into(), Value::from(PLAN_SCHEMA_VERSION));
        object.insert("executionVersion".into(), Value::from(0));
        object.insert("currentStepId".into(), Value::Null);
        object.insert("startedAt".into(), Value::Null);
        object.insert("pausedReason".into(), Value::Null);
    } else if version != u64::from(PLAN_SCHEMA_VERSION) {
        return Err(XduduError::validation(format!(
            "不支持的计划 Schema 版本：{version}。"
        )));
    }
    let plan: Plan = serde_json::from_value(value)?;
    plan.validate()?;
    Ok(plan)
}

#[async_trait]
pub trait PlanStore: Send + Sync {
    async fn create_plan(&self, plan: &Plan) -> XduduResult<()>;
    async fn update_plan(&self, plan: &Plan) -> XduduResult<()>;
    async fn get_plan(&self, plan_id: Uuid) -> XduduResult<Option<Plan>>;
    async fn latest_plan_for_session(&self, session_id: Uuid) -> XduduResult<Option<Plan>>;
    async fn list_plans(&self, limit: usize) -> XduduResult<Vec<Plan>>;
    async fn update_plan_if_current(
        &self,
        plan: &Plan,
        expected_revision: u32,
        expected_status: PlanStatus,
    ) -> XduduResult<bool>;
    async fn append_revision_if_current(
        &self,
        plan: &Plan,
        revision: &PlanRevision,
        expected_revision: u32,
        expected_status: PlanStatus,
    ) -> XduduResult<bool>;
    async fn list_plan_revisions(&self, plan_id: Uuid) -> XduduResult<Vec<PlanRevision>>;
    async fn checkpoint_plan_execution(
        &self,
        plan: &Plan,
        session: &crate::Session,
        expected_execution_version: u64,
        expected_status: PlanStatus,
    ) -> XduduResult<bool>;
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

    #[test]
    fn 待审批计划可以记录批准或拒绝且绑定当前修订() {
        let mut approved = dependency_plan();
        approved.transition_to(PlanStatus::PendingApproval).unwrap();
        assert!(approved.submitted_at.is_some());
        approved
            .add_review(PlanReviewDecision::Approved, "整体方案可行")
            .unwrap();
        approved.transition_to(PlanStatus::Approved).unwrap();
        assert_eq!(approved.review_history[0].revision, 1);
        assert_eq!(
            approved.review_history[0].decision,
            PlanReviewDecision::Approved
        );
        assert!(
            approved
                .replace_for_revision("新目标".into(), vec![], "修改".into())
                .is_err()
        );

        let mut rejected = dependency_plan();
        rejected.transition_to(PlanStatus::PendingApproval).unwrap();
        rejected
            .add_review(PlanReviewDecision::Rejected, "暂不实施")
            .unwrap();
        rejected.transition_to(PlanStatus::Rejected).unwrap();
        assert!(rejected.completed_at.is_some());
    }

    #[test]
    fn 请求修改保留计划身份并生成新步骤标识() {
        let mut plan = dependency_plan();
        let id = plan.id;
        let session_id = plan.session_id;
        let created_at = plan.created_at;
        let old_step = plan.steps[0].id;
        plan.transition_to(PlanStatus::PendingApproval).unwrap();
        let new_step = PlanStep::new("重新检查", "按反馈重新检查")
            .with_completion_criteria(["检查完成".to_owned()]);
        plan.replace_for_revision("完成可靠修改".into(), vec![new_step], "减少步骤".into())
            .unwrap();
        assert_eq!(plan.id, id);
        assert_eq!(plan.session_id, session_id);
        assert_eq!(plan.created_at, created_at);
        assert_eq!(plan.revision, 2);
        assert_ne!(plan.steps[0].id, old_step);
        assert_eq!(plan.status, PlanStatus::PendingApproval);
        assert_eq!(
            plan.review_history[0].decision,
            PlanReviewDecision::ChangesRequested
        );
    }

    #[test]
    fn schema_v1_计划可以兼容读取为_revision_1() {
        let plan = dependency_plan();
        let mut value = serde_json::to_value(plan).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("schemaVersion".into(), Value::from(1));
        object.remove("revision");
        object.remove("submittedAt");
        object.remove("reviewHistory");
        let migrated = deserialize_plan_compatible(value).unwrap();
        assert_eq!(migrated.schema_version, PLAN_SCHEMA_VERSION);
        assert_eq!(migrated.revision, 1);
        assert!(migrated.review_history.is_empty());
    }
}
