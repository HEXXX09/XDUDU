//! `terminal_exec`：不经过 shell，以“可执行文件 + 参数数组”执行命令。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time,
};

use crate::{
    SideEffectKind,
    config::CommandRules,
    permission::{PermissionLevel, PermissionMode},
};

use super::path_policy::{resolve_directory, resolve_existing};
use super::{
    Tool, ToolContext, ToolDefinition, ToolResult, object, reject_unknown_fields, required_string,
};

const MAX_OUTPUT_LENGTH: usize = 100_000;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

pub struct TerminalExecTool;

/// 命令三档档位：deny > allow > ask。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandTier {
    Deny,
    Allow,
    Ask,
}

/// 按「完整可执行名 + 首个参数」前缀匹配三档规则，匹配顺序 deny > allow > ask。
pub(crate) fn classify_command(
    command: &str,
    args: &[String],
    rules: &CommandRules,
) -> CommandTier {
    let first_arg = args.first().map(String::as_str).unwrap_or_default();
    let rule_matches = |rule: &String| {
        let mut parts = rule.split_whitespace();
        if parts.next() != Some(command) {
            return false;
        }
        match parts.next() {
            None => true,
            Some(expected) => expected == first_arg,
        }
    };
    if rules.deny.iter().any(rule_matches) {
        CommandTier::Deny
    } else if rules.allow.iter().any(rule_matches) {
        CommandTier::Allow
    } else {
        CommandTier::Ask
    }
}

fn valid_command(command: &str) -> bool {
    !command.is_empty()
        && command.len() <= 128
        && command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
}

fn check_git_args(args: &[String]) -> Result<(), String> {
    let Some(subcommand) = args.first() else {
        return Err(
            "auto-safe 只允许只读 git 子命令：status、diff、log、show、branch、stash。".into(),
        );
    };
    match subcommand.as_str() {
        "status" | "diff" | "log" | "show" => {}
        "branch" => {
            let read_only_flags = [
                "-l",
                "--list",
                "-a",
                "--all",
                "-r",
                "--remotes",
                "-v",
                "--verbose",
            ];
            if args
                .iter()
                .skip(1)
                .any(|arg| !read_only_flags.contains(&arg.as_str()))
            {
                return Err("auto-safe 的 git branch 只允许只读列举参数（-l/-a/-r/-v）。".into());
            }
        }
        "stash" => {
            if args.len() > 1 && !matches!(args[1].as_str(), "list" | "show") {
                return Err("auto-safe 的 git stash 只允许 list 与 show。".into());
            }
        }
        _ => {
            return Err(
                "auto-safe 只允许只读 git 子命令：status、diff、log、show、branch、stash。".into(),
            );
        }
    }
    let forbidden = [
        "-C",
        "-c",
        "--git-dir",
        "--work-tree",
        "--no-index",
        "--ext-diff",
        "--output",
        "--exec",
    ];
    if args.iter().any(|arg| {
        forbidden
            .iter()
            .any(|item| arg == item || arg.starts_with(&format!("{item}=")))
    }) {
        return Err("git 参数可能改变仓库边界、执行外部程序或写入文件。".into());
    }
    Ok(())
}

/// auto-safe 命令分类：先应用三档规则（deny 立即拒绝），再对 allow 档附加
/// 固定安全校验（ls 工作区隔离、git 只读参数）。返回档位供审批决策使用。
async fn classify_auto_safe(
    command: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    rules: &CommandRules,
) -> Result<CommandTier, String> {
    if !env.is_empty() {
        return Err("auto-safe 不允许覆盖环境变量。".into());
    }
    let tier = classify_command(command, args, rules);
    if tier == CommandTier::Deny {
        return Err(format!("命令“{command}”命中 deny 规则，已拒绝。"));
    }
    match command {
        "ls" => {
            let allowed_flags = ["-a", "-A", "--all", "-l", "-la", "-al"];
            if let Some(flag) = args
                .iter()
                .find(|arg| arg.starts_with('-') && !allowed_flags.contains(&arg.as_str()))
            {
                return Err(format!("auto-safe 的 ls 不支持参数：{flag}"));
            }
            for arg in args.iter().filter(|arg| !arg.starts_with('-')) {
                resolve_existing(Path::new(arg), cwd)
                    .await
                    .map_err(|_| format!("ls 路径不在工作区内或不存在：{arg}"))?;
            }
        }
        "git" => check_git_args(args)?,
        _ => {}
    }
    Ok(tier)
}

