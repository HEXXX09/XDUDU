use std::{fs, sync::Arc};

use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xdudu_core::{AllowAllApprovalGate, PermissionMode, ToolRegistry, register_builtins};

async fn execute(
    registry: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
    cwd: &std::path::Path,
    mode: PermissionMode,
) -> xdudu_core::tools::ToolResult {
    registry
        .execute(
            name,
            input,
            Uuid::new_v4(),
            cwd,
            mode,
            CancellationToken::new(),
        )
        .await
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::with_approval_gate(Arc::new(AllowAllApprovalGate));
    register_builtins(&mut registry).unwrap();
    registry
}

#[tokio::test]
async fn 默认审批策略拒绝副作用工具() {
    let dir = tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry).unwrap();
    let result = execute(
        &registry,
        "file_write",
        json!({"path":"blocked.txt","content":"blocked"}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(result.error.unwrap().code, "APPROVAL_DENIED");
    assert!(!dir.path().join("blocked.txt").exists());
}

#[test]
fn 内置工具定义完整() {
    let definitions = registry().definitions();
    assert_eq!(definitions.len(), 3);
    assert_eq!(
        definitions.iter().map(|item| item.name).collect::<Vec<_>>(),
        ["file_read", "file_write", "terminal_exec"]
    );
}

#[tokio::test]
async fn 读取文件返回范围和哈希() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "一\n二\n三\n四").unwrap();
    let result = execute(
        &registry(),
        "file_read",
        json!({"path":"a.txt","startLine":2,"endLine":3}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert!(result.success);
    let output = result.output.unwrap();
    assert_eq!(output["content"], "二\n三");
    assert_eq!(output["sha256"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn 读取拒绝未知字段和错误行号() {
    let dir = tempdir().unwrap();
    let registry = registry();
    let unknown = execute(
        &registry,
        "file_read",
        json!({"path":"a.txt","extra":true}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(unknown.error.unwrap().code, "INVALID_TOOL_INPUT");
    let range = execute(
        &registry,
        "file_read",
        json!({"path":"a.txt","startLine":3,"endLine":2}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(range.error.unwrap().code, "INVALID_TOOL_INPUT");
}

#[tokio::test]
async fn 读写拒绝父目录逃逸() {
    let dir = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    fs::write(&secret, "secret").unwrap();
    let registry = registry();
    let read = execute(
        &registry,
        "file_read",
        json!({"path":secret}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(read.error.unwrap().code, "PATH_OUTSIDE_WORKSPACE");
    let write = execute(
        &registry,
        "file_write",
        json!({"path":"../escaped.txt","content":"blocked"}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(write.error.unwrap().code, "PATH_OUTSIDE_WORKSPACE");
}

#[cfg(unix)]
#[tokio::test]
async fn 读写拒绝符号链接逃逸() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        dir.path().join("read-link"),
    )
    .unwrap();
    symlink(outside.path(), dir.path().join("write-link")).unwrap();
    let registry = registry();
    let read = execute(
        &registry,
        "file_read",
        json!({"path":"read-link"}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(read.error.unwrap().code, "PATH_OUTSIDE_WORKSPACE");
    let write = execute(
        &registry,
        "file_write",
        json!({"path":"write-link/new.txt","content":"blocked"}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(write.error.unwrap().code, "PATH_OUTSIDE_WORKSPACE");
}

#[tokio::test]
async fn 写入创建文件并返回差异() {
    let dir = tempdir().unwrap();
    let result = execute(
        &registry(),
        "file_write",
        json!({"path":"nested/a.txt","content":"hello"}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert!(result.success);
    let output = result.output.unwrap();
    assert_eq!(output["created"], true);
    assert!(output["unifiedDiff"].as_str().unwrap().contains("+hello"));
    assert_eq!(
        fs::read_to_string(dir.path().join("nested/a.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn 写入哈希冲突不覆盖原文件() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "original").unwrap();
    let result = execute(
        &registry(),
        "file_write",
        json!({
            "path":"a.txt",
            "content":"changed",
            "expectedSha256":"0000000000000000000000000000000000000000000000000000000000000000"
        }),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    let error = result.error.unwrap();
    assert_eq!(error.code, "HASH_MISMATCH", "{}", error.message);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "original"
    );
}

#[tokio::test]
async fn 写入可禁止创建缺失文件() {
    let dir = tempdir().unwrap();
    let result = execute(
        &registry(),
        "file_write",
        json!({"path":"missing.txt","content":"x","createIfMissing":false}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(result.error.unwrap().code, "FILE_NOT_FOUND");
}

#[tokio::test]
async fn auto_safe_允许_pwd_并拒绝任意命令() {
    let dir = tempdir().unwrap();
    let registry = registry();
    let pwd = execute(
        &registry,
        "terminal_exec",
        json!({"command":"pwd"}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert!(pwd.success);
    let stdout = pwd.output.unwrap()["stdout"]
        .as_str()
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(
        fs::canonicalize(stdout).unwrap(),
        fs::canonicalize(dir.path()).unwrap()
    );

    let node = execute(
        &registry,
        "terminal_exec",
        json!({"command":"node","args":["--version"]}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(node.error.unwrap().code, "UNSAFE_COMMAND");
}

#[tokio::test]
async fn auto_safe_拒绝写入型_git_和环境覆盖() {
    let dir = tempdir().unwrap();
    let registry = registry();
    for input in [
        json!({"command":"git","args":["commit","-m","x"]}),
        json!({"command":"echo","args":["x"],"env":{"TOKEN":"secret"}}),
    ] {
        let result = execute(
            &registry,
            "terminal_exec",
            input,
            dir.path(),
            PermissionMode::AutoSafe,
        )
        .await;
        assert_eq!(result.error.unwrap().code, "UNSAFE_COMMAND");
    }
}

#[tokio::test]
async fn terminal_拒绝_shell_拼接和工作目录逃逸() {
    let dir = tempdir().unwrap();
    let registry = registry();
    let shell = execute(
        &registry,
        "terminal_exec",
        json!({"command":"echo hello; rm -rf x"}),
        dir.path(),
        PermissionMode::FullAccess,
    )
    .await;
    assert_eq!(shell.error.unwrap().code, "INVALID_TOOL_INPUT");
    let cwd = execute(
        &registry,
        "terminal_exec",
        json!({"command":"pwd","cwd":".."}),
        dir.path(),
        PermissionMode::FullAccess,
    )
    .await;
    assert_eq!(cwd.error.unwrap().code, "PATH_OUTSIDE_WORKSPACE");
}

#[tokio::test]
async fn full_access_仍然不经过_shell() {
    let dir = tempdir().unwrap();
    let marker = dir.path().join("should-not-exist");
    let result = execute(
        &registry(),
        "terminal_exec",
        json!({"command":"echo","args":[format!("hello;touch {}", marker.display())]}),
        dir.path(),
        PermissionMode::FullAccess,
    )
    .await;
    assert!(result.success);
    assert!(!marker.exists());
}
