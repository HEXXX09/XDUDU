//! XDUDU 核心运行时。
//!
//! 本 crate 不依赖具体终端界面，负责 Agent 循环、模型适配、权限控制、
//! 工具执行和会话持久化，便于 CLI、桌面端或服务端复用同一套行为。

pub mod agent;
pub mod approval;
pub mod changes;
pub mod config;
pub mod credentials;
pub mod error;
pub mod events;
pub mod permission;
pub mod plan;
pub mod plan_executor;
pub mod plan_generation;
pub mod plan_review;
pub mod prompt;
pub mod provider;
pub mod redaction;
pub mod session;
pub mod sqlite_session;
pub mod tools;

pub use agent::{AgentRunConfig, AgentRunResult, run_agent};
pub use approval::{
    AllowAllApprovalGate, ApprovalDecision, ApprovalGate, ApprovalMode, ApprovalRecord,
    ApprovalRequest, ApprovalRule, ApprovalScope, DenyAllApprovalGate, JsonApprovalRuleStore,
    SideEffectKind,
};
pub use changes::{
    ChangeLedger, ChangeSetDraft, ChangeSetFileDraft, ChangeSetRecord, ChangeSetStatus,
    FileChangeDraft, FileChangeRecord, FileChangeStatus, FileOperation, JsonChangeLedger,
    NoopChangeLedger, UndoResult,
};
pub use config::{
    AppConfig, ConfigOverrides, ConfigSource, ResolvedConfig, approval_rules_path, config_paths,
    load_config, write_config_value,
};
pub use credentials::{
    KeyringSecretStore, SecretSource, SecretStore, SecretString, resolve_secret,
};
pub use error::{ErrorKind, XduduError, XduduResult};
pub use events::{AgentEvent, EventSink, NoopEventSink};
pub use permission::{PermissionLevel, PermissionMode};
pub use plan::{
    CompletionEvidence, PLAN_SCHEMA_VERSION, Plan, PlanReviewDecision, PlanReviewRecord,
    PlanRevision, PlanStatus, PlanStep, PlanStepAttempt, PlanStore, StepAttemptStatus, StepStatus,
    validate_completion_evidence,
};
pub use plan_executor::{PlanExecutionResult, PlanExecutorConfig, run_plan};
pub use plan_generation::{
    PlanGenerationConfig, PlanGenerationResult, build_planning_prompt, generate_plan,
};
pub use plan_review::{
    PlanRevisionConfig, PlanRevisionResult, approve_plan, build_revision_prompt, reject_plan,
    revise_plan, submit_plan_for_review,
};
pub use provider::{
    AnthropicProvider, DeepSeekProvider, DefaultProviderFactory, Provider, ProviderFactory,
};
pub use redaction::{redact_text, redact_value};
pub use session::{
    AgentLoopState, JsonSessionStore, Message, Session, SessionStatus, SessionStore,
    ToolCallRecord, ToolCallStatus,
};
pub use sqlite_session::{SqliteSessionStore, WorkspaceLock};
pub use tools::{ToolRegistry, register_builtins};
