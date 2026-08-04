//! XDUDU Rust 命令行入口。

mod approval_prompt;
mod doctor;
mod input_editor;
mod markdown;
mod renderer;
mod tui;
mod ui;

use std::io::Write as _;
use std::{
    collections::BTreeSet,
    env,
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

use async_trait::async_trait;
use clap::{Args, Parser, Subcommand};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xdudu_core::{
    AgentLoopState, AgentRunConfig, AgentRunResult, AllowAllApprovalGate, ApprovalDecision,
    ApprovalGate, ApprovalMode, ApprovalRequest, ApprovalRule, ApprovalScope, ConfigOverrides,
    DefaultProviderFactory, DenyAllApprovalGate, EventSink, JsonApprovalRuleStore,
    JsonChangeLedger, KeyringSecretStore, McpConfigFile, McpServerConfig, McpServerRuntime,
    McpTransportKind, PermissionMode, Plan, PlanExecutorConfig, PlanGenerationConfig,
    PlanRevisionConfig, PlanStatus, PlanStore, PluginManifest, Provider, ProviderFactory,
    ResolvedConfig, SecretSource, SecretStore, SecretString, Session, SessionStatus, SessionStore,
    SqliteSessionStore, ToolRegistry, WorkspaceLock, XduduError, approval_rules_path, approve_plan,
    config_paths, generate_plan, load_config, load_mcp_config, load_plugin_manifests,
    mcp_config_path, plugin_directory, redact_text, register_builtins,
    register_configured_mcp_tools, reject_plan, resolve_secret, revise_plan, run_agent, run_plan,
    save_mcp_config, save_plugin_manifest, submit_plan_for_review, write_config_value,
};

use crate::approval_prompt::{ApprovalMenuChoice, format_approval_prompt, read_approval_menu};
use crate::doctor::run_doctor;
use crate::input_editor::{InputEditor, ReadResult};
use crate::renderer::ConsoleRenderer;
use crate::tui::{
    InputOutcome, PlanRecoveryChoice, PlanReviewChoice, SessionChoice, TuiApp, TuiContext,
};
use crate::ui::TerminalTheme;

#[derive(Debug, Parser)]
#[command(name = "xdudu", version, about = "终端原生 AI 编程助手")]
struct Cli {
    /// 自然语言指令；省略时进入交互模式，管道输入则作为一次性指令。
    prompt: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,

    /// 模型名称；默认值由分层配置决定。
    #[arg(long, global = true)]
    model: Option<String>,

    /// Provider：anthropic 或 deepseek。
    #[arg(long, global = true)]
    provider: Option<String>,

    /// 自定义 Provider Base URL。
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// 单次任务最大 Agent 循环次数。
    #[arg(long, global = true, value_parser = clap::value_parser!(u32).range(1..=100))]
    max_turns: Option<u32>,

    /// 强制进入交互模式。
    #[arg(short, long, global = true)]
    interactive: bool,

    /// 权限模式：read-only、auto-safe 或 full-access。
    #[arg(long, global = true)]
    permission: Option<String>,

    /// 副作用审批模式：ask、never 或 always。
    #[arg(long, global = true)]
    approval: Option<String>,

    /// 继续已有会话。
    #[arg(long, global = true)]
    session: Option<Uuid>,

    /// 以 JSON Lines 输出机器可读事件。
    #[arg(long, global = true)]
    json: bool,

    /// 禁用流式终端渲染。
    #[arg(long, global = true)]
    no_stream: bool,

    /// 禁用颜色。
    #[arg(long, global = true)]
    no_color: bool,

    /// 输出不含模型思维链、且经过脱敏的结构化运行时调试轨迹。
    #[arg(long, global = true)]
    debug_trace: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 执行一次任务；省略 prompt 时从 stdin 读取或进入交互模式。
    Run(RunArgs),
    /// 管理系统凭据中的 Provider API Key。
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// 查看、解释或修改分层配置。
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// 查看或撤销永久工具审批规则。
    Approval {
        #[command(subcommand)]
        command: ApprovalCommand,
    },
    /// 检查安装、配置、凭据和工作区状态。
    Doctor,
    /// 安全撤销最近一次或指定的 Agent 文件变更。
    Undo(UndoArgs),
    /// 查询或恢复本地会话。
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// 创建、审阅、执行和恢复计划。
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// 管理 Model Context Protocol 服务器。
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// 管理只声明 MCP Server 的隔离插件。
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    /// 列出本地插件清单。
    List,
    /// 显示插件的脱敏清单。
    Show { id: String },
    /// 启用插件。
    Enable { id: String },
    /// 禁用插件。
    Disable { id: String },
    /// 校验插件并连接其中的 MCP Server。
    Doctor { id: Option<String> },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// 列出已配置的 MCP Server。
    List,
    /// 显示一个 MCP Server 的脱敏配置。
    Show { name: String },
    /// 添加本地 stdio MCP Server；额外参数直接放在命令之后。
    AddStdio {
        name: String,
        command: String,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// 添加 Streamable HTTP MCP Server。
    AddHttp {
        name: String,
        url: String,
        /// 使用系统凭据 mcp:<name> 作为 Bearer Token。
        #[arg(long)]
        auth: bool,
    },
    /// 启用 MCP Server。
    Enable { name: String },
    /// 禁用 MCP Server。
    Disable { name: String },
    /// 删除 MCP Server 配置（不会删除系统凭据）。
    Remove { name: String },
    /// 把远程 MCP Bearer Token 保存到系统凭据。
    Login { name: String },
    /// 删除远程 MCP Bearer Token。
    Logout { name: String },
    /// 启动并检查 Server、协议和工具列表。
    Doctor { name: Option<String> },
}

#[derive(Debug, Args)]
struct RunArgs {
    /// 要执行的自然语言任务。
    prompt: Option<String>,
}

#[derive(Debug, Args)]
struct UndoArgs {
    /// 指定变更记录 ID；省略时撤销符合条件的最近一次变更。
    #[arg(long)]
    change: Option<Uuid>,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// 按更新时间列出会话。
    List {
        /// 最多显示的会话数量。
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=200))]
        limit: u16,
    },
    /// 显示一个会话的完整记录。
    Show { id: Uuid },
    /// 恢复一个会话；省略指令时进入交互模式。
    Resume { id: Uuid, prompt: Option<String> },
}

#[derive(Debug, Subcommand)]
enum PlanCommand {
    Create {
        goal: String,
    },
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Show {
        id: Uuid,
    },
    Revisions {
        id: Uuid,
    },
    Approve {
        id: Uuid,
        #[arg(long, default_value = "用户通过命令行批准计划。")]
        reason: String,
    },
    Reject {
        id: Uuid,
        #[arg(long, default_value = "用户通过命令行拒绝计划。")]
        reason: String,
    },
    Revise {
        id: Uuid,
        request: String,
    },
    Run {
        id: Uuid,
    },
    Retry {
        id: Uuid,
    },
    Cancel {
        id: Uuid,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// 通过隐藏输入把 API Key 保存到系统凭据存储。
    Login { provider: Option<String> },
    /// 查看环境变量或系统凭据是否已配置。
    Status { provider: Option<String> },
    /// 从系统凭据存储删除 API Key。
    Logout { provider: Option<String> },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// 显示脱敏后的最终配置和每项来源。
    Show,
    /// 显示某项配置的最终值与来源。
    Explain { key: String },
    /// 写入非秘密配置项。
    Set {
        key: String,
        value: String,
        /// 写入用户配置；默认写入当前项目配置。
        #[arg(long, conflicts_with = "project")]
        user: bool,
        /// 明确写入当前项目配置。
        #[arg(long)]
        project: bool,
    },
    /// 显示用户配置与项目配置路径。
    Path,
}

#[derive(Debug, Subcommand)]
enum ApprovalCommand {
    /// 列出永久允许的同类工具。
    List,
    /// 按工具名撤销永久审批规则。
    Revoke { tool: String },
    /// 清除全部永久审批规则。
    Clear,
}

struct Runtime {
    provider: Box<dyn Provider>,
    provider_display: String,
    model: String,
    max_turns: u32,
    cwd: PathBuf,
    permission_mode: PermissionMode,
    approval_mode: ApprovalMode,
    registry: ToolRegistry,
    store: SqliteSessionStore,
    renderer: ConsoleRenderer,
    stream: bool,
    color: bool,
    debug_trace: bool,
    startup_notices: Vec<String>,
}

fn overrides(cli: &Cli) -> ConfigOverrides {
    ConfigOverrides {
        provider: cli.provider.clone(),
        model: cli.model.clone(),
        base_url: cli.base_url.clone(),
        max_turns: cli.max_turns,
        permission: cli.permission.clone(),
        approval: cli.approval.clone(),
        json: cli.json.then_some(true),
        no_stream: cli.no_stream.then_some(true),
        color: cli.no_color.then_some(false),
        debug_trace: cli.debug_trace.then_some(true),
    }
}

fn provider_label(name: &str) -> String {
    match name {
        "anthropic" => "Anthropic",
        "deepseek" => "DeepSeek",
        other => other,
    }
    .to_owned()
}

#[derive(Debug)]
struct ConsoleApprovalGate {
    can_prompt: bool,
    fullscreen: bool,
    theme: TerminalTheme,
    session_rules: tokio::sync::Mutex<BTreeSet<(Uuid, ApprovalRule)>>,
    persistent_rules: JsonApprovalRuleStore,
}

impl ConsoleApprovalGate {
    fn new(
        can_prompt: bool,
        persistent_rules: JsonApprovalRuleStore,
        theme: TerminalTheme,
        fullscreen: bool,
    ) -> Self {
        Self {
            can_prompt,
            fullscreen,
            theme,
            session_rules: tokio::sync::Mutex::new(BTreeSet::new()),
            persistent_rules,
        }
    }
}

#[async_trait]
impl ApprovalGate for ConsoleApprovalGate {
    async fn review(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let rule = ApprovalRule::from_request(request);
        if self.persistent_rules.contains(&rule).await {
            return ApprovalDecision::approve_with_scope(
                "命中用户永久审批规则。",
                ApprovalScope::Always,
            );
        }
        if self
            .session_rules
            .lock()
            .await
            .contains(&(request.session_id, rule.clone()))
        {
            return ApprovalDecision::approve_with_scope(
                "命中当前会话审批规则。",
                ApprovalScope::Session,
            );
        }
        if !self.can_prompt {
            return ApprovalDecision::deny("当前运行方式无法交互审批，且没有匹配的永久审批规则。");
        }
        let theme = self.theme;
        let fullscreen = self.fullscreen;
        let prompt = format_approval_prompt(theme, request);
        let choice = match tokio::task::spawn_blocking(move || {
            read_approval_menu(theme, &prompt, fullscreen)
        })
        .await
        {
            Ok(Ok(choice)) => Some(choice),
            _ => None,
        };
        match choice {
            Some(ApprovalMenuChoice::Once) => {
                ApprovalDecision::approve_with_scope("用户批准当前工具调用。", ApprovalScope::Once)
            }
            Some(ApprovalMenuChoice::Session) => {
                self.session_rules
                    .lock()
                    .await
                    .insert((request.session_id, rule));
                ApprovalDecision::approve_with_scope(
                    "用户批准本会话中的同类工具调用。",
                    ApprovalScope::Session,
                )
            }
            Some(ApprovalMenuChoice::Always) => match self.persistent_rules.allow(rule).await {
                Ok(()) => ApprovalDecision::approve_with_scope(
                    "用户永久批准同类工具调用。",
                    ApprovalScope::Always,
                ),
                Err(error) => ApprovalDecision::deny(format!(
                    "无法保存永久审批规则，本次未执行：{}",
                    error.message
                )),
            },
            Some(ApprovalMenuChoice::Deny) => ApprovalDecision::deny("用户拒绝或未明确批准。"),
            None => ApprovalDecision::deny("审批界面已关闭或无法读取审批输入。"),
        }
    }
}

async fn create_runtime(
    cwd: PathBuf,
    resolved: &ResolvedConfig,
    interactive: bool,
) -> Result<Runtime, XduduError> {
    let change_ledger = Arc::new(JsonChangeLedger::new(&cwd));
    change_ledger.recover_incomplete().await?;
    let store = KeyringSecretStore;
    let (secret, _) = resolve_secret(&resolved.config.provider.name, &store).await?;
    let provider = DefaultProviderFactory.create(&resolved.config.provider, secret)?;
    let approval_mode = resolved.config.agent.approval_mode()?;
    let color = resolved.config.output.color && io::stdout().is_terminal();
    let theme = TerminalTheme::new(color);
    let rich_terminal = interactive
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && env::var("TERM").is_ok_and(|term| term != "dumb");
    let approval_gate: Arc<dyn ApprovalGate> = match approval_mode {
        ApprovalMode::Always => Arc::new(AllowAllApprovalGate),
        ApprovalMode::Never => Arc::new(DenyAllApprovalGate),
        ApprovalMode::Ask => Arc::new(ConsoleApprovalGate::new(
            interactive && !resolved.config.output.json,
            JsonApprovalRuleStore::open(approval_rules_path()?).await?,
            theme,
            rich_terminal,
        )),
    };
    let mut registry = ToolRegistry::with_runtime(approval_gate, change_ledger);
    register_builtins(&mut registry)?;
    let mcp = register_configured_mcp_tools(&mut registry).await?;
    Ok(Runtime {
        provider,
        provider_display: provider_label(&resolved.config.provider.name),
        model: resolved.config.provider.model.clone(),
        max_turns: resolved.config.agent.max_turns,
        cwd: cwd.clone(),
        permission_mode: resolved.config.agent.permission_mode()?,
        approval_mode,
        registry,
        store: SqliteSessionStore::new(&cwd)?,
        renderer: ConsoleRenderer::new(
            resolved.config.output.json,
            !resolved.config.output.no_stream,
            color,
            resolved.config.output.debug_trace,
        ),
        stream: !resolved.config.output.no_stream,
        color,
        debug_trace: resolved.config.output.debug_trace,
        startup_notices: mcp.failures,
    })
}

async fn execute_prompt(
    runtime: &Runtime,
    prompt: String,
    session_id: Option<Uuid>,
) -> Result<AgentRunResult, XduduError> {
    runtime.renderer.begin_run();
    execute_prompt_with_sink(runtime, prompt, session_id, &runtime.renderer, true).await
}

async fn execute_prompt_with_sink(
    runtime: &Runtime,
    prompt: String,
    session_id: Option<Uuid>,
    event_sink: &dyn EventSink,
    print_interrupt: bool,
) -> Result<AgentRunResult, XduduError> {
    let cancellation = CancellationToken::new();
    let run = execute_prompt_with_cancellation(
        runtime,
        prompt,
        session_id,
        event_sink,
        cancellation.clone(),
    );
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => result,
        signal = tokio::signal::ctrl_c() => {
            if signal.is_ok() {
                if print_interrupt {
                    eprintln!("\n  ⏸  已中断，正在保存...");
                }
                cancellation.cancel();
            }
            run.await
        }
    }
}

