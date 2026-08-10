use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Mutex, mpsc},
    thread,
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use xdudu_core::provider::{
    FinishReason, MessageRole, ProviderMessage, ProviderRequest, ProviderStreamEvent,
    ProviderStreamSink, ProviderToolDefinition,
};
use xdudu_core::{AnthropicProvider, DeepSeekProvider, Provider};

fn mock_server(response_body: serde_json::Value) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
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
        sender
            .send(String::from_utf8_lossy(&request).into_owned())
            .unwrap();
        let body = serde_json::to_string(&response_body).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(), body
        ).unwrap();
    });
    (format!("http://{address}"), receiver)
}

fn mock_sse_server(response_body: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
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
        sender
            .send(String::from_utf8_lossy(&request).into_owned())
            .unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        )
        .unwrap();
    });
    (format!("http://{address}"), receiver)
}

#[derive(Default)]
struct TextSink(Mutex<String>);

#[async_trait]
impl ProviderStreamSink for TextSink {
    async fn emit(&self, event: ProviderStreamEvent) {
        match event {
            ProviderStreamEvent::TextDelta { text } => self.0.lock().unwrap().push_str(&text),
            ProviderStreamEvent::ReasoningDelta { .. } => {}
        }
    }
}

fn request() -> ProviderRequest {
    ProviderRequest {
        session_id: "session-1".into(),
        model: "test-model".into(),
        messages: vec![ProviderMessage::text(MessageRole::User, "读取 README")],
        tools: vec![ProviderToolDefinition {
            name: "file_read".into(),
            description: "读取文件".into(),
            input_schema: json!({"type":"object"}),
        }],
        system: "系统提示".into(),
        temperature: 0.2,
        max_output_tokens: 128,
        reasoning: false,
        cancellation: CancellationToken::new(),
    }
}

#[tokio::test]
async fn anthropic_发送正确协议并解析响应() {
    let (base_url, captured) = mock_server(json!({
        "content": [{"type":"tool_use","id":"call-a","name":"file_read","input":{"path":"README.md"}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 6}
    }));
    let provider = AnthropicProvider::new("test-key", base_url).unwrap();
    let response = provider.chat(request()).await.unwrap();
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls[0].name, "file_read");

    let raw = captured.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(raw.starts_with("POST /v1/messages HTTP/1.1"));
    assert!(raw.to_ascii_lowercase().contains("x-api-key: test-key"));
    assert!(raw.contains("\"system\":\"系统提示\""));
    assert!(raw.contains("\"name\":\"file_read\""));
}

#[tokio::test]
async fn deepseek_发送正确协议并解析响应() {
    let (base_url, captured) = mock_server(json!({
        "choices": [{"message":{"role":"assistant","content":"完成"},"finish_reason":"stop"}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 8}
    }));
    let provider = DeepSeekProvider::new("deepseek-key", base_url).unwrap();
    let response = provider.chat(request()).await.unwrap();
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(response.message.text_content(), "完成");
    assert_eq!(response.usage.output_tokens, 8);

    let raw = captured.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(raw.starts_with("POST /chat/completions HTTP/1.1"));
    assert!(
        raw.to_ascii_lowercase()
            .contains("authorization: bearer deepseek-key")
    );
    assert!(raw.contains("\"role\":\"system\""));
    assert!(raw.contains("\"max_tokens\":128"));
}

#[tokio::test]
async fn deepseek_流式聚合文本和分段工具参数() {
    let (base_url, captured) = mock_sse_server(concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"正在\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-d\",\"function\":{\"name\":\"file_\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read\",\"arguments\":\"\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":10}}\n\n",
        "data: [DONE]\n\n",
    ));
    let provider = DeepSeekProvider::new("deepseek-key", base_url).unwrap();
    let sink = TextSink::default();
    let response = provider.stream_chat(request(), &sink).await.unwrap();
    assert_eq!(sink.0.lock().unwrap().as_str(), "正在");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls[0].name, "file_read");
    assert_eq!(response.tool_calls[0].input["path"], "README.md");
    assert_eq!(response.usage.output_tokens, 10);
    let raw = captured.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(raw.contains("\"stream\":true"));
    assert!(raw.contains("\"include_usage\":true"));
}
