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
    permission::{PermissionLevel, PermissionMode},
};

use super::path_policy::{resolve_directory, resolve_existing};
use super::{
    Tool, ToolContext, ToolDefinition, ToolResult, object, reject_unknown_fields, required_string,
};

const MAX_OUTPUT_LENGTH: usize = 100_000;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

pub struct TerminalExecTool;

fn valid_command(command: &str) -> bool {
    !command.is_empty()
        && command.len() <= 128
        && command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
}

fn check_git_args(args: &[String]) -> Result<(), String> {
    let Some(subcommand) = args.first() else {
        return Err("auto-safe 只允许 git status、diff、log 和 show。".into());
    };
    if !["status", "diff", "log", "show"].contains(&subcommand.as_str()) {
        return Err("auto-safe 只允许 git status、diff、log 和 show。".into());
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

async fn check_auto_safe(
    command: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Result<(), String> {
    if !env.is_empty() {
        return Err("auto-safe 不允许覆盖环境变量。".into());
    }
    match command {
        "pwd" if args.is_empty() => Ok(()),
        "pwd" => Err("pwd 不接受参数。".into()),
        "echo" => Ok(()),
        "ls" => {
            for arg in args.iter().filter(|arg| !arg.starts_with('-')) {
                resolve_existing(Path::new(arg), cwd)
                    .await
                    .map_err(|_| format!("ls 路径不在工作区内或不存在：{arg}"))?;
            }
            Ok(())
        }
        "git" => check_git_args(args),
        _ => Err(format!("命令“{command}”不在 auto-safe 白名单中。")),
    }
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
            description: "运行单个可执行文件并返回 stdout、stderr 和退出码。所有参数必须放入 args；auto-safe 仅允许 pwd、echo、ls 和只读 git 子命令。".into(),
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

    /// auto-safe 白名单命令（pwd/echo/ls/只读 git）豁免审批，对齐 Claude Code：
    /// 安全命令直接放行，危险命令才询问。full-access 下白名单同样豁免；
    /// 白名单外命令仍需审批。auto-safe 下白名单外会在执行阶段以
    /// `UNSAFE_COMMAND` 拒绝，故也不弹审批。
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
        match check_auto_safe(command, &args, &cwd, &env_overrides).await {
            Ok(()) => false,
            Err(_) => context.permission_mode == PermissionMode::FullAccess,
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
        if context.permission_mode != PermissionMode::FullAccess {
            if let Err(reason) = check_auto_safe(command, &args, &cwd, &env_overrides).await {
                return ToolResult::failure(
                    "UNSAFE_COMMAND",
                    reason,
                    context.started_at,
                    json!({"command":command,"args":args,"permissionMode":context.permission_mode.as_str()}),
                );
            }
        }
        // `pwd` 在 Windows 上不是独立的可执行文件。直接返回已经过工作区边界
        // 校验的目录，既保证跨平台行为一致，也避免为内建命令启动 shell。
        if command == "pwd" && args.is_empty() {
            let stdout = format!("{}\n", cwd.display());
            return ToolResult::success(
                json!({
                    "exitCode":0,
                    "signal":null,
                    "stdout":stdout,
                    "stderr":"",
                    "outputSummary":cwd.display().to_string(),
                    "truncated":false
                }),
                context.started_at,
                json!({"command":command,"args":args,"exitCode":0,"signal":null,"cwd":cwd}),
            );
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
    }
}