async fn execute_prompt_with_cancellation(
    runtime: &Runtime,
    prompt: String,
    session_id: Option<Uuid>,
    event_sink: &dyn EventSink,
    cancellation: CancellationToken,
) -> Result<AgentRunResult, XduduError> {
    run_agent(AgentRunConfig {
        prompt,
        model: runtime.model.clone(),
        max_turns: runtime.max_turns,
        cwd: runtime.cwd.clone(),
        provider: runtime.provider.as_ref(),
        tool_registry: &runtime.registry,
        session_store: &runtime.store,
        permission_mode: runtime.permission_mode,
        cancellation,
        session_id,
        event_sink: Some(event_sink),
        stream: runtime.stream,
    })
    .await
}

fn print_banner(runtime: &Runtime, interactive: bool) {
    if !interactive {
        println!(
            "{}",
            ui::compact_banner(
                TerminalTheme::new(runtime.color),
                env!("CARGO_PKG_VERSION"),
                &runtime.provider_display,
                &runtime.model,
            )
        );
    }
}

async fn interactive_loop(
    runtime: Runtime,
    initial_prompt: Option<String>,
    initial_session: Option<Uuid>,
) -> Result<u8, XduduError> {
    if interactive_terminal() {
        tui_interactive_loop(runtime, initial_prompt, initial_session).await
    } else {
        plain_interactive_loop(runtime, initial_prompt, initial_session).await
    }
}

fn interactive_terminal() -> bool {
    io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && env::var("TERM").is_ok_and(|term| term != "dumb")
}

