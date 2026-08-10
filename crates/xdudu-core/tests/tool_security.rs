use std::{fs, sync::Arc};

use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xdudu_core::{
    AllowAllApprovalGate, JsonChangeLedger, PermissionMode, ToolRegistry, WebReadTool,
    register_builtins,
};

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
async fn web_read_拒绝回环地址且不会发起请求() {
    let dir = tempdir().unwrap();
    let mut registry = ToolRegistry::with_approval_gate(Arc::new(AllowAllApprovalGate));
    registry
        .register(WebReadTool::new(None, "test-model".into(), 0.0, 128, false))
        .unwrap();
    let result = execute(
        &registry,
        "web_read",
        json!({"url":"https://127.0.0.1/private","goal":"读取内容"}),
        dir.path(),
        PermissionMode::FullAccess,
    )
    .await;
    assert_eq!(result.error.unwrap().code, "WEB_BLOCKED");
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

#[tokio::test]
async fn auto_safe_白名单命令豁免审批() {
    // 默认 DenyAll：若仍按 ProcessExecution 一律审批，pwd/echo/ls 会被拒。
    let dir = tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry).unwrap();
    for input in [
        json!({"command": "pwd"}),
        json!({"command": "echo", "args": ["hello"]}),
        json!({"command": "ls"}),
    ] {
        let result = execute(
            &registry,
            "terminal_exec",
            input.clone(),
            dir.path(),
            PermissionMode::AutoSafe,
        )
        .await;
        assert!(
            result.success,
            "白名单命令应豁免审批并可执行：input={input:?} error={:?}",
            result.error
        );
        assert!(
            result.approval.is_none(),
            "豁免审批时不应写入审批记录：input={input:?}"
        );
    }
}

