use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
    time::Duration,
};

use serde_json::json;
use tempfile::tempdir;

fn xdudu() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xdudu"));
    command.env("XDUDU_PROVIDER", "anthropic");
    command
}

fn read_http_request(stream: &mut std::net::TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut expected_length = None;
    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header_end = header_end + 4;
            if expected_length.is_none() {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                expected_length = headers.lines().find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")?
                        .trim()
                        .parse::<usize>()
                        .ok()
                });
            }
            if request.len() >= header_end + expected_length.unwrap_or(0) {
                break;
            }
        }
    }
}

fn write_http_json(stream: &mut std::net::TcpStream, body: serde_json::Value) {
    let body = serde_json::to_string(&body).unwrap();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
}

fn write_http_sse(stream: &mut std::net::TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
}

fn two_turn_anthropic_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        read_http_request(&mut first);
        write_http_json(
            &mut first,
            json!({
                "content":[{"type":"tool_use","id":"call-1","name":"file_read","input":{"path":"fixture.txt"}}],
                "stop_reason":"tool_use",
                "usage":{"input_tokens":1,"output_tokens":1}
            }),
        );
        let (mut second, _) = listener.accept().unwrap();
        read_http_request(&mut second);
        write_http_json(
            &mut second,
            json!({
                "content":[{"type":"text","text":"Rust CLI E2E 完成"}],
                "stop_reason":"end_turn",
                "usage":{"input_tokens":2,"output_tokens":2}
            }),
        );
    });
    format!("http://{address}")
}

fn two_turn_anthropic_sse_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        read_http_request(&mut first);
        write_http_sse(
            &mut first,
            concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-1\",\"name\":\"file_read\",\"input\":{}}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"fixture.txt\\\"}\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ),
        );
        let (mut second, _) = listener.accept().unwrap();
        read_http_request(&mut second);
        write_http_sse(
            &mut second,
            concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2}}}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Rust CLI \"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"SSE 完成\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ),
        );
    });
    format!("http://{address}")
}

fn two_turn_anthropic_write_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        read_http_request(&mut first);
        write_http_json(
            &mut first,
            json!({
                "content":[{
                    "type":"tool_use",
                    "id":"write-1",
                    "name":"file_write",
                    "input":{"path":"created.txt","content":"approval test"}
                }],
                "stop_reason":"tool_use",
                "usage":{"input_tokens":1,"output_tokens":1}
            }),
        );
        let (mut second, _) = listener.accept().unwrap();
        read_http_request(&mut second);
        write_http_json(
            &mut second,
            json!({
                "content":[{"type":"text","text":"写入流程结束"}],
                "stop_reason":"end_turn",
                "usage":{"input_tokens":2,"output_tokens":2}
            }),
        );
    });
    format!("http://{address}")
}

#[test]
fn help_可以在没有_api_key_时运行() {
    let output = xdudu().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("终端原生 AI 编程助手"));
    assert!(stdout.contains("--permission"));
    assert!(stdout.contains("--approval"));
    assert!(stdout.contains("undo"));
}

#[test]
fn 非法权限模式返回参数退出码() {
    let output = xdudu()
        .args(["--permission", "unknown", "test"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("非法权限模式"));
}

#[test]
fn 未知_provider_返回参数退出码() {
    let output = xdudu()
        .args(["--provider", "unknown", "test"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("不支持的 Provider"));
}

#[test]
fn 缺少_api_key_返回配置退出码和登录指引() {
    let output = xdudu()
        .env("XDUDU_PROVIDER", "anthropic")
        .env_remove("ANTHROPIC_API_KEY")
        .arg("test")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ANTHROPIC_API_KEY"));
    assert!(stderr.contains("xdudu auth login anthropic"));
}

#[test]
fn config_命令无需_api_key_并展示来源() {
    let dir = tempdir().unwrap();
    let output = xdudu()
        .current_dir(dir.path())
        .env_remove("ANTHROPIC_API_KEY")
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"provider\""));
    assert!(stdout.contains("\"sources\""));
    assert!(!stdout.contains("API_KEY"));
}

#[test]
fn config_set_写入项目配置并可解释来源() {
    let dir = tempdir().unwrap();
    let set = xdudu()
        .current_dir(dir.path())
        .args(["config", "set", "agent.max_turns", "31"])
        .output()
        .unwrap();
    assert!(set.status.success());
    let explain = xdudu()
        .current_dir(dir.path())
        .args(["config", "explain", "agent.max_turns"])
        .output()
        .unwrap();
    assert!(explain.status.success());
    let stdout = String::from_utf8_lossy(&explain.stdout);
    assert!(stdout.contains("31"));
    assert!(stdout.contains("project"));
}