async fn tui_interactive_loop(
    mut runtime: Runtime,
    initial_prompt: Option<String>,
    initial_session: Option<Uuid>,
) -> Result<u8, XduduError> {
    let available_tools = runtime
        .registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name.to_owned())
        .collect();
    let context = TuiContext {
        provider: runtime.provider_display.clone(),
        model: runtime.model.clone(),
        cwd: runtime.cwd.clone(),
        permission: runtime.permission_mode.as_str().to_owned(),
        approval: runtime.approval_mode.as_str().to_owned(),
        available_tools,
        skills: Vec::new(),
        color: runtime.color,
        debug_trace: runtime.debug_trace,
    };
    let (app, _screen) = TuiApp::enter(context).map_err(XduduError::from)?;
    let renderer = app.renderer();
    let mut session_id = initial_session;

    for notice in &runtime.startup_notices {
        app.notice(notice).map_err(XduduError::from)?;
    }

    if let Some(id) = session_id {
        let session = session_for_resume(&runtime, id).await?;
        app.load_session(&session).map_err(XduduError::from)?;
        if let Some(plan) = pending_plan_for_session(&runtime, Some(id)).await? {
            review_plan_in_tui(&runtime, &app, plan).await?;
        } else if let Some(plan) = paused_plan_for_session(&runtime, Some(id)).await? {
            recover_plan_in_tui(&runtime, &app, plan).await?;
        }
    }
    if let Some(prompt) = initial_prompt {
        app.begin_prompt(&prompt).map_err(XduduError::from)?;
        let result =
            execute_prompt_with_sink(&runtime, prompt, session_id, &renderer, false).await?;
        app.finish_prompt(&result).map_err(XduduError::from)?;
        session_id = Some(result.session_id);
    }

    loop {
        match app.read_input().map_err(XduduError::from)? {
            InputOutcome::Exit => break,
            InputOutcome::Interrupted => {
                app.notice("再次按 Ctrl+D 或输入 /exit 可退出。")
                    .map_err(XduduError::from)?;
            }
            InputOutcome::Submit(prompt) => {
                app.begin_prompt(&prompt).map_err(XduduError::from)?;
                let result =
                    execute_prompt_with_sink(&runtime, prompt, session_id, &renderer, false)
                        .await?;
                app.finish_prompt(&result).map_err(XduduError::from)?;
                session_id = Some(result.session_id);
            }
            InputOutcome::Command(command) => {
                let input = command.trim();
                match input {
                    "/exit" | "/quit" | "/q" => break,
                    "/help" | "/h" => {
                        app.notice(
                            "/new  新会话  ·  /resume  恢复会话  ·  /plan  生成/审阅计划  ·  /model  选择模型  ·  /mcp  外部工具  ·  /plugins  插件  ·  /turns N  最大循环次数  ·  /exit  退出",
                        )
                        .map_err(XduduError::from)?;
                    }
                    "/new" => {
                        session_id = None;
                        app.notice("已开始新会话。").map_err(XduduError::from)?;
                    }
                    "/model" => {
                        if let Some(model) = app.select_model().map_err(XduduError::from)? {
                            runtime.model = model;
                            let saved = write_config_value(
                                &runtime.cwd,
                                true,
                                "provider.model",
                                &runtime.model,
                            );
                            app.set_model(&runtime.model).map_err(XduduError::from)?;
                            app.notice(format!(
                                "已切换到 {}{}",
                                ui::model_display_name(&runtime.provider_display, &runtime.model),
                                if saved.is_ok() {
                                    "，并保存为默认模型。"
                                } else {
                                    "（仅当前会话，默认配置保存失败）。"
                                }
                            ))
                            .map_err(XduduError::from)?;
                        }
                    }
                    "/mcp" => {
                        let config = load_mcp_config()?;
                        let external_tools = runtime
                            .registry
                            .definitions()
                            .into_iter()
                            .filter(|definition| definition.name.starts_with("mcp__"))
                            .count();
                        let servers = if config.servers.is_empty() {
                            "尚未配置 Server".to_owned()
                        } else {
                            config
                                .servers
                                .iter()
                                .map(|server| {
                                    format!(
                                        "{} [{}]",
                                        server.name,
                                        if server.enabled {
                                            "enabled"
                                        } else {
                                            "disabled"
                                        }
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("、")
                        };
                        app.notice(format!(
                            "MCP：{servers}\n已加载外部工具：{external_tools}\n管理命令：xdudu mcp --help"
                        ))
                        .map_err(XduduError::from)?;
                    }
                    "/plugins" => {
                        let plugins = load_plugin_manifests()?;
                        let summary = if plugins.is_empty() {
                            "尚未安装插件".to_owned()
                        } else {
                            plugins
                                .iter()
                                .map(|plugin| {
                                    format!(
                                        "{} [{}] · {} MCP servers",
                                        plugin.id,
                                        if plugin.enabled {
                                            "enabled"
                                        } else {
                                            "disabled"
                                        },
                                        plugin.mcp_servers.len()
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        };
                        app.notice(format!("插件：\n{summary}\n管理命令：xdudu plugin --help"))
                            .map_err(XduduError::from)?;
                    }
                    "/resume" => {
                        let sessions = runtime.store.list(20).await?;
                        if sessions.is_empty() {
                            app.notice("当前工作区还没有可恢复的历史会话。")
                                .map_err(XduduError::from)?;
                            continue;
                        }
                        let choices = sessions.iter().map(session_choice).collect();
                        if let Some(id) = app.select_session(choices).map_err(XduduError::from)? {
                            let session = session_for_resume(&runtime, id).await?;
                            app.load_session(&session).map_err(XduduError::from)?;
                            session_id = Some(id);
                            if let Some(plan) =
                                pending_plan_for_session(&runtime, session_id).await?
                            {
                                review_plan_in_tui(&runtime, &app, plan).await?;
                            } else if let Some(plan) =
                                paused_plan_for_session(&runtime, session_id).await?
                            {
                                recover_plan_in_tui(&runtime, &app, plan).await?;
                            }
                        }
                    }
                    "/plan" => {
                        if let Some(plan) = pending_plan_for_session(&runtime, session_id).await? {
                            review_plan_in_tui(&runtime, &app, plan).await?;
                        } else if let Some(plan) =
                            paused_plan_for_session(&runtime, session_id).await?
                        {
                            recover_plan_in_tui(&runtime, &app, plan).await?;
                        } else if let Some(id) = session_id
                            && let Some(plan) = runtime.store.latest_plan_for_session(id).await?
                        {
                            app.notice(plan_summary(&plan)).map_err(XduduError::from)?;
                        } else {
                            app.notice("用法：/plan <目标>").map_err(XduduError::from)?;
                        }
                    }
                    "/plan status" => {
                        if let Some(id) = session_id
                            && let Some(plan) = runtime.store.latest_plan_for_session(id).await?
                        {
                            app.notice(plan_summary(&plan)).map_err(XduduError::from)?;
                        } else {
                            app.notice("当前会话没有计划。").map_err(XduduError::from)?;
                        }
                    }
                    "/plan run" | "/plan retry" => {
                        let Some(id) = session_id else {
                            app.notice("当前没有活动会话。").map_err(XduduError::from)?;
                            continue;
                        };
                        let Some(plan) = runtime.store.latest_plan_for_session(id).await? else {
                            app.notice("当前会话没有计划。").map_err(XduduError::from)?;
                            continue;
                        };
                        if input == "/plan run" && plan.status != PlanStatus::Approved {
                            app.notice("/plan run 只执行已批准计划；暂停计划请使用 /plan retry。")
                                .map_err(XduduError::from)?;
                            continue;
                        }
                        if input == "/plan retry" && plan.status != PlanStatus::Paused {
                            app.notice("/plan retry 只重试暂停计划。")
                                .map_err(XduduError::from)?;
                            continue;
                        }
                        let result = execute_plan_with_sink(&runtime, plan.id, &renderer).await?;
                        app.notice(result.message).map_err(XduduError::from)?;
                    }
                    "/plan revisions" => {
                        let Some(id) = session_id else {
                            app.notice("当前没有活动会话。").map_err(XduduError::from)?;
                            continue;
                        };
                        let Some(plan) = runtime.store.latest_plan_for_session(id).await? else {
                            app.notice("当前会话没有计划。").map_err(XduduError::from)?;
                            continue;
                        };
                        let revisions = runtime.store.list_plan_revisions(plan.id).await?;
                        app.notice(format!(
                            "计划修订：{}",
                            revisions
                                .iter()
                                .map(|item| item.revision.to_string())
                                .collect::<Vec<_>>()
                                .join("、")
                        ))
                        .map_err(XduduError::from)?;
                    }
                    "/plan cancel" => {
                        app.notice("取消不会撤销既有副作用。请输入 YES 确认。")
                            .map_err(XduduError::from)?;
                        let confirmed = matches!(
                            app.read_input().map_err(XduduError::from)?,
                            InputOutcome::Submit(value) if value.trim() == "YES"
                        );
                        if !confirmed {
                            app.notice("已保留计划。").map_err(XduduError::from)?;
                            continue;
                        }
                        let Some(id) = session_id else {
                            app.notice("当前没有活动会话。").map_err(XduduError::from)?;
                            continue;
                        };
                        let Some(plan) = runtime.store.latest_plan_for_session(id).await? else {
                            app.notice("当前会话没有计划。").map_err(XduduError::from)?;
                            continue;
                        };
                        cancel_plan(&runtime, plan.id).await?;
                        app.notice("计划已取消；既有副作用未撤销。")
                            .map_err(XduduError::from)?;
                    }
                    _ => {
                        if let Some(raw_id) = input
                            .strip_prefix("/resume ")
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                        {
                            match Uuid::parse_str(raw_id) {
                                Ok(id) => match session_for_resume(&runtime, id).await {
                                    Ok(session) => {
                                        app.load_session(&session).map_err(XduduError::from)?;
                                        session_id = Some(id);
                                        if let Some(plan) =
                                            pending_plan_for_session(&runtime, session_id).await?
                                        {
                                            review_plan_in_tui(&runtime, &app, plan).await?;
                                        } else if let Some(plan) =
                                            paused_plan_for_session(&runtime, session_id).await?
                                        {
                                            recover_plan_in_tui(&runtime, &app, plan).await?;
                                        }
                                    }
                                    Err(error) => {
                                        app.notice(error.message).map_err(XduduError::from)?;
                                    }
                                },
                                Err(_) => {
                                    app.notice("会话 ID 必须是完整 UUID。")
                                        .map_err(XduduError::from)?;
                                }
                            }
                        } else if let Some(goal) = input
                            .strip_prefix("/plan new ")
                            .or_else(|| input.strip_prefix("/plan "))
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                        {
                            app.notice("正在生成结构化计划…")
                                .map_err(XduduError::from)?;
                            match create_plan_for_review(&runtime, goal.to_owned(), session_id)
                                .await
                            {
                                Ok((session, plan)) => {
                                    session_id = Some(session.id);
                                    review_plan_in_tui(&runtime, &app, plan).await?;
                                }
                                Err(error) => {
                                    app.notice(error.message).map_err(XduduError::from)?;
                                }
                            }
                        } else if let Some(model) = input
                            .strip_prefix("/model ")
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                        {
                            runtime.model = model.to_owned();
                            let saved = write_config_value(
                                &runtime.cwd,
                                true,
                                "provider.model",
                                &runtime.model,
                            );
                            app.set_model(&runtime.model).map_err(XduduError::from)?;
                            app.notice(if saved.is_ok() {
                                format!("已切换到 {}，并保存为默认模型。", runtime.model)
                            } else {
                                format!("已切换到 {}，但默认配置保存失败。", runtime.model)
                            })
                            .map_err(XduduError::from)?;
                        } else if let Some(turns) = input.strip_prefix("/turns ") {
                            match turns.trim().parse::<u32>() {
                                Ok(value) if (1..=100).contains(&value) => {
                                    runtime.max_turns = value;
                                    app.notice(format!("最大循环次数已设为 {value}。"))
                                        .map_err(XduduError::from)?;
                                }
                                _ => app
                                    .notice("最大循环次数必须是 1 到 100 之间的整数。")
                                    .map_err(XduduError::from)?,
                            }
                        } else {
                            app.notice(format!("未知命令：{input}"))
                                .map_err(XduduError::from)?;
                        }
                    }
                }
            }
        }
    }
    Ok(0)
}

async fn session_for_resume(runtime: &Runtime, id: Uuid) -> Result<Session, XduduError> {
    let session = runtime
        .store
        .get(id)
        .await?
        .ok_or_else(|| XduduError::validation(format!("找不到会话：{id}")))?;
    if session.cwd != runtime.cwd {
        return Err(XduduError::validation("不能在不同工作目录中恢复已有会话。"));
    }
    Ok(session)
}

fn session_choice(session: &Session) -> SessionChoice {
    let status = serde_json::to_string(&session.status)
        .unwrap_or_else(|_| "\"unknown\"".into())
        .trim_matches('"')
        .to_owned();
    SessionChoice {
        id: session.id,
        title: session.title.clone(),
        status,
        updated_at: session.updated_at.format("%m-%d %H:%M").to_string(),
    }
}

fn plan_context(session: &Session) -> Option<String> {
    const LIMIT: usize = 65_536;
    let mut parts = Vec::new();
    if !session.context_summary.trim().is_empty() {
        parts.push(format!(
            "较早会话摘要：\n{}",
            redact_text(&session.context_summary)
        ));
    }
    let recent = session
        .messages
        .iter()
        .rev()
        .filter(|message| {
            matches!(
                message.role,
                xdudu_core::provider::MessageRole::User
                    | xdudu_core::provider::MessageRole::Assistant
            )
        })
        .take(24)
        .collect::<Vec<_>>();
    for message in recent.into_iter().rev() {
        let role = if message.role == xdudu_core::provider::MessageRole::User {
            "用户"
        } else {
            "助手"
        };
        parts.push(format!("{role}：{}", redact_text(&message.content)));
    }
    let mut output = String::new();
    for part in parts {
        let separator = if output.is_empty() { "" } else { "\n\n" };
        if output.len() + separator.len() + part.len() > LIMIT {
            break;
        }
        output.push_str(separator);
        output.push_str(&part);
    }
    (!output.is_empty()).then_some(output)
}

fn active_plan(status: PlanStatus) -> bool {
    matches!(
        status,
        PlanStatus::Draft
            | PlanStatus::PendingApproval
            | PlanStatus::Approved
            | PlanStatus::Running
            | PlanStatus::Paused
    )
}

async fn create_plan_for_review(
    runtime: &Runtime,
    goal: String,
    session_id: Option<Uuid>,
) -> Result<(Session, Plan), XduduError> {
    let mut session = if let Some(id) = session_id {
        session_for_resume(runtime, id).await?
    } else {
        Session::new(
            runtime.cwd.clone(),
            runtime.provider.name(),
            runtime.model.clone(),
            goal.clone(),
        )
    };
    if let Some(plan) = runtime.store.latest_plan_for_session(session.id).await?
        && active_plan(plan.status)
    {
        return Err(XduduError::validation(format!(
            "当前会话已有活动计划（revision {}，状态 {:?}），不能创建第二份计划。",
            plan.revision, plan.status
        )));
    }

    let context = plan_context(&session);
    session.status = SessionStatus::Running;
    session.current_state = AgentLoopState::Planning;
    session.provider_name = runtime.provider.name().to_owned();
    session.model.clone_from(&runtime.model);
    session.completed_at = None;
    if session_id.is_some() {
        session.append_user_message(goal.clone());
        runtime.store.update(&session).await?;
    } else {
        runtime.store.create(&session).await?;
    }

    let cancellation = CancellationToken::new();
    let generation = generate_plan(PlanGenerationConfig {
        session_id: session.id,
        goal,
        context,
        model: runtime.model.clone(),
        cwd: runtime.cwd.clone(),
        provider: runtime.provider.as_ref(),
        plan_store: &runtime.store,
        cancellation: cancellation.clone(),
    });
    tokio::pin!(generation);
    let generated = tokio::select! {
        result = &mut generation => result,
        signal = tokio::signal::ctrl_c() => {
            if signal.is_ok() {
                cancellation.cancel();
            }
            generation.await
        }
    };
    let generated = match generated {
        Ok(generated) => generated,
        Err(error) => {
            session.status = SessionStatus::Incomplete;
            session.current_state = AgentLoopState::Incomplete;
            session.touch();
            runtime.store.update(&session).await?;
            return Err(error);
        }
    };
    session.total_input_tokens = session
        .total_input_tokens
        .saturating_add(generated.usage.input_tokens);
    session.total_output_tokens = session
        .total_output_tokens
        .saturating_add(generated.usage.output_tokens);
    let plan = submit_plan_for_review(&runtime.store, generated.plan.id, 1).await?;
    session.status = SessionStatus::WaitingApproval;
    session.current_state = AgentLoopState::WaitingApproval;
    session.touch();
    runtime.store.update(&session).await?;
    Ok((session, plan))
}

async fn pending_plan_for_session(
    runtime: &Runtime,
    session_id: Option<Uuid>,
) -> Result<Option<Plan>, XduduError> {
    let Some(session_id) = session_id else {
        return Ok(None);
    };
    Ok(runtime
        .store
        .latest_plan_for_session(session_id)
        .await?
        .filter(|plan| plan.status == PlanStatus::PendingApproval))
}

async fn paused_plan_for_session(
    runtime: &Runtime,
    session_id: Option<Uuid>,
) -> Result<Option<Plan>, XduduError> {
    let Some(session_id) = session_id else {
        return Ok(None);
    };
    Ok(runtime
        .store
        .latest_plan_for_session(session_id)
        .await?
        .filter(|plan| plan.status == PlanStatus::Paused))
}

async fn sync_plan_session(
    runtime: &Runtime,
    session_id: Uuid,
    status: SessionStatus,
    state: AgentLoopState,
) -> Result<(), XduduError> {
    let mut session = session_for_resume(runtime, session_id).await?;
    session.status = status;
    session.current_state = state;
    session.completed_at = None;
    session.touch();
    runtime.store.update(&session).await
}

async fn sync_plan_session_store(
    store: &SqliteSessionStore,
    session_id: Uuid,
    status: SessionStatus,
    state: AgentLoopState,
) -> Result<(), XduduError> {
    let mut session = store
        .get(session_id)
        .await?
        .ok_or_else(|| XduduError::validation(format!("找不到会话：{session_id}")))?;
    session.status = status;
    session.current_state = state;
    session.completed_at = None;
    session.touch();
    store.update(&session).await
}

async fn review_plan_in_tui(
    runtime: &Runtime,
    app: &TuiApp,
    mut plan: Plan,
) -> Result<(), XduduError> {
    loop {
        match app.review_plan(&plan).map_err(XduduError::from)? {
            None => {
                app.notice("计划仍保持等待审批，可稍后输入 /plan 重新打开。")
                    .map_err(XduduError::from)?;
                return Ok(());
            }
            Some(PlanReviewChoice::Approve) => {
                let approved = approve_plan(
                    &runtime.store,
                    plan.id,
                    plan.revision,
                    "用户在终端审阅界面批准计划。",
                )
                .await?;
                sync_plan_session(
                    runtime,
                    approved.session_id,
                    SessionStatus::PlanReady,
                    AgentLoopState::Completed,
                )
                .await?;
                app.notice("计划已批准。输入 /plan run 开始执行；具体副作用仍会单独审批。")
                    .map_err(XduduError::from)?;
                return Ok(());
            }
            Some(PlanReviewChoice::Reject) => {
                let rejected = reject_plan(
                    &runtime.store,
                    plan.id,
                    plan.revision,
                    "用户在终端审阅界面拒绝计划。",
                )
                .await?;
                sync_plan_session(
                    runtime,
                    rejected.session_id,
                    SessionStatus::Incomplete,
                    AgentLoopState::Incomplete,
                )
                .await?;
                app.notice("计划已拒绝，未执行任何步骤。")
                    .map_err(XduduError::from)?;
                return Ok(());
            }
            Some(PlanReviewChoice::RequestChanges) => {
                app.notice("请输入对计划的修改要求；Ctrl+C 取消并保留当前版本。")
                    .map_err(XduduError::from)?;
                let change_request = match app.read_input().map_err(XduduError::from)? {
                    InputOutcome::Submit(value) | InputOutcome::Command(value)
                        if !value.trim().is_empty() =>
                    {
                        value
                    }
                    _ => {
                        app.notice("已取消修改，原计划继续等待审批。")
                            .map_err(XduduError::from)?;
                        continue;
                    }
                };
                let session = session_for_resume(runtime, plan.session_id).await?;
                sync_plan_session(
                    runtime,
                    plan.session_id,
                    SessionStatus::Running,
                    AgentLoopState::Planning,
                )
                .await?;
                let cancellation = CancellationToken::new();
                let revision = revise_plan(PlanRevisionConfig {
                    plan_id: plan.id,
                    expected_revision: plan.revision,
                    change_request,
                    context: plan_context(&session),
                    model: runtime.model.clone(),
                    cwd: runtime.cwd.clone(),
                    provider: runtime.provider.as_ref(),
                    plan_store: &runtime.store,
                    cancellation: cancellation.clone(),
                });
                tokio::pin!(revision);
                let result = tokio::select! {
                    result = &mut revision => result,
                    signal = tokio::signal::ctrl_c() => {
                        if signal.is_ok() {
                            cancellation.cancel();
                        }
                        revision.await
                    }
                };
                match result {
                    Ok(result) => {
                        plan = result.plan;
                        let mut session = session_for_resume(runtime, plan.session_id).await?;
                        session.total_input_tokens = session
                            .total_input_tokens
                            .saturating_add(result.usage.input_tokens);
                        session.total_output_tokens = session
                            .total_output_tokens
                            .saturating_add(result.usage.output_tokens);
                        session.status = SessionStatus::WaitingApproval;
                        session.current_state = AgentLoopState::WaitingApproval;
                        session.touch();
                        runtime.store.update(&session).await?;
                        app.notice(format!(
                            "计划已修订为 revision {}，请重新审阅。",
                            plan.revision
                        ))
                        .map_err(XduduError::from)?;
                    }
                    Err(error) => {
                        sync_plan_session(
                            runtime,
                            plan.session_id,
                            SessionStatus::WaitingApproval,
                            AgentLoopState::WaitingApproval,
                        )
                        .await?;
                        app.notice(format!("修订失败，原计划保持不变：{}", error.message))
                            .map_err(XduduError::from)?;
                    }
                }
            }
        }
    }
}

async fn review_plan_classic(
    runtime: &Runtime,
    editor: &mut InputEditor,
    mut plan: Plan,
) -> Result<(), XduduError> {
    loop {
        println!("\n{}", plan_summary(&plan));
        for (index, step) in plan.steps.iter().enumerate() {
            println!("  {}. {}", index + 1, step.title);
            if !step.dependencies.is_empty() {
                println!("     依赖 {} 个步骤", step.dependencies.len());
            }
            for criterion in &step.completion_criteria {
                println!("     ✓ {criterion}");
            }
        }
        println!("\n  1 批准计划  ·  2 请求修改  ·  0 拒绝（默认）");
        let choice = match editor.read_line("  选择：").map_err(XduduError::from)? {
            ReadResult::Line(value) => value,
            _ => return Ok(()),
        };
        match choice.trim() {
            "1" => {
                let approved = approve_plan(
                    &runtime.store,
                    plan.id,
                    plan.revision,
                    "用户在经典终端审阅界面批准计划。",
                )
                .await?;
                sync_plan_session(
                    runtime,
                    approved.session_id,
                    SessionStatus::PlanReady,
                    AgentLoopState::Completed,
                )
                .await?;
                println!("  计划已批准。输入 /plan run 开始执行；工具副作用仍会单独审批。");
                return Ok(());
            }
            "2" => {
                let request = match editor.read_line("  修改要求：").map_err(XduduError::from)?
                {
                    ReadResult::Line(value) if !value.trim().is_empty() => value,
                    _ => continue,
                };
                let session = session_for_resume(runtime, plan.session_id).await?;
                sync_plan_session(
                    runtime,
                    plan.session_id,
                    SessionStatus::Running,
                    AgentLoopState::Planning,
                )
                .await?;
                let result = revise_plan(PlanRevisionConfig {
                    plan_id: plan.id,
                    expected_revision: plan.revision,
                    change_request: request,
                    context: plan_context(&session),
                    model: runtime.model.clone(),
                    cwd: runtime.cwd.clone(),
                    provider: runtime.provider.as_ref(),
                    plan_store: &runtime.store,
                    cancellation: CancellationToken::new(),
                })
                .await;
                match result {
                    Ok(result) => {
                        plan = result.plan;
                        sync_plan_session(
                            runtime,
                            plan.session_id,
                            SessionStatus::WaitingApproval,
                            AgentLoopState::WaitingApproval,
                        )
                        .await?;
                        println!("  计划已修订为 revision {}。", plan.revision);
                    }
                    Err(error) => {
                        sync_plan_session(
                            runtime,
                            plan.session_id,
                            SessionStatus::WaitingApproval,
                            AgentLoopState::WaitingApproval,
                        )
                        .await?;
                        println!("  修订失败，原计划保持不变：{}", error.message);
                    }
                }
            }
            _ => {
                let rejected = reject_plan(
                    &runtime.store,
                    plan.id,
                    plan.revision,
                    "用户在经典终端审阅界面拒绝计划。",
                )
                .await?;
                sync_plan_session(
                    runtime,
                    rejected.session_id,
                    SessionStatus::Incomplete,
                    AgentLoopState::Incomplete,
                )
                .await?;
                println!("  计划已拒绝，未执行任何步骤。");
                return Ok(());
            }
        }
    }
}

async fn recover_plan_in_tui(
    runtime: &Runtime,
    app: &TuiApp,
    mut plan: Plan,
) -> Result<(), XduduError> {
    loop {
        match app.recover_plan(&plan).map_err(XduduError::from)? {
            None => {
                app.notice("计划保持暂停，未重放任何工具调用。")
                    .map_err(XduduError::from)?;
                return Ok(());
            }
            Some(PlanRecoveryChoice::ViewDetails) => {
                app.notice(plan_summary(&plan)).map_err(XduduError::from)?;
                plan = runtime
                    .store
                    .get_plan(plan.id)
                    .await?
                    .ok_or_else(|| XduduError::validation("计划已不存在。"))?;
            }
            Some(PlanRecoveryChoice::Continue | PlanRecoveryChoice::Retry) => {
                let result = execute_plan_with_sink(runtime, plan.id, &app.renderer()).await?;
                app.notice(result.message).map_err(XduduError::from)?;
                return Ok(());
            }
            Some(PlanRecoveryChoice::Cancel) => {
                app.notice("取消不会撤销既有副作用。请输入 YES 确认。")
                    .map_err(XduduError::from)?;
                let confirmed = matches!(
                    app.read_input().map_err(XduduError::from)?,
                    InputOutcome::Submit(value) if value.trim() == "YES"
                );
                if confirmed {
                    cancel_plan(runtime, plan.id).await?;
                    app.notice("计划已取消；既有副作用未撤销。")
                        .map_err(XduduError::from)?;
                    return Ok(());
                }
                app.notice("已保留暂停计划。").map_err(XduduError::from)?;
            }
        }
    }
}

async fn execute_plan_with_sink(
    runtime: &Runtime,
    plan_id: Uuid,
    sink: &dyn EventSink,
) -> Result<xdudu_core::PlanExecutionResult, XduduError> {
    let cancellation = CancellationToken::new();
    let execution = run_plan(PlanExecutorConfig {
        plan_id,
        model: runtime.model.clone(),
        cwd: runtime.cwd.clone(),
        max_turns_per_step: runtime.max_turns,
        provider: runtime.provider.as_ref(),
        tool_registry: &runtime.registry,
        session_store: &runtime.store,
        plan_store: &runtime.store,
        permission_mode: runtime.permission_mode,
        cancellation: cancellation.clone(),
        event_sink: Some(sink),
    });
    tokio::pin!(execution);
    tokio::select! {
        result = &mut execution => result,
        signal = tokio::signal::ctrl_c() => {
            if signal.is_ok() {
                cancellation.cancel();
            }
            execution.await
        }
    }
}

fn plan_summary(plan: &Plan) -> String {
    let completed = plan
        .steps
        .iter()
        .filter(|step| step.status == xdudu_core::StepStatus::Completed)
        .count();
    format!(
        "Plan {} · revision {} · {:?} · {}/{} 步完成{}",
        plan.id,
        plan.revision,
        plan.status,
        completed,
        plan.steps.len(),
        plan.paused_reason
            .as_ref()
            .map(|reason| format!(" · {reason}"))
            .unwrap_or_default()
    )
}

async fn cancel_plan(runtime: &Runtime, plan_id: Uuid) -> Result<Plan, XduduError> {
    cancel_plan_with_store(&runtime.store, plan_id).await
}

async fn cancel_plan_with_store(
    store: &SqliteSessionStore,
    plan_id: Uuid,
) -> Result<Plan, XduduError> {
    let mut plan = store
        .get_plan(plan_id)
        .await?
        .ok_or_else(|| XduduError::validation(format!("找不到计划：{plan_id}")))?;
    let expected_status = plan.status;
    if matches!(
        expected_status,
        PlanStatus::Completed | PlanStatus::Failed | PlanStatus::Rejected | PlanStatus::Cancelled
    ) {
        return Err(XduduError::validation("终态计划不能取消。"));
    }
    let mut session = store
        .get(plan.session_id)
        .await?
        .ok_or_else(|| XduduError::validation(format!("找不到计划会话：{}", plan.session_id)))?;
    for step in &mut plan.steps {
        if !matches!(
            step.status,
            xdudu_core::StepStatus::Completed | xdudu_core::StepStatus::Skipped
        ) {
            let _ = step.transition_to(xdudu_core::StepStatus::Cancelled);
        }
    }
    plan.transition_to(PlanStatus::Cancelled)?;
    let expected_version = plan.execution_version;
    plan.execution_version += 1;
    session.status = SessionStatus::Incomplete;
    session.current_state = AgentLoopState::Incomplete;
    if !store
        .checkpoint_plan_execution(&plan, &session, expected_version, expected_status)
        .await?
    {
        return Err(XduduError::validation(
            "PLAN_CONFLICT：计划已被其他请求更新。",
        ));
    }
    Ok(plan)
}

async fn plain_interactive_loop(
    mut runtime: Runtime,
    initial_prompt: Option<String>,
    initial_session: Option<Uuid>,
) -> Result<u8, XduduError> {
    let mut session_id = initial_session;
    let mut editor = InputEditor::with_workspace(&runtime.cwd);
    println!(
        "{}",
        ui::compact_banner(
            TerminalTheme::new(runtime.color),
            env!("CARGO_PKG_VERSION"),
            &runtime.provider_display,
            &runtime.model,
        )
    );
    println!(
        "  {} · {} · 输入 /help 查看命令",
        runtime.permission_mode.as_str(),
        runtime.cwd.display()
    );
    for notice in &runtime.startup_notices {
        eprintln!("  警告：{notice}");
    }
    if let Some(prompt) = initial_prompt {
        let result = execute_prompt(&runtime, prompt, session_id).await?;
        runtime.renderer.finish_run(&result)?;
        session_id = Some(result.session_id);
    }

    loop {
        println!();
        let prompt = ui::prompt(TerminalTheme::new(runtime.color));
        let line = match editor.read_line(&prompt).map_err(XduduError::from)? {
            ReadResult::Line(line) => line,
            ReadResult::Interrupted => {
                println!(
                    "  {}",
                    TerminalTheme::new(runtime.color).muted("已取消当前输入。")
                );
                continue;
            }
            ReadResult::Eof => break,
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        match input {
            "/exit" | "/quit" | "/q" => {
                println!(
                    "  {}",
                    TerminalTheme::new(runtime.color).muted("再见，期待下次一起写代码。")
                );
                break;
            }
            "/help" | "/h" => {
                print!("{}", ui::help(TerminalTheme::new(runtime.color)));
                continue;
            }
            "/new" | "/clear" => {
                session_id = None;
                println!("  已开始新会话。");
                continue;
            }
            "/transcript" => {
                if let Some(id) = session_id {
                    let session = session_for_resume(&runtime, id).await?;
                    println!("\n  会话：{} · {}", session.title, session.id);
                    for message in &session.messages {
                        if message.content.trim().is_empty() {
                            continue;
                        }
                        let role = match message.role {
                            xdudu_core::provider::MessageRole::User => "❯",
                            xdudu_core::provider::MessageRole::Assistant => "┊",
                            xdudu_core::provider::MessageRole::Tool => "⏺",
                            xdudu_core::provider::MessageRole::System => "·",
                        };
                        println!("\n  {role} {}", redact_text(&message.content));
                    }
                } else {
                    println!("  当前没有活动会话。");
                }
                continue;
            }
            "/copy" => {
                if let Some(id) = session_id {
                    let session = session_for_resume(&runtime, id).await?;
                    if let Some(message) = session.messages.iter().rev().find(|message| {
                        message.role == xdudu_core::provider::MessageRole::Assistant
                            && !message.content.trim().is_empty()
                    }) {
                        match copy_text(&redact_text(&message.content)) {
                            Ok(()) => println!("  已复制最后一条助手回答。"),
                            Err(error) => println!("  无法访问系统剪贴板：{error}"),
                        }
                    }
                }
                continue;
            }
            "/export" => {
                if let Some(id) = session_id {
                    let session = session_for_resume(&runtime, id).await?;
                    let path = export_session_markdown(&runtime.cwd, &session)?;
                    println!("  会话已导出：{}", path.display());
                } else {
                    println!("  当前没有活动会话。");
                }
                continue;
            }
            "/compact" => {
                println!("  XDUDU 会在上下文接近上限时自动压缩；手动压缩协议尚未开放。");
                continue;
            }
            "/mcp" => {
                let config = load_mcp_config()?;
                if config.servers.is_empty() {
                    println!("  尚未配置 MCP Server。使用 xdudu mcp --help 查看管理命令。");
                } else {
                    for server in config.servers {
                        println!(
                            "  {} · {} · {:?}",
                            server.name,
                            if server.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            },
                            server.transport
                        );
                    }
                }
                continue;
            }
            "/plugins" => {
                let plugins = load_plugin_manifests()?;
                if plugins.is_empty() {
                    println!("  尚未安装插件。使用 xdudu plugin --help 查看管理命令。");
                } else {
                    for plugin in plugins {
                        println!(
                            "  {} · {} · {} MCP servers",
                            plugin.id,
                            if plugin.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            },
                            plugin.mcp_servers.len()
                        );
                    }
                }
                continue;
            }
            "/model" => {
                let options = ui::model_options(&runtime.provider_display, &runtime.model);
                println!("  当前 Provider 可用模型：");
                for (index, option) in options.iter().enumerate() {
                    let current = if ui::model_matches(&option.id, &runtime.model) {
                        "（当前）"
                    } else {
                        ""
                    };
                    println!(
                        "    {}. {} {}  {}",
                        index + 1,
                        option.label,
                        current,
                        option.description
                    );
                }
                let selection = match editor
                    .read_line("  选择（Enter 保持当前）：")
                    .map_err(XduduError::from)?
                {
                    ReadResult::Line(value) => value,
                    _ => continue,
                };
                if let Some(option) = selection
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| options.get(index.saturating_sub(1)))
                {
                    runtime.model.clone_from(&option.id);
                    let saved =
                        write_config_value(&runtime.cwd, true, "provider.model", &runtime.model);
                    println!(
                        "  已切换到 {}{}",
                        option.label,
                        if saved.is_ok() {
                            "，并保存为默认模型。"
                        } else {
                            "（仅当前会话）。"
                        }
                    );
                }
                continue;
            }
            "/resume" => {
                let sessions = runtime.store.list(30).await?;
                if sessions.is_empty() {
                    println!("  当前工作区还没有历史会话。");
                    continue;
                }
                println!("  最近会话（输入编号、标题关键词或完整 UUID）：");
                for (index, session) in sessions.iter().enumerate() {
                    println!(
                        "  {:>2}. {} · {} · {:?} · {} 条消息",
                        index + 1,
                        session.updated_at.format("%m-%d %H:%M"),
                        session.title,
                        session.status,
                        session.messages.len()
                    );
                }
                let selection = match editor.read_line("  选择：").map_err(XduduError::from)? {
                    ReadResult::Line(value) => value,
                    _ => continue,
                };
                let selected = selection
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| sessions.get(index.saturating_sub(1)))
                    .or_else(|| {
                        Uuid::parse_str(selection.trim())
                            .ok()
                            .and_then(|id| sessions.iter().find(|session| session.id == id))
                    })
                    .or_else(|| {
                        let query = selection.trim().to_ascii_lowercase();
                        sessions
                            .iter()
                            .find(|session| session.title.to_ascii_lowercase().contains(&query))
                    });
                if let Some(selected) = selected {
                    let session = session_for_resume(&runtime, selected.id).await?;
                    session_id = Some(session.id);
                    println!("  已恢复会话：{}", session.title);
                    for message in session.messages.iter().rev().take(6).rev() {
                        if !message.content.trim().is_empty() {
                            let role = match message.role {
                                xdudu_core::provider::MessageRole::User => "❯",
                                xdudu_core::provider::MessageRole::Assistant => "┊",
                                _ => "·",
                            };
                            println!("  {role} {}", redact_text(&message.content));
                        }
                    }
                    if let Some(plan) = pending_plan_for_session(&runtime, session_id).await? {
                        review_plan_classic(&runtime, &mut editor, plan).await?;
                    }
                } else {
                    println!("  没有找到匹配会话。");
                }
                continue;
            }
            "/plan" => {
                if let Some(plan) = pending_plan_for_session(&runtime, session_id).await? {
                    review_plan_classic(&runtime, &mut editor, plan).await?;
                } else {
                    if let Some(plan) = paused_plan_for_session(&runtime, session_id).await? {
                        println!("{}", plan_summary(&plan));
                        println!(
                            "  使用 /plan retry 重试，或 /plan cancel 取消。工具不会自动重放。"
                        );
                    } else {
                        println!("  用法：/plan <目标>");
                    }
                }
                continue;
            }
            "/plan status" => {
                if let Some(id) = session_id
                    && let Some(plan) = runtime.store.latest_plan_for_session(id).await?
                {
                    println!("{}", plan_summary(&plan));
                } else {
                    println!("  当前会话没有计划。");
                }
                continue;
            }
            "/plan run" | "/plan retry" => {
                let Some(id) = session_id else {
                    println!("  当前没有活动会话。");
                    continue;
                };
                let Some(plan) = runtime.store.latest_plan_for_session(id).await? else {
                    println!("  当前会话没有计划。");
                    continue;
                };
                if input == "/plan run" && plan.status != PlanStatus::Approved {
                    println!("  /plan run 只执行已批准计划；暂停计划请使用 /plan retry。");
                    continue;
                }
                if input == "/plan retry" && plan.status != PlanStatus::Paused {
                    println!("  /plan retry 只重试暂停计划。");
                    continue;
                }
                let result = execute_plan_with_sink(&runtime, plan.id, &runtime.renderer).await?;
                println!("  {}", result.message);
                continue;
            }
            "/plan revisions" => {
                let Some(id) = session_id else {
                    println!("  当前没有活动会话。");
                    continue;
                };
                let Some(plan) = runtime.store.latest_plan_for_session(id).await? else {
                    println!("  当前会话没有计划。");
                    continue;
                };
                let revisions = runtime.store.list_plan_revisions(plan.id).await?;
                println!(
                    "  计划修订：{}",
                    revisions
                        .iter()
                        .map(|item| item.revision.to_string())
                        .collect::<Vec<_>>()
                        .join("、")
                );
                continue;
            }
            "/plan cancel" => {
                let Some(id) = session_id else {
                    println!("  当前没有活动会话。");
                    continue;
                };
                let Some(plan) = runtime.store.latest_plan_for_session(id).await? else {
                    println!("  当前会话没有计划。");
                    continue;
                };
                println!("  取消不会撤销既有副作用。再次输入 YES 确认。");
                let confirmed = matches!(
                    editor.read_line("  确认：").map_err(XduduError::from)?,
                    ReadResult::Line(value) if value.trim() == "YES"
                );
                if confirmed {
                    cancel_plan(&runtime, plan.id).await?;
                    println!("  计划已取消；既有副作用未撤销。");
                } else {
                    println!("  已保留计划。");
                }
                continue;
            }
            _ => {}
        }
        if let Some(title) = input
            .strip_prefix("/rename ")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let Some(id) = session_id else {
                println!("  当前没有活动会话。");
                continue;
            };
            let mut session = session_for_resume(&runtime, id).await?;
            session.title = redact_text(&title.chars().take(120).collect::<String>());
            session.touch();
            runtime.store.update(&session).await?;
            println!("  会话已重命名：{}", session.title);
            continue;
        }
        if let Some(raw_id) = input
            .strip_prefix("/resume ")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let id = Uuid::parse_str(raw_id)
                .map_err(|_| XduduError::validation("会话 ID 必须是完整 UUID。"))?;
            let session = session_for_resume(&runtime, id).await?;
            session_id = Some(id);
            println!("  已恢复会话：{}", session.title);
            if let Some(plan) = pending_plan_for_session(&runtime, session_id).await? {
                review_plan_classic(&runtime, &mut editor, plan).await?;
            } else if paused_plan_for_session(&runtime, session_id)
                .await?
                .is_some()
            {
                println!("  此会话有暂停计划；使用 /plan status 查看，/plan retry 重试。");
            }
            continue;
        }
        if let Some(goal) = input
            .strip_prefix("/plan new ")
            .or_else(|| input.strip_prefix("/plan "))
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let (session, plan) =
                create_plan_for_review(&runtime, goal.to_owned(), session_id).await?;
            session_id = Some(session.id);
            if io::stdin().is_terminal() {
                review_plan_classic(&runtime, &mut editor, plan).await?;
            } else {
                println!(
                    "  计划 revision {} 已生成并等待审批。会话：{}",
                    plan.revision, session.id
                );
            }
            continue;
        }
        if let Some(model) = input
            .strip_prefix("/model ")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            runtime.model = model.to_owned();
            let saved = write_config_value(&runtime.cwd, true, "provider.model", &runtime.model);
            println!(
                "  模型已切换：{}{}",
                runtime.model,
                if saved.is_ok() {
                    "（已保存为默认模型）"
                } else {
                    "（默认配置保存失败）"
                }
            );
            continue;
        }
        if let Some(turns) = input.strip_prefix("/turns ") {
            match turns.trim().parse::<u32>() {
                Ok(value) if (1..=100).contains(&value) => {
                    runtime.max_turns = value;
                    println!("  最大循环次数：{value}");
                }
                _ => println!("  最大循环次数必须是 1 到 100 之间的整数。"),
            }
            continue;
        }
        let result = execute_prompt(&runtime, input.to_owned(), session_id).await?;
        runtime.renderer.finish_run(&result)?;
        session_id = Some(result.session_id);
    }
    Ok(0)
}