#[tokio::test]
async fn portable_内建命令不依赖系统shell() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("visible.txt"), "visible").unwrap();
    fs::write(dir.path().join(".hidden.txt"), "hidden").unwrap();
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry).unwrap();

    let echo = execute(
        &registry,
        "terminal_exec",
        json!({"command":"echo","args":["hello","XDUDU"]}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert!(echo.success);
    assert_eq!(echo.output.as_ref().unwrap()["stdout"], "hello XDUDU\n");

    let listing = execute(
        &registry,
        "terminal_exec",
        json!({"command":"ls"}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert!(listing.success);
    let stdout = listing.output.as_ref().unwrap()["stdout"].as_str().unwrap();
    assert!(stdout.contains("visible.txt"));
    assert!(!stdout.contains(".hidden.txt"));

    let listing_all = execute(
        &registry,
        "terminal_exec",
        json!({"command":"ls","args":["-a"]}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert!(listing_all.success);
    assert!(
        listing_all.output.as_ref().unwrap()["stdout"]
            .as_str()
            .unwrap()
            .contains(".hidden.txt")
    );

    let unsupported = execute(
        &registry,
        "terminal_exec",
        json!({"command":"ls","args":["--color=always"]}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(unsupported.error.unwrap().code, "UNSAFE_COMMAND");
    assert!(unsupported.approval.is_none());
}

#[tokio::test]
async fn auto_safe_非白名单命令进入审批门() {
    let dir = tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry).unwrap();
    let result = execute(
        &registry,
        "terminal_exec",
        json!({"command": "node", "args": ["--version"]}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    // 三档策略下未匹配命令属于 ask 档：进入审批门；DenyAll 默认拒绝。
    assert_eq!(result.error.unwrap().code, "APPROVAL_DENIED");
    assert!(result.approval.is_some());
}

#[tokio::test]
async fn auto_safe_命中deny规则立即拒绝() {
    let dir = tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry).unwrap();
    let result = execute(
        &registry,
        "terminal_exec",
        json!({"command": "sudo", "args": ["apt", "update"]}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    // deny 档：在执行阶段拒绝，不弹审批。
    assert_eq!(result.error.unwrap().code, "UNSAFE_COMMAND");
    assert!(result.approval.is_none());
}

#[tokio::test]
async fn full_access_非白名单命令仍需审批() {
    let dir = tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry).unwrap();
    let result = execute(
        &registry,
        "terminal_exec",
        // `true` 不在 auto-safe 白名单；full-access 下会真正启动进程，故仍需审批。
        json!({"command": "true"}),
        dir.path(),
        PermissionMode::FullAccess,
    )
    .await;
    assert_eq!(result.error.unwrap().code, "APPROVAL_DENIED");
}

#[tokio::test]
async fn full_access_白名单命令同样豁免审批() {
    let dir = tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry).unwrap();
    let result = execute(
        &registry,
        "terminal_exec",
        json!({"command": "pwd"}),
        dir.path(),
        PermissionMode::FullAccess,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert!(result.approval.is_none());
}

#[test]
fn 内置工具定义完整() {
    let definitions = registry().definitions();
    assert_eq!(definitions.len(), 9);
    assert_eq!(
        definitions
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        [
            "apply_patch",
            "file_read",
            "file_write",
            "git_diff",
            "git_status",
            "search_text",
            "terminal_exec",
            "web_fetch",
            "web_search"
        ]
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

    let mut deny_registry = ToolRegistry::new();
    register_builtins(&mut deny_registry).unwrap();
    let node = execute(
        &deny_registry,
        "terminal_exec",
        json!({"command":"node","args":["--version"]}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    // 未匹配命令为 ask 档，DenyAll 审批拒绝（不依赖外部可执行文件）。
    assert_eq!(node.error.unwrap().code, "APPROVAL_DENIED");
    assert!(node.approval.is_some());
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

#[tokio::test]
async fn search_text_遵守忽略规则并返回_unicode_列号() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    fs::create_dir_all(dir.path().join("target")).unwrap();
    fs::write(
        dir.path().join(".github/workflows/ci.yml"),
        "步骤：ToolRegistry\n",
    )
    .unwrap();
    fs::write(dir.path().join("target/ignored.txt"), "ToolRegistry\n").unwrap();
    let result = execute(
        &registry(),
        "search_text",
        json!({"query":"ToolRegistry","path":".","mode":"literal"}),
        dir.path(),
        PermissionMode::ReadOnly,
    )
    .await;
    assert!(result.success);
    let output = result.output.unwrap();
    assert_eq!(output["matches"].as_array().unwrap().len(), 1);
    assert_eq!(output["matches"][0]["path"], ".github/workflows/ci.yml");
    assert_eq!(output["matches"][0]["column"], 4);
    assert!(output["nextActionHint"].is_null());

    let empty = execute(
        &registry(),
        "search_text",
        json!({"query":"不存在的外部知识"}),
        dir.path(),
        PermissionMode::ReadOnly,
    )
    .await;
    let empty_output = empty.output.unwrap();
    assert!(empty_output["matches"].as_array().unwrap().is_empty());
    assert!(
        empty_output["nextActionHint"]
            .as_str()
            .unwrap()
            .contains("web_search")
    );
}

#[tokio::test]
async fn git_专用工具返回结构化状态和暂存差异() {
    let dir = tempdir().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let status_result = execute(
        &registry(),
        "git_status",
        json!({}),
        dir.path(),
        PermissionMode::ReadOnly,
    )
    .await;
    assert!(status_result.success);
    assert_eq!(
        status_result.output.unwrap()["entries"][0]["kind"],
        "untracked"
    );
    assert!(
        std::process::Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let diff = execute(
        &registry(),
        "git_diff",
        json!({"scope":"staged"}),
        dir.path(),
        PermissionMode::ReadOnly,
    )
    .await;
    assert!(diff.success);
    assert!(
        diff.output.unwrap()["diff"]
            .as_str()
            .unwrap()
            .contains("+hello")
    );
}

#[tokio::test]
async fn apply_patch_多文件事务可以整批撤销() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "old\n").unwrap();
    let mut registry = ToolRegistry::with_runtime(
        Arc::new(AllowAllApprovalGate),
        Arc::new(JsonChangeLedger::new(dir.path())),
    );
    register_builtins(&mut registry).unwrap();
    let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old
+new
--- /dev/null
+++ b/b.txt
@@ -0,0 +1 @@
+created
";
    let result = execute(
        &registry,
        "apply_patch",
        json!({"patch":patch}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "created\n"
    );
    let transaction = result.output.unwrap()["transactionId"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    JsonChangeLedger::new(dir.path())
        .undo(Some(transaction), None)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "old\n"
    );
    assert!(!dir.path().join("b.txt").exists());
}

#[tokio::test]
async fn apply_patch_上下文错误在审批前完成预检() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "actual\n").unwrap();
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry).unwrap();
    let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-expected
+new
";
    let result = execute(
        &registry,
        "apply_patch",
        json!({"patch":patch}),
        dir.path(),
        PermissionMode::AutoSafe,
    )
    .await;
    assert_eq!(result.error.unwrap().code, "PATCH_CONTEXT_MISMATCH");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "actual\n"
    );
}

#[tokio::test]
async fn web_fetch_所有权限模式都必须经过网络审批() {
    let dir = tempdir().unwrap();
    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::AutoSafe,
        PermissionMode::FullAccess,
    ] {
        let mut denied_registry = ToolRegistry::new();
        register_builtins(&mut denied_registry).unwrap();
        let denied = execute(
            &denied_registry,
            "web_fetch",
            json!({"url":"https://example.com"}),
            dir.path(),
            mode,
        )
        .await;
        assert_eq!(denied.error.unwrap().code, "APPROVAL_DENIED");
    }
}

#[tokio::test]
async fn web_search_所有权限模式都必须经过网络审批() {
    let dir = tempdir().unwrap();
    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::AutoSafe,
        PermissionMode::FullAccess,
    ] {
        let mut denied_registry = ToolRegistry::new();
        register_builtins(&mut denied_registry).unwrap();
        let denied = execute(
            &denied_registry,
            "web_search",
            json!({"query":"Rust programming language"}),
            dir.path(),
            mode,
        )
        .await;
        assert_eq!(denied.error.unwrap().code, "APPROVAL_DENIED");
    }
}