#[test]
fn 项目配置不能自动批准副作用() {
    let dir = tempdir().unwrap();
    let output = xdudu()
        .current_dir(dir.path())
        .args(["config", "set", "agent.approval", "always"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("只能把 agent.approval 收紧"));
}

#[test]
fn doctor_无需访问模型即可输出诊断() {
    let dir = tempdir().unwrap();
    let output = xdudu()
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert!(report["checks"].as_array().unwrap().len() >= 4);
}

#[test]
fn rust_cli_真实进程完成工具循环并保存会话() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("fixture.txt"), "fixture content").unwrap();
    let base_url = two_turn_anthropic_server();
    let output = xdudu()
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("XDUDU_NO_STREAM", "true")
        .arg("读取 fixture.txt")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Rust CLI E2E 完成"));

    let sessions_dir = dir.path().join(".xdudu/sessions/json");
    let session_file = fs::read_dir(sessions_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let session: serde_json::Value =
        serde_json::from_slice(&fs::read(session_file).unwrap()).unwrap();
    assert_eq!(session["status"], "completed");
    assert_eq!(session["toolCalls"][0]["toolName"], "file_read");
    assert_eq!(session["toolCalls"][0]["status"], "succeeded");
}

#[test]
fn json_模式只输出可解析事件且没有横幅() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("fixture.txt"), "fixture content").unwrap();
    let output = xdudu()
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", two_turn_anthropic_server())
        .args(["--json", "--no-stream", "读取 fixture.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("XDUDU v"));
    let events = stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(events.iter().any(|event| event["type"] == "tool_started"));
    assert!(events.iter().any(|event| event["type"] == "run_completed"));
}

#[test]
fn no_stream_只在完成时打印最终文本() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("fixture.txt"), "fixture content").unwrap();
    let output = xdudu()
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", two_turn_anthropic_server())
        .args(["--no-stream", "读取 fixture.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("Rust CLI E2E 完成").count(), 1);
}

#[test]
fn 默认流式模式聚合工具参数并增量输出文本() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("fixture.txt"), "fixture content").unwrap();
    let output = xdudu()
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", two_turn_anthropic_sse_server())
        .arg("读取 fixture.txt")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("Rust CLI SSE 完成").count(), 1);
}

#[test]
fn 非交互默认拒绝写入且显式批准可以执行() {
    let denied_dir = tempdir().unwrap();
    let denied = xdudu()
        .current_dir(denied_dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", two_turn_anthropic_write_server())
        .args(["--no-stream", "创建 created.txt"])
        .output()
        .unwrap();
    assert!(denied.status.success());
    assert!(!denied_dir.path().join("created.txt").exists());
    let denied_session = fs::read_dir(denied_dir.path().join(".xdudu/sessions/json"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let denied_session: serde_json::Value =
        serde_json::from_slice(&fs::read(denied_session).unwrap()).unwrap();
    assert_eq!(denied_session["toolCalls"][0]["status"], "denied");
    assert_eq!(
        denied_session["toolCalls"][0]["approval"]["approved"],
        false
    );

    let allowed_dir = tempdir().unwrap();
    let allowed = xdudu()
        .current_dir(allowed_dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", two_turn_anthropic_write_server())
        .args(["--no-stream", "--approval", "always", "创建 created.txt"])
        .output()
        .unwrap();
    assert!(allowed.status.success());
    assert_eq!(
        fs::read_to_string(allowed_dir.path().join("created.txt")).unwrap(),
        "approval test"
    );
    assert_eq!(
        fs::read_dir(allowed_dir.path().join(".xdudu/changes/json"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn undo_无需_api_key_即可撤销_agent_创建的文件() {
    let dir = tempdir().unwrap();
    let write = xdudu()
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", two_turn_anthropic_write_server())
        .args(["--no-stream", "--approval", "always", "创建 created.txt"])
        .output()
        .unwrap();
    assert!(write.status.success());
    assert!(dir.path().join("created.txt").exists());

    let undo = xdudu()
        .current_dir(dir.path())
        .env_remove("ANTHROPIC_API_KEY")
        .arg("undo")
        .output()
        .unwrap();
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    assert!(!dir.path().join("created.txt").exists());
    assert!(String::from_utf8_lossy(&undo.stdout).contains("已删除由 Agent 创建的文件"));
}

#[test]
fn undo_发现后续人工修改时拒绝覆盖() {
    let dir = tempdir().unwrap();
    let write = xdudu()
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", two_turn_anthropic_write_server())
        .args(["--no-stream", "--approval", "always", "创建 created.txt"])
        .output()
        .unwrap();
    assert!(write.status.success());
    fs::write(dir.path().join("created.txt"), "user edit").unwrap();

    let undo = xdudu()
        .current_dir(dir.path())
        .env_remove("ANTHROPIC_API_KEY")
        .arg("undo")
        .output()
        .unwrap();
    assert!(!undo.status.success());
    assert!(String::from_utf8_lossy(&undo.stderr).contains("又发生变化"));
    assert_eq!(
        fs::read_to_string(dir.path().join("created.txt")).unwrap(),
        "user edit"
    );
}