fn bounded_output(text: String) -> (String, bool) {
    if text.len() <= MAX_OUTPUT_LENGTH {
        return (text, false);
    }
    let mut end = MAX_OUTPUT_LENGTH;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn portable_success(
    command: &str,
    args: &[String],
    cwd: &Path,
    stdout: String,
    context: &ToolContext,
) -> ToolResult {
    let (stdout, truncated) = bounded_output(stdout);
    let output_summary = stdout
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    ToolResult::success(
        json!({
            "exitCode":0,
            "signal":null,
            "stdout":stdout,
            "stderr":"",
            "outputSummary":output_summary,
            "truncated":truncated
        }),
        context.started_at,
        json!({"command":command,"args":args,"exitCode":0,"signal":null,"cwd":cwd}),
    )
}

async fn portable_ls(args: &[String], cwd: &Path) -> Result<String, String> {
    let show_hidden = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-a" | "-A" | "--all" | "-la" | "-al"));
    let long = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-l" | "-la" | "-al"));
    let requested = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .collect::<Vec<_>>();
    let requested = if requested.is_empty() {
        vec!["."]
    } else {
        requested
    };
    let multiple = requested.len() > 1;
    let mut output = String::new();
    for (index, requested_path) in requested.iter().enumerate() {
        let path = resolve_existing(Path::new(requested_path), cwd)
            .await
            .map_err(|error| error.message)?;
        if multiple {
            if index > 0 {
                output.push('\n');
            }
            output.push_str(requested_path);
            output.push_str(":\n");
        }
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|error| format!("读取 {requested_path} 元数据失败：{error}"))?;
        if !metadata.is_dir() {
            output.push_str(requested_path);
            output.push('\n');
            continue;
        }
        let mut directory = tokio::fs::read_dir(&path)
            .await
            .map_err(|error| format!("读取目录 {requested_path} 失败：{error}"))?;
        let mut entries = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|error| format!("读取目录 {requested_path} 失败：{error}"))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let metadata = entry
                .metadata()
                .await
                .map_err(|error| format!("读取 {name} 元数据失败：{error}"))?;
            entries.push((name, metadata.is_dir(), metadata.len()));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, is_directory, size) in entries {
            if long {
                let kind = if is_directory { 'd' } else { '-' };
                output.push_str(&format!("{kind} {size:>10} {name}"));
            } else {
                output.push_str(&name);
            }
            if is_directory {
                output.push('/');
            }
            output.push('\n');
        }
    }
    Ok(output)
}