fn copy_text(text: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    #[cfg(target_os = "windows")]
    let mut child = std::process::Command::new("clip")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut child = std::process::Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("剪贴板命令执行失败"))
    }
}

fn export_session_markdown(
    cwd: &std::path::Path,
    session: &Session,
) -> Result<PathBuf, XduduError> {
    let directory = cwd.join(".xdudu/exports");
    std::fs::create_dir_all(&directory).map_err(XduduError::from)?;
    let path = directory.join(format!("{}.md", session.id));
    let mut output = format!(
        "# {}\n\n- 会话：{}\n- 更新时间：{}\n\n",
        redact_text(&session.title),
        session.id,
        session.updated_at
    );
    for message in &session.messages {
        if message.content.trim().is_empty() {
            continue;
        }
        let heading = match message.role {
            xdudu_core::provider::MessageRole::User => "用户",
            xdudu_core::provider::MessageRole::Assistant => "XDUDU",
            xdudu_core::provider::MessageRole::Tool => "工具",
            xdudu_core::provider::MessageRole::System => "系统",
        };
        output.push_str(&format!(
            "## {heading}\n\n{}\n\n",
            redact_text(&message.content)
        ));
    }
    std::fs::write(&path, output).map_err(XduduError::from)?;
    Ok(path)
}

