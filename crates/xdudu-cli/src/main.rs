//! XDUDU Rust 命令行入口。

mod doctor;
mod renderer;

use std::{
    env,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

use async_trait::async_trait;
use clap::{Args, Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xdudu_core::{
    AgentRunConfig, AgentRunResult, AllowAllApprovalGate, ApprovalDecision, ApprovalGate,
    ApprovalMode, ApprovalRequest, ConfigOverrides, DefaultProviderFactory, DenyAllApprovalGate,
    JsonChangeLedger, JsonSessionStore, KeyringSecretStore, PermissionMode, Provider,
    ProviderFactory, ResolvedConfig, SecretSource, SecretStore, SecretString, ToolRegistry,
    XduduError, config_paths, load_config, redact_text, redact_value, register_builtins,
    resolve_secret, run_agent, write_config_value,
};

use crate::doctor::run_doctor;
use crate::renderer::ConsoleRenderer;

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
    /// 检查安装、配置、凭据和工作区状态。
    Doctor,
    /// 安全撤销最近一次或指定的 Agent 文件变更。
    Undo(UndoArgs),
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

struct Runtime {
    provider: Box<dyn Provider>,
    provider_display: String,
    model: String,
    max_turns: u32,
    cwd: PathBuf,
    permission_mode: PermissionMode,
    approval_mode: ApprovalMode,
    registry: ToolRegistry,
    store: JsonSessionStore,
    renderer: ConsoleRenderer,
    stream: bool,
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
struct ConsoleApprovalGate;

#[async_trait]
impl ApprovalGate for ConsoleApprovalGate {
    async fn review(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let input = serde_json::to_string_pretty(&redact_value(&request.input))
            .unwrap_or_else(|_| "<无法显示输入>".into());
        let prompt = format!(
            "\n  待审批工具：{}\n  副作用：{}\n  输入：{}\n  是否批准？[y/N] ",
            request.tool_name,
            request.side_effect.as_str(),
            input
        );
        match tokio::task::spawn_blocking(move || {
            eprint!("{prompt}");
            io::stderr().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            Ok::<_, io::Error>(answer)
        })
        .await
        {
            Ok(Ok(answer))
                if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") =>
            {
                ApprovalDecision::approve("用户在交互终端批准。")
            }
            Ok(Ok(_)) => ApprovalDecision::deny("用户拒绝或未明确批准。"),
            Ok(Err(error)) => ApprovalDecision::deny(format!("读取审批输入失败：{error}")),
            Err(error) => ApprovalDecision::deny(format!("审批任务失败：{error}")),
        }
    }
}

async fn create_runtime(
    cwd: PathBuf,
    resolved: &ResolvedConfig,
    interactive: bool,
) -> Result<Runtime, XduduError> {
    let store = KeyringSecretStore;
    let (secret, _) = resolve_secret(&resolved.config.provider.name, &store).await?;
    let provider = DefaultProviderFactory.create(&resolved.config.provider, secret)?;
    let approval_mode = resolved.config.agent.approval_mode()?;
    let approval_gate: Arc<dyn ApprovalGate> = match approval_mode {
        ApprovalMode::Always => Arc::new(AllowAllApprovalGate),
        ApprovalMode::Never => Arc::new(DenyAllApprovalGate),
        ApprovalMode::Ask if interactive && !resolved.config.output.json => {
            Arc::new(ConsoleApprovalGate)
        }
        ApprovalMode::Ask => Arc::new(DenyAllApprovalGate),
    };
    let change_ledger = Arc::new(JsonChangeLedger::new(&cwd));
    let mut registry = ToolRegistry::with_runtime(approval_gate, change_ledger);
    register_builtins(&mut registry)?;
    Ok(Runtime {
        provider,
        provider_display: provider_label(&resolved.config.provider.name),
        model: resolved.config.provider.model.clone(),
        max_turns: resolved.config.agent.max_turns,
        cwd: cwd.clone(),
        permission_mode: resolved.config.agent.permission_mode()?,
        approval_mode,
        registry,
        store: JsonSessionStore::new(&cwd),
        renderer: ConsoleRenderer::new(
            resolved.config.output.json,
            !resolved.config.output.no_stream,
            resolved.config.output.color && io::stdout().is_terminal(),
        ),
        stream: !resolved.config.output.no_stream,
    })
}

async fn execute_prompt(
    runtime: &Runtime,
    prompt: String,
    session_id: Option<Uuid>,
) -> Result<AgentRunResult, XduduError> {
    runtime.renderer.begin_run();
    let cancellation = CancellationToken::new();
    let run = run_agent(AgentRunConfig {
        prompt,
        model: runtime.model.clone(),
        max_turns: runtime.max_turns,
        cwd: runtime.cwd.clone(),
        provider: runtime.provider.as_ref(),
        tool_registry: &runtime.registry,
        session_store: &runtime.store,
        permission_mode: runtime.permission_mode,
        cancellation: cancellation.clone(),
        session_id,
        event_sink: Some(&runtime.renderer),
        stream: runtime.stream,
    });
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => result,
        signal = tokio::signal::ctrl_c() => {
            if signal.is_ok() {
                eprintln!("\n  ⏸  已中断，正在保存...");
                cancellation.cancel();
            }
            run.await
        }
    }
}

fn print_banner(runtime: &Runtime, interactive: bool) {
    println!(
        "\n  XDUDU v{} — Rust AI 编程助手",
        env!("CARGO_PKG_VERSION")
    );
    println!(
        "  Provider: {}  |  模型: {}",
        runtime.provider_display, runtime.model
    );
    println!("  工作目录: {}", runtime.cwd.display());
    println!("  权限模式: {}", runtime.permission_mode.as_str());
    println!("  审批模式: {}", runtime.approval_mode.as_str());
    if interactive {
        println!("  输入 /help 查看命令，/exit 退出\n");
    }
}

async fn interactive_loop(
    mut runtime: Runtime,
    initial_prompt: Option<String>,
    initial_session: Option<Uuid>,
) -> Result<u8, XduduError> {
    let mut session_id = initial_session;
    if let Some(prompt) = initial_prompt {
        let result = execute_prompt(&runtime, prompt, session_id).await?;
        runtime.renderer.finish_run(&result)?;
        session_id = Some(result.session_id);
    }

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    loop {
        print!("\n❯ ");
        io::stdout().flush().map_err(XduduError::from)?;
        let Some(line) = lines.next_line().await.map_err(XduduError::from)? else {
            break;
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        match input {
            "/exit" | "/quit" | "/q" => {
                println!("  再见！");
                break;
            }
            "/help" | "/h" => {
                println!(
                    "  /help        显示帮助\n  /exit        退出\n  /new         开始新会话\n  /model NAME  切换模型\n  /turns N     修改最大循环次数"
                );
                continue;
            }
            "/new" => {
                session_id = None;
                println!("  已开始新会话。");
                continue;
            }
            _ => {}
        }
        if let Some(model) = input
            .strip_prefix("/model ")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            runtime.model = model.to_owned();
            println!("  模型已切换：{}", runtime.model);
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
                "用户配置：{}\n项目配置：{}",
                user.display(),
                project.display()
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
        Some(Command::Doctor) => {
            return run_doctor(&cwd, cli_overrides, cli.json).await;
        }
        Some(Command::Undo(args)) => {
            let result = JsonChangeLedger::new(&cwd)
                .undo(args.change, cli.session)
                .await?;
            let action = if result.removed_created_file {
                "已删除由 Agent 创建的文件"
            } else {
                "已恢复变更前内容"
            };
            println!(
                "{action}：{}\n变更记录：{}",
                result.path.display(),
                result.change_id
            );
            return Ok(0);
        }
        _ => {}
    }

    let command_prompt = match &cli.command {
        Some(Command::Run(args)) => args.prompt.clone(),
        _ => None,
    };
    let resolved = load_config(&cwd, cli_overrides)?;
    let piped = !io::stdin().is_terminal();
    let prompt = command_prompt.or(cli.prompt);
    let interactive = cli.interactive || (prompt.is_none() && !piped);
    if resolved.config.output.json && interactive {
        return Err(XduduError::validation(
            "--json 仅支持非交互模式，请同时提供 prompt 或管道输入。",
        ));
    }
    let runtime = create_runtime(cwd, &resolved, interactive).await?;
    if !resolved.config.output.json {
        print_banner(&runtime, interactive);
    }
    if interactive {
        return interactive_loop(runtime, prompt, cli.session).await;
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
    let result = execute_prompt(&runtime, prompt, cli.session).await?;
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