fn parse_args(input: &Value) -> Vec<String> {
    input
        .get("args")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_env(input: &Value) -> HashMap<String, String> {
    input
        .get("env")
        .and_then(Value::as_object)
        .map(|env| {
            env.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn resolve_safe_executable(command: &str, workspace: &Path) -> Option<PathBuf> {
    let real_workspace = tokio::fs::canonicalize(workspace).await.ok()?;
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        let real_directory = match tokio::fs::canonicalize(&directory).await {
            Ok(value) => value,
            Err(_) => continue,
        };
        if real_directory.starts_with(&real_workspace) {
            continue;
        }
        let candidate = real_directory.join(command);
        let real_candidate = match tokio::fs::canonicalize(&candidate).await {
            Ok(value) => value,
            Err(_) => continue,
        };
        let metadata = match tokio::fs::metadata(&real_candidate).await {
            Ok(value) => value,
            _ => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Some(real_candidate);
    }
    None
}

async fn read_limited(mut reader: impl AsyncRead + Unpin) -> (String, bool) {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let remaining = MAX_OUTPUT_LENGTH.saturating_sub(retained.len());
        if remaining > 0 {
            retained.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            truncated = true;
        }
    }
    (String::from_utf8_lossy(&retained).into_owned(), truncated)
}

#[async_trait]
impl Tool for TerminalExecTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "terminal_exec".into(),
            description: "运行单个可执行文件并返回 stdout、stderr 和退出码。所有参数必须放入 args；auto-safe 按三档策略执行：命中 allow 白名单直接运行，命中 ask 或未匹配的命令进入审批，命中 deny 的命令立即拒绝。".into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string","pattern":"^[A-Za-z0-9._+-]+$"},
                    "args":{"type":"array","items":{"type":"string","maxLength":4096},"maxItems":128},
                    "cwd":{"type":"string","minLength":1,"maxLength":4096},
                    "timeoutMs":{"type":"integer","minimum":1,"maximum":DEFAULT_TIMEOUT_MS},
                    "env":{"type":"object","additionalProperties":{"type":"string","maxLength":32768}}
                },
                "required":["command"],
                "additionalProperties":false
            }),
            permission_level: PermissionLevel::RunSafeCommands,
            side_effect: SideEffectKind::ProcessExecution,
            default_timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS + 1_000),
        }
    }

    /// auto-safe 按三档规则决定是否进入审批：allow 直接放行，ask 进入审批，
    /// deny 在执行阶段以 `UNSAFE_COMMAND` 拒绝故不弹审批；固定安全校验失败
    /// （ls 越界、只读 git 之外）同样在执行阶段拒绝。full-access 下 deny
    /// 仍生效，其余命令按既有逻辑（白名单豁免、其他需审批）。
    async fn needs_approval(&self, input: &Value, context: &ToolContext) -> bool {
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return true;
        };
        let args = parse_args(input);
        let env_overrides = parse_env(input);
        let requested_cwd = input
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_else(|| context.cwd.to_str().unwrap_or("."));
        let cwd = match resolve_directory(Path::new(requested_cwd), &context.cwd).await {
            Ok(path) => path,
            // 路径非法时执行阶段会直接失败，无需先打扰用户审批。
            Err(_) => return false,
        };
        if context.permission_mode != PermissionMode::FullAccess {
            match classify_auto_safe(command, &args, &cwd, &env_overrides, &context.command_rules)
                .await
            {
                Ok(CommandTier::Allow) => false,
                Ok(CommandTier::Ask) => true,
                // deny 与固定安全校验失败都会在执行阶段拒绝，不弹审批。
                Ok(CommandTier::Deny) | Err(_) => false,
            }
        } else {
            match classify_command(command, &args, &context.command_rules) {
                // deny 在执行阶段拒绝；allow 豁免审批；ask 需要审批。
                CommandTier::Deny => false,
                CommandTier::Allow => false,
                CommandTier::Ask => true,
            }
        }
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let map = object(input)?;
        let mut issues = Vec::new();
        reject_unknown_fields(
            map,
            &["command", "args", "cwd", "timeoutMs", "env"],
            &mut issues,
        );
        match required_string(map, "command", 128, &mut issues) {
            Some(command) if !valid_command(command) => issues
                .push("command 只能是单个可执行文件名，不能包含路径、空格或 shell 元字符。".into()),
            _ => {}
        }
        if let Some(args) = map.get("args") {
            match args.as_array() {
                Some(args)
                    if args.len() <= 128
                        && args
                            .iter()
                            .all(|arg| arg.as_str().is_some_and(|value| value.len() <= 4096)) => {}
                _ => {
                    issues.push("args 必须是最多 128 项、单项不超过 4096 字节的字符串数组。".into())
                }
            }
        }
        if let Some(cwd) = map.get("cwd")
            && !cwd
                .as_str()
                .is_some_and(|value| !value.is_empty() && value.len() <= 4096)
        {
            issues.push("cwd 必须是 1 到 4096 字节的字符串。".into());
        }
        if let Some(timeout) = map.get("timeoutMs")
            && !timeout
                .as_u64()
                .is_some_and(|value| (1..=DEFAULT_TIMEOUT_MS).contains(&value))
        {
            issues.push(format!("timeoutMs 必须在 1 到 {DEFAULT_TIMEOUT_MS} 之间。"));
        }
        if let Some(env) = map.get("env") {
            match env.as_object() {
                Some(env)
                    if env
                        .values()
                        .all(|value| value.as_str().is_some_and(|value| value.len() <= 32_768)) => {
                }
                _ => issues.push("env 必须是字符串键值对象，值不超过 32768 字节。".into()),
            }
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        let command = input["command"].as_str().unwrap();
        let args = parse_args(&input);
        let requested_cwd = input
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_else(|| context.cwd.to_str().unwrap_or("."));
        let cwd = match resolve_directory(Path::new(requested_cwd), &context.cwd).await {
            Ok(path) => path,
            Err(error) => {
                let code = if error.message.contains("超出工作区") {
                    "PATH_OUTSIDE_WORKSPACE"
                } else {
                    "INVALID_CWD"
                };
                return ToolResult::failure(
                    code,
                    error.message,
                    context.started_at,
                    json!({"cwd":requested_cwd}),
                );
            }
        };
        let env_overrides = parse_env(&input);
        // deny 规则在全部权限模式下生效。
        if classify_command(command, &args, &context.command_rules) == CommandTier::Deny {
            return ToolResult::failure(
                "UNSAFE_COMMAND",
                format!("命令“{command}”命中 deny 规则，已拒绝。"),
                context.started_at,
                json!({"command":command,"args":args,"permissionMode":context.permission_mode.as_str()}),
            );
        }
        if context.permission_mode != PermissionMode::FullAccess {
            if let Err(reason) =
                classify_auto_safe(command, &args, &cwd, &env_overrides, &context.command_rules)
                    .await
            {
                return ToolResult::failure(
                    "UNSAFE_COMMAND",
                    reason,
                    context.started_at,
                    json!({"command":command,"args":args,"permissionMode":context.permission_mode.as_str()}),
                );
            }
        }
        // `pwd`、`echo` 与 `ls` 在不同平台可能只是 shell 内建命令。这里使用
        // 受限 Rust 实现，保证三平台行为一致，同时继续坚持“不启动 shell”。
        match command {
            "pwd" if args.is_empty() => {
                return portable_success(
                    command,
                    &args,
                    &cwd,
                    format!("{}\n", cwd.display()),
                    &context,
                );
            }
            "pwd" => {
                return ToolResult::failure(
                    "INVALID_TOOL_INPUT",
                    "pwd 不接受参数。",
                    context.started_at,
                    json!({"command":command,"args":args}),
                );
            }
            "echo" => {
                return portable_success(
                    command,
                    &args,
                    &cwd,
                    format!("{}\n", args.join(" ")),
                    &context,
                );
            }
            "ls" => match portable_ls(&args, &cwd).await {
                Ok(stdout) => return portable_success(command, &args, &cwd, stdout, &context),
                Err(message) => {
                    return ToolResult::failure(
                        "INVALID_LS_PATH",
                        message,
                        context.started_at,
                        json!({"command":command,"args":args}),
                    );
                }
            },
            _ => {}
        }
        let executable = if context.permission_mode == PermissionMode::FullAccess {
            PathBuf::from(command)
        } else {
            match resolve_safe_executable(command, &context.cwd).await {
                Some(path) => path,
                None => {
                    return ToolResult::failure(
                        "SAFE_EXECUTABLE_NOT_FOUND",
                        format!("无法在工作区外的可信 PATH 中找到命令“{command}”。"),
                        context.started_at,
                        json!({"command":command}),
                    );
                }
            }
        };
        let timeout_ms = input
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let mut process = Command::new(executable);
        process
            .args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_remove("GIT_EXTERNAL_DIFF")
            .env_remove("GIT_CONFIG")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GIT_CONFIG_SYSTEM")
            .envs(&env_overrides);
        let mut child = match process.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ToolResult::failure(
                    "COMMAND_SPAWN_ERROR",
                    error.to_string(),
                    context.started_at,
                    json!({"command":command}),
                );
            }
        };
        let stdout_task = child
            .stdout
            .take()
            .map(|stdout| tokio::spawn(read_limited(stdout)));
        let stderr_task = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(read_limited(stderr)));
        enum Outcome {
            Status(std::process::ExitStatus),
            Timeout,
            Aborted,
            WaitError(String),
        }
        let outcome = tokio::select! {
            _ = context.cancellation.cancelled() => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Outcome::Aborted
            }
            _ = time::sleep(Duration::from_millis(timeout_ms)) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Outcome::Timeout
            }
            status = child.wait() => match status {
                Ok(status) => Outcome::Status(status),
                Err(error) => Outcome::WaitError(error.to_string()),
            }
        };
        let (stdout, stdout_truncated) = match stdout_task {
            Some(task) => task.await.unwrap_or_default(),
            None => Default::default(),
        };
        let (stderr, stderr_truncated) = match stderr_task {
            Some(task) => task.await.unwrap_or_default(),
            None => Default::default(),
        };
        let truncated = stdout_truncated || stderr_truncated || matches!(outcome, Outcome::Timeout);
        let summary = stdout
            .lines()
            .chain(stderr.lines())
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let (exit_code, signal, error) = match outcome {
            Outcome::Status(status) => {
                #[cfg(unix)]
                let signal = {
                    use std::os::unix::process::ExitStatusExt;
                    status.signal().map(|signal| signal.to_string())
                };
                #[cfg(not(unix))]
                let signal: Option<String> = None;
                let error = if status.success() {
                    None
                } else {
                    Some((
                        "NONZERO_EXIT",
                        format!("命令退出码为 {:?}", status.code()),
                        false,
                    ))
                };
                (status.code(), signal, error)
            }
            Outcome::Timeout => (
                None,
                Some("TIMEOUT".into()),
                Some((
                    "COMMAND_TIMEOUT",
                    format!("命令在 {timeout_ms}ms 后超时"),
                    true,
                )),
            ),
            Outcome::Aborted => (
                None,
                Some("ABORTED".into()),
                Some(("COMMAND_ABORTED", "命令已中断".into(), false)),
            ),
            Outcome::WaitError(message) => {
                (None, None, Some(("COMMAND_WAIT_ERROR", message, false)))
            }
        };
        let mut result = if let Some((code, message, retryable)) = error {
            let mut result = ToolResult::failure(
                code,
                message,
                context.started_at,
                json!({"command":command,"exitCode":exit_code,"signal":signal,"timeoutMs":timeout_ms}),
            );
            result.error.as_mut().unwrap().retryable = retryable;
            result.output = Some(json!({
                "exitCode":exit_code,"signal":signal,"stdout":stdout,"stderr":stderr,
                "outputSummary":summary,"truncated":truncated
            }));
            result
        } else {
            ToolResult::success(
                json!({"exitCode":exit_code,"signal":signal,"stdout":stdout,"stderr":stderr,"outputSummary":summary,"truncated":truncated}),
                context.started_at,
                json!({"command":command,"args":args,"cwd":cwd}),
            )
        };
        result.metadata =
            json!({"command":command,"args":args,"exitCode":exit_code,"signal":signal,"cwd":cwd});
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_拒绝_shell_语法() {
        assert!(valid_command("git"));
        assert!(!valid_command("git status"));
        assert!(!valid_command("sh;rm"));
        assert!(!valid_command("/bin/ls"));
    }

    #[test]
    fn git_仅允许只读子命令() {
        assert!(check_git_args(&["status".into(), "--short".into()]).is_ok());
        assert!(check_git_args(&["commit".into()]).is_err());
        assert!(check_git_args(&["diff".into(), "--output=x".into()]).is_err());
        assert!(check_git_args(&["branch".into(), "-l".into()]).is_ok());
        assert!(check_git_args(&["branch".into(), "-d".into()]).is_err());
        assert!(check_git_args(&["stash".into(), "list".into()]).is_ok());
        assert!(check_git_args(&["stash".into(), "push".into()]).is_err());
    }

    #[test]
    fn 三档规则按_deny_allow_ask_顺序匹配() {
        let rules = CommandRules {
            allow: ["cargo check".into(), "echo".into()].into_iter().collect(),
            ask: ["cargo install".into()].into_iter().collect(),
            deny: ["sudo".into(), "rm".into()].into_iter().collect(),
        };
        let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();

        // deny 优先：sudo 即使不在 allow 也拒绝；rm 全名匹配拒绝。
        assert_eq!(
            classify_command("sudo", &args(&[]), &rules),
            CommandTier::Deny
        );
        assert_eq!(
            classify_command("rm", &args(&["-rf", "x"]), &rules),
            CommandTier::Deny
        );
        // allow：完整可执行名 + 首个参数匹配。
        assert_eq!(
            classify_command("cargo", &args(&["check"]), &rules),
            CommandTier::Allow
        );
        assert_eq!(
            classify_command("echo", &args(&["hi"]), &rules),
            CommandTier::Allow
        );
        // ask：未匹配 allow 但命中 ask 前缀。
        assert_eq!(
            classify_command("cargo", &args(&["install"]), &rules),
            CommandTier::Ask
        );
        // 未匹配任何规则 → ask。
        assert_eq!(
            classify_command("cargo", &args(&["publish"]), &rules),
            CommandTier::Ask
        );
        assert_eq!(
            classify_command("python3", &args(&["-m", "pytest"]), &rules),
            CommandTier::Ask
        );
        // 可执行名不同不匹配。
        assert_eq!(
            classify_command("git", &args(&["status"]), &rules),
            CommandTier::Ask
        );
    }
}