fn print_config(resolved: &ResolvedConfig) -> Result<(), XduduError> {
    let value = serde_json::json!({
        "config": resolved.config,
        "sources": resolved.sources.iter().map(|(key, source)| {
            (key.clone(), source.as_str())
        }).collect::<std::collections::BTreeMap<_, _>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(XduduError::from)?
    );
    Ok(())
}

fn config_value(resolved: &ResolvedConfig, key: &str) -> Option<String> {
    match key {
        "provider.name" => Some(resolved.config.provider.name.clone()),
        "provider.model" => Some(resolved.config.provider.model.clone()),
        "provider.base_url" => Some(
            resolved
                .config
                .provider
                .base_url
                .clone()
                .unwrap_or_else(|| "<默认端点>".into()),
        ),
        "provider.timeout_seconds" => Some(resolved.config.provider.timeout_seconds.to_string()),
        "provider.max_attempts" => Some(resolved.config.provider.max_attempts.to_string()),
        "provider.retry_base_ms" => Some(resolved.config.provider.retry_base_ms.to_string()),
        "provider.min_request_interval_ms" => {
            Some(resolved.config.provider.min_request_interval_ms.to_string())
        }
        "agent.max_turns" => Some(resolved.config.agent.max_turns.to_string()),
        "agent.permission" => Some(resolved.config.agent.permission.clone()),
        "agent.approval" => Some(resolved.config.agent.approval.clone()),
        "output.json" => Some(resolved.config.output.json.to_string()),
        "output.no_stream" => Some(resolved.config.output.no_stream.to_string()),
        "output.color" => Some(resolved.config.output.color.to_string()),
        "output.debug_trace" => Some(resolved.config.output.debug_trace.to_string()),
        _ => None,
    }
}

async fn handle_config(
    command: ConfigCommand,
    cwd: &std::path::Path,
    cli_overrides: ConfigOverrides,
) -> Result<u8, XduduError> {
    match command {
        ConfigCommand::Show => print_config(&load_config(cwd, cli_overrides)?)?,
        ConfigCommand::Explain { key } => {
            let resolved = load_config(cwd, cli_overrides)?;
            let value = config_value(&resolved, &key)
                .ok_or_else(|| XduduError::validation(format!("未知配置项：{key}")))?;
            let source = resolved
                .source(&key)
                .map(|source| source.as_str())
                .unwrap_or("unknown");
            println!("{key} = {value}\n来源：{source}");
        }
        ConfigCommand::Set {
            key,
            value,
            user,
            project: _,
        } => {
            let path = write_config_value(cwd, user, &key, &value)?;
            println!("已写入：{}", path.display());
        }
        ConfigCommand::Path => {
            let (user, project) = config_paths(cwd)?;
            println!(
                "用户配置：{}\n项目配置：{}\n永久审批规则：{}",
                user.display(),
                project.display(),
                approval_rules_path()?.display()
            );
        }
    }
    Ok(0)
}

fn auth_provider(
    explicit: Option<String>,
    resolved: &ResolvedConfig,
) -> Result<String, XduduError> {
    let provider = explicit.unwrap_or_else(|| resolved.config.provider.name.clone());
    if !matches!(provider.as_str(), "anthropic" | "deepseek") {
        return Err(XduduError::validation(format!(
            "不支持的 Provider：{provider}"
        )));
    }
    Ok(provider)
}

async fn handle_auth(command: AuthCommand, resolved: &ResolvedConfig) -> Result<u8, XduduError> {
    let store = KeyringSecretStore;
    match command {
        AuthCommand::Login { provider } => {
            let provider = auth_provider(provider, resolved)?;
            let value = rpassword::prompt_password(format!("{provider} API Key："))
                .map_err(XduduError::from)?;
            store.set(&provider, SecretString::new(value)?).await?;
            println!("已将 {provider} API Key 保存到系统凭据存储。");
        }
        AuthCommand::Status { provider } => {
            let provider = auth_provider(provider, resolved)?;
            match resolve_secret(&provider, &store).await {
                Ok((secret, source)) => {
                    let source = match source {
                        SecretSource::Environment => "环境变量",
                        SecretSource::SystemStore => "系统凭据",
                    };
                    println!("{provider}：已配置（{source}，{}）", secret.masked());
                }
                Err(_) => println!("{provider}：未配置"),
            }
        }
        AuthCommand::Logout { provider } => {
            let provider = auth_provider(provider, resolved)?;
            if store.delete(&provider).await? {
                println!("已从系统凭据存储删除 {provider} API Key。");
            } else {
                println!("系统凭据中没有 {provider} API Key。");
            }
        }
    }
    Ok(0)
}

async fn handle_approval(command: ApprovalCommand) -> Result<u8, XduduError> {
    let store = JsonApprovalRuleStore::open(approval_rules_path()?).await?;
    match command {
        ApprovalCommand::List => {
            let rules = store.list().await;
            if rules.is_empty() {
                println!("没有永久审批规则。\n规则文件：{}", store.path().display());
            } else {
                println!("工具                 副作用");
                for rule in rules {
                    println!("{:<20} {}", rule.tool_name, rule.side_effect.as_str());
                }
                println!("规则文件：{}", store.path().display());
            }
        }
        ApprovalCommand::Revoke { tool } => {
            let removed = store.revoke(&tool).await?;
            if removed == 0 {
                println!("没有找到工具“{tool}”的永久审批规则。");
            } else {
                println!("已撤销工具“{tool}”的永久审批规则。");
            }
        }
        ApprovalCommand::Clear => {
            let removed = store.clear().await?;
            println!("已清除 {removed} 条永久审批规则。");
        }
    }
    Ok(0)
}

async fn handle_mcp(command: McpCommand) -> Result<u8, XduduError> {
    let mut config = load_mcp_config()?;
    match command {
        McpCommand::List => {
            if config.servers.is_empty() {
                println!(
                    "尚未配置 MCP Server。配置文件：{}",
                    mcp_config_path()?.display()
                );
            } else {
                for server in &config.servers {
                    let endpoint = match server.transport {
                        McpTransportKind::Stdio => server.command.as_deref().unwrap_or_default(),
                        McpTransportKind::StreamableHttp => {
                            server.url.as_deref().unwrap_or_default()
                        }
                    };
                    println!(
                        "{}\t{}\t{}\t{}",
                        server.name,
                        if server.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        match server.transport {
                            McpTransportKind::Stdio => "stdio",
                            McpTransportKind::StreamableHttp => "streamable-http",
                        },
                        endpoint
                    );
                }
            }
        }
        McpCommand::Show { name } => {
            let server = find_mcp_server(&config, &name)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "name":server.name,
                    "enabled":server.enabled,
                    "transport":server.transport,
                    "command":server.command,
                    "args":server.args,
                    "environmentKeys":server.env.keys().collect::<Vec<_>>(),
                    "url":server.url,
                    "credential":server.credential.as_ref().map(|_| "[系统凭据]"),
                    "timeoutSeconds":server.timeout_seconds,
                }))?
            );
        }
        McpCommand::AddStdio {
            name,
            command,
            args,
        } => {
            ensure_new_mcp_name(&config, &name)?;
            config.servers.push(McpServerConfig {
                name: name.clone(),
                enabled: true,
                transport: McpTransportKind::Stdio,
                command: Some(command),
                args,
                env: Default::default(),
                url: None,
                credential: None,
                timeout_seconds: 30,
            });
            let path = save_mcp_config(&config)?;
            println!(
                "已添加并启用 stdio MCP Server：{name}\n配置：{}",
                path.display()
            );
        }
        McpCommand::AddHttp { name, url, auth } => {
            ensure_new_mcp_name(&config, &name)?;
            config.servers.push(McpServerConfig {
                name: name.clone(),
                enabled: true,
                transport: McpTransportKind::StreamableHttp,
                command: None,
                args: Vec::new(),
                env: Default::default(),
                url: Some(url),
                credential: auth.then(|| name.clone()),
                timeout_seconds: 30,
            });
            let path = save_mcp_config(&config)?;
            println!("已添加并启用 Streamable HTTP MCP Server：{name}");
            if auth {
                println!("请继续运行：xdudu mcp login {name}");
            }
            println!("配置：{}", path.display());
        }
        McpCommand::Enable { name } => {
            let server = find_mcp_server_mut(&mut config, &name)?;
            server.enabled = true;
            save_mcp_config(&config)?;
            println!("MCP Server {name} 已启用。");
        }
        McpCommand::Disable { name } => {
            let server = find_mcp_server_mut(&mut config, &name)?;
            server.enabled = false;
            save_mcp_config(&config)?;
            println!("MCP Server {name} 已禁用。");
        }
        McpCommand::Remove { name } => {
            let before = config.servers.len();
            config.servers.retain(|server| server.name != name);
            if config.servers.len() == before {
                return Err(XduduError::validation(format!("找不到 MCP Server：{name}")));
            }
            save_mcp_config(&config)?;
            println!("已删除 MCP Server 配置：{name}");
        }
        McpCommand::Login { name } => {
            let server = find_mcp_server(&config, &name)?;
            if server.transport != McpTransportKind::StreamableHttp {
                return Err(XduduError::validation("只有 HTTP MCP 使用 Bearer Token。"));
            }
            let account = server
                .credential_account()
                .ok_or_else(|| XduduError::validation("该 Server 未启用认证引用。"))?;
            let token = rpassword::prompt_password(format!("请输入 {name} 的 Bearer Token："))?;
            KeyringSecretStore
                .set(&account, SecretString::new(token)?)
                .await?;
            println!("已保存到系统凭据：{account}");
        }
        McpCommand::Logout { name } => {
            let account = format!("mcp:{name}");
            let removed = KeyringSecretStore.delete(&account).await?;
            println!(
                "{}",
                if removed {
                    format!("已删除系统凭据：{account}")
                } else {
                    format!("系统凭据不存在：{account}")
                }
            );
        }
        McpCommand::Doctor { name } => {
            let selected = config
                .servers
                .iter()
                .filter(|server| name.as_ref().is_none_or(|value| &server.name == value))
                .cloned()
                .collect::<Vec<_>>();
            if selected.is_empty() {
                return Err(XduduError::validation("没有匹配的 MCP Server。"));
            }
            let store = KeyringSecretStore;
            for server in selected {
                if !server.enabled {
                    println!("{}\tdisabled", server.name);
                    continue;
                }
                let runtime = McpServerRuntime::new(server.clone(), &store).await?;
                let tools = runtime.list_tools(CancellationToken::new()).await?;
                println!("{}\tok\t{} tools", server.name, tools.len());
            }
        }
    }
    Ok(0)
}

async fn handle_plugin(command: PluginCommand) -> Result<u8, XduduError> {
    let mut manifests = load_plugin_manifests()?;
    match command {
        PluginCommand::List => {
            if manifests.is_empty() {
                println!("尚未安装插件。目录：{}", plugin_directory()?.display());
            } else {
                for plugin in manifests {
                    println!(
                        "{}\t{}\t{}\t{} MCP servers",
                        plugin.id,
                        if plugin.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        plugin.version,
                        plugin.mcp_servers.len()
                    );
                }
            }
        }
        PluginCommand::Show { id } => {
            let plugin = find_plugin(&manifests, &id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "schemaVersion": plugin.schema_version,
                    "id": plugin.id,
                    "name": plugin.name,
                    "version": plugin.version,
                    "description": plugin.description,
                    "enabled": plugin.enabled,
                    "homepage": plugin.homepage,
                    "sha256": plugin.sha256,
                    "signature": plugin.signature.as_ref().map(|signature| serde_json::json!({
                        "algorithm": signature.algorithm,
                        "keyId": signature.key_id,
                        "value": "[已隐藏]",
                    })),
                    "mcpServers": plugin.mcp_servers.iter().map(|server| serde_json::json!({
                        "name": server.name,
                        "enabled": server.enabled,
                        "transport": server.transport,
                        "command": server.command,
                        "args": server.args,
                        "environmentKeys": server.env.keys().collect::<Vec<_>>(),
                        "url": server.url,
                        "credential": server.credential.as_ref().map(|_| "[系统凭据]"),
                    })).collect::<Vec<_>>(),
                }))?
            );
        }
        PluginCommand::Enable { id } => {
            let plugin = find_plugin_mut(&mut manifests, &id)?;
            plugin.enabled = true;
            save_plugin_manifest(plugin)?;
            println!("插件 {id} 已启用。");
        }
        PluginCommand::Disable { id } => {
            let plugin = find_plugin_mut(&mut manifests, &id)?;
            plugin.enabled = false;
            save_plugin_manifest(plugin)?;
            println!("插件 {id} 已禁用。");
        }
        PluginCommand::Doctor { id } => {
            let selected = manifests
                .into_iter()
                .filter(|plugin| id.as_ref().is_none_or(|value| &plugin.id == value))
                .collect::<Vec<_>>();
            if selected.is_empty() {
                return Err(XduduError::validation("没有匹配的插件。"));
            }
            let store = KeyringSecretStore;
            for plugin in selected {
                plugin.validate()?;
                if !plugin.enabled {
                    println!("{}\tdisabled", plugin.id);
                    continue;
                }
                let mut tool_count = 0usize;
                for server in plugin.mcp_servers {
                    if !server.enabled {
                        continue;
                    }
                    let runtime = McpServerRuntime::new(server, &store).await?;
                    tool_count += runtime.list_tools(CancellationToken::new()).await?.len();
                }
                println!("{}\tok\t{} tools", plugin.id, tool_count);
            }
        }
    }
    Ok(0)
}

fn find_plugin<'a>(
    manifests: &'a [PluginManifest],
    id: &str,
) -> Result<&'a PluginManifest, XduduError> {
    manifests
        .iter()
        .find(|plugin| plugin.id == id)
        .ok_or_else(|| XduduError::validation(format!("找不到插件：{id}")))
}

fn find_plugin_mut<'a>(
    manifests: &'a mut [PluginManifest],
    id: &str,
) -> Result<&'a mut PluginManifest, XduduError> {
    manifests
        .iter_mut()
        .find(|plugin| plugin.id == id)
        .ok_or_else(|| XduduError::validation(format!("找不到插件：{id}")))
}

fn find_mcp_server<'a>(
    config: &'a McpConfigFile,
    name: &str,
) -> Result<&'a McpServerConfig, XduduError> {
    config
        .servers
        .iter()
        .find(|server| server.name == name)
        .ok_or_else(|| XduduError::validation(format!("找不到 MCP Server：{name}")))
}

fn find_mcp_server_mut<'a>(
    config: &'a mut McpConfigFile,
    name: &str,
) -> Result<&'a mut McpServerConfig, XduduError> {
    config
        .servers
        .iter_mut()
        .find(|server| server.name == name)
        .ok_or_else(|| XduduError::validation(format!("找不到 MCP Server：{name}")))
}

fn ensure_new_mcp_name(config: &McpConfigFile, name: &str) -> Result<(), XduduError> {
    if config.servers.iter().any(|server| server.name == name) {
        Err(XduduError::validation(format!("MCP Server 已存在：{name}")))
    } else {
        Ok(())
    }
}

async fn handle_session(command: SessionCommand, cwd: &std::path::Path) -> Result<u8, XduduError> {
    let store = SqliteSessionStore::new(cwd)?;
    match command {
        SessionCommand::List { limit } => {
            let sessions = store.list(limit as usize).await?;
            if sessions.is_empty() {
                println!("当前工作区还没有会话。");
                return Ok(0);
            }
            println!(
                "会话 ID                              状态               更新时间                 标题"
            );
            for session in sessions {
                let status = serde_json::to_string(&session.status)
                    .unwrap_or_else(|_| "\"unknown\"".into())
                    .trim_matches('"')
                    .to_owned();
                println!(
                    "{}  {:<18} {}  {}",
                    session.id,
                    status,
                    session.updated_at.format("%Y-%m-%d %H:%M:%S"),
                    session.title.replace(['\r', '\n'], " ")
                );
            }
        }
        SessionCommand::Show { id } => {
            let session = store
                .get(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到会话：{id}")))?;
            println!("{}", serde_json::to_string_pretty(&session)?);
        }
        SessionCommand::Resume { .. } => {
            return Err(XduduError::validation(
                "session resume 应由 Agent 运行入口处理。",
            ));
        }
    }
    Ok(0)
}

async fn handle_plan_command(runtime: &Runtime, command: PlanCommand) -> Result<u8, XduduError> {
    match command {
        PlanCommand::Create { goal } => {
            let (session, plan) = create_plan_for_review(runtime, goal, None).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "sessionId": session.id,
                    "plan": plan
                }))?
            );
        }
        PlanCommand::List { limit } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&runtime.store.list_plans(limit).await?)?
            );
        }
        PlanCommand::Show { id } => {
            let plan = runtime
                .store
                .get_plan(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到计划：{id}")))?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
        }
        PlanCommand::Revisions { id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&runtime.store.list_plan_revisions(id).await?)?
            );
        }
        PlanCommand::Approve { id, reason } => {
            let plan = runtime
                .store
                .get_plan(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到计划：{id}")))?;
            let approved = approve_plan(&runtime.store, id, plan.revision, reason).await?;
            sync_plan_session(
                runtime,
                approved.session_id,
                SessionStatus::PlanReady,
                AgentLoopState::Completed,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&approved)?);
        }
        PlanCommand::Reject { id, reason } => {
            let plan = runtime
                .store
                .get_plan(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到计划：{id}")))?;
            let rejected = reject_plan(&runtime.store, id, plan.revision, reason).await?;
            sync_plan_session(
                runtime,
                rejected.session_id,
                SessionStatus::Incomplete,
                AgentLoopState::Incomplete,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&rejected)?);
        }
        PlanCommand::Revise { id, request } => {
            let plan = runtime
                .store
                .get_plan(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到计划：{id}")))?;
            let session = session_for_resume(runtime, plan.session_id).await?;
            let revised = revise_plan(PlanRevisionConfig {
                plan_id: id,
                expected_revision: plan.revision,
                change_request: request,
                context: plan_context(&session),
                model: runtime.model.clone(),
                cwd: runtime.cwd.clone(),
                provider: runtime.provider.as_ref(),
                plan_store: &runtime.store,
                cancellation: CancellationToken::new(),
            })
            .await?;
            sync_plan_session(
                runtime,
                revised.plan.session_id,
                SessionStatus::WaitingApproval,
                AgentLoopState::WaitingApproval,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&revised.plan)?);
        }
        PlanCommand::Run { id } => {
            let plan = runtime
                .store
                .get_plan(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到计划：{id}")))?;
            if plan.status != PlanStatus::Approved {
                return Err(XduduError::validation(
                    "plan run 只执行已批准计划；暂停计划请使用 plan retry。",
                ));
            }
            let result = execute_plan_with_sink(runtime, id, &runtime.renderer).await?;
            println!("{}", serde_json::to_string_pretty(&result.plan)?);
            return Ok((!result.completed) as u8);
        }
        PlanCommand::Retry { id } => {
            let plan = runtime
                .store
                .get_plan(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到计划：{id}")))?;
            if plan.status != PlanStatus::Paused {
                return Err(XduduError::validation("plan retry 只重试暂停计划。"));
            }
            let result = execute_plan_with_sink(runtime, id, &runtime.renderer).await?;
            println!("{}", serde_json::to_string_pretty(&result.plan)?);
            return Ok((!result.completed) as u8);
        }
        PlanCommand::Cancel { id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&cancel_plan(runtime, id).await?)?
            );
        }
    }
    Ok(0)
}

async fn handle_plan_local(cwd: &std::path::Path, command: PlanCommand) -> Result<u8, XduduError> {
    let store = SqliteSessionStore::new(cwd)?;
    match command {
        PlanCommand::List { limit } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&store.list_plans(limit).await?)?
            );
        }
        PlanCommand::Show { id } => {
            let plan = store
                .get_plan(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到计划：{id}")))?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
        }
        PlanCommand::Revisions { id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&store.list_plan_revisions(id).await?)?
            );
        }
        PlanCommand::Approve { id, reason } => {
            let plan = store
                .get_plan(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到计划：{id}")))?;
            let approved = approve_plan(&store, id, plan.revision, reason).await?;
            sync_plan_session_store(
                &store,
                approved.session_id,
                SessionStatus::PlanReady,
                AgentLoopState::Completed,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&approved)?);
        }
        PlanCommand::Reject { id, reason } => {
            let plan = store
                .get_plan(id)
                .await?
                .ok_or_else(|| XduduError::validation(format!("找不到计划：{id}")))?;
            let rejected = reject_plan(&store, id, plan.revision, reason).await?;
            sync_plan_session_store(
                &store,
                rejected.session_id,
                SessionStatus::Incomplete,
                AgentLoopState::Incomplete,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&rejected)?);
        }
        PlanCommand::Cancel { id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&cancel_plan_with_store(&store, id).await?)?
            );
        }
        _ => unreachable!("本地计划入口只接收无需 Provider 的命令"),
    }
    Ok(0)
}

async fn run() -> Result<u8, XduduError> {
    let cli = Cli::parse();
    let cwd = env::current_dir().map_err(XduduError::from)?;
    let cli_overrides = overrides(&cli);

    match cli.command {
        Some(Command::Config { command }) => {
            return handle_config(command, &cwd, cli_overrides).await;
        }
        Some(Command::Auth { command }) => {
            let resolved = load_config(&cwd, cli_overrides)?;
            return handle_auth(command, &resolved).await;
        }
        Some(Command::Approval { command }) => {
            return handle_approval(command).await;
        }
        Some(Command::Mcp { command }) => {
            return handle_mcp(command).await;
        }
        Some(Command::Plugin { command }) => {
            return handle_plugin(command).await;
        }
        Some(Command::Doctor) => {
            return run_doctor(&cwd, cli_overrides, cli.json).await;
        }
        Some(Command::Undo(args)) => {
            let _workspace_lock = WorkspaceLock::acquire(&cwd)?;
            let result = JsonChangeLedger::new(&cwd)
                .undo(args.change, cli.session)
                .await?;
            let action = if result.removed_created_files == result.paths.len() {
                "已删除由 Agent 创建的文件"
            } else {
                "已恢复变更事务"
            };
            println!(
                "{action}：{}\n变更记录：{}",
                result
                    .paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("、"),
                result.change_id
            );
            return Ok(0);
        }
        Some(Command::Session {
            command: command @ (SessionCommand::List { .. } | SessionCommand::Show { .. }),
        }) => {
            return handle_session(command, &cwd).await;
        }
        Some(Command::Plan {
            command:
                command @ (PlanCommand::List { .. }
                | PlanCommand::Show { .. }
                | PlanCommand::Revisions { .. }
                | PlanCommand::Approve { .. }
                | PlanCommand::Reject { .. }
                | PlanCommand::Cancel { .. }),
        }) => {
            return handle_plan_local(&cwd, command).await;
        }
        _ => {}
    }

    let command_prompt = match &cli.command {
        Some(Command::Run(args)) => args.prompt.clone(),
        Some(Command::Session {
            command: SessionCommand::Resume { prompt, .. },
        }) => prompt.clone(),
        _ => None,
    };
    let requested_session = match &cli.command {
        Some(Command::Session {
            command: SessionCommand::Resume { id, .. },
        }) => Some(*id),
        _ => cli.session,
    };
    let resolved = load_config(&cwd, cli_overrides)?;
    if let Some(Command::Plan { command }) = cli.command {
        let runtime = create_runtime(cwd, &resolved, false).await?;
        return handle_plan_command(&runtime, command).await;
    }
    let piped = !io::stdin().is_terminal();
    let prompt = command_prompt.or(cli.prompt);
    let interactive = cli.interactive || (prompt.is_none() && !piped);
    if resolved.config.output.json && interactive {
        return Err(XduduError::validation(
            "--json 仅支持非交互模式，请同时提供 prompt 或管道输入。",
        ));
    }
    let runtime = create_runtime(cwd, &resolved, interactive).await?;
    if !resolved.config.output.json && !interactive {
        print_banner(&runtime, interactive);
    }
    if interactive {
        return interactive_loop(runtime, prompt, requested_session).await;
    }
    let prompt = if let Some(prompt) = prompt {
        prompt
    } else {
        let mut input = String::new();
        tokio::io::stdin()
            .read_to_string(&mut input)
            .await
            .map_err(XduduError::from)?;
        input.trim().to_owned()
    };
    if prompt.is_empty() {
        return Err(XduduError::validation("prompt 不能为空。"));
    }
    let result = execute_prompt(&runtime, prompt, requested_session).await?;
    runtime.renderer.finish_run(&result)?;
    Ok(result.exit_code)
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("\n  错误：{}", redact_text(&error.message));
            ExitCode::from(error.exit_code())
        }
    }
}
