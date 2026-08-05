//! MCP 客户端、双传输生命周期和声明式服务器配置。
//!
//! 首版只暴露 Tools 能力。stdio 使用逐行 JSON-RPC；Streamable HTTP
//! 使用有界 POST，并支持 JSON 或 SSE 响应。外部工具统一经过 ToolRegistry。

use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{
    Client,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    KeyringSecretStore, SecretStore, SecretString, SideEffectKind, ToolRegistry, XduduError,
    XduduResult,
    permission::PermissionLevel,
    tools::{Tool, ToolContext, ToolDefinition, ToolResult},
};

const MCP_SCHEMA_VERSION: u32 = 1;
const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_TOOLS: usize = 1000;
const MAX_PAGES: usize = 20;
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransportKind {
    Stdio,
    StreamableHttp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub transport: McpTransportKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn enabled_by_default() -> bool {
    true
}

fn default_timeout_seconds() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

impl McpServerConfig {
    pub fn validate(&self) -> XduduResult<()> {
        if self.name.is_empty()
            || self.name.len() > 64
            || !self
                .name
                .chars()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, '_' | '-'))
        {
            return Err(XduduError::validation(
                "MCP Server 名称必须为 1～64 个字母、数字、下划线或连字符。",
            ));
        }
        if !(1..=120).contains(&self.timeout_seconds) {
            return Err(XduduError::validation(
                "MCP timeoutSeconds 必须是 1 到 120。",
            ));
        }
        if self.args.len() > 128 || self.args.iter().any(|value| value.len() > 4096) {
            return Err(XduduError::validation("MCP 参数数量或长度超过限制。"));
        }
        if self.env.len() > 32
            || self.env.iter().any(|(key, value)| {
                key.is_empty() || key.len() > 128 || value.len() > 8192 || sensitive_name(key)
            })
        {
            return Err(XduduError::validation(
                "MCP env 最多 32 项，且不能包含密钥类字段。",
            ));
        }
        match self.transport {
            McpTransportKind::Stdio => {
                let command = self.command.as_deref().unwrap_or_default();
                if command.is_empty() || command.len() > 4096 {
                    return Err(XduduError::validation("stdio MCP 必须提供有效 command。"));
                }
                if self.url.is_some() || self.credential.is_some() {
                    return Err(XduduError::validation(
                        "stdio MCP 不能配置 url 或 credential。",
                    ));
                }
            }
            McpTransportKind::StreamableHttp => {
                if self.command.is_some() || !self.args.is_empty() || !self.env.is_empty() {
                    return Err(XduduError::validation(
                        "Streamable HTTP MCP 不能配置 command、args 或 env。",
                    ));
                }
                validate_http_url(self.url.as_deref().unwrap_or_default())?;
                if self.credential.as_ref().is_some_and(|value| {
                    value.is_empty() || value.len() > 128 || sensitive_name(value)
                }) {
                    return Err(XduduError::validation(
                        "credential 必须是系统凭据中的安全引用名。",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn credential_account(&self) -> Option<String> {
        self.credential.as_ref().map(|value| format!("mcp:{value}"))
    }
}

fn sensitive_name(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase().replace('-', "_");
    ["token", "secret", "password", "api_key", "apikey"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpConfigFile {
    #[serde(default = "mcp_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

fn mcp_schema_version() -> u32 {
    MCP_SCHEMA_VERSION
}

impl Default for McpConfigFile {
    fn default() -> Self {
        Self {
            schema_version: MCP_SCHEMA_VERSION,
            servers: Vec::new(),
        }
    }
}

impl McpConfigFile {
    pub fn validate(&self) -> XduduResult<()> {
        if self.schema_version != MCP_SCHEMA_VERSION {
            return Err(XduduError::validation(format!(
                "不支持的 MCP 配置版本：{}。",
                self.schema_version
            )));
        }
        if self.servers.len() > 32 {
            return Err(XduduError::validation("MCP Server 最多配置 32 个。"));
        }
        let mut names = HashSet::new();
        for server in &self.servers {
            server.validate()?;
            if !names.insert(server.name.clone()) {
                return Err(XduduError::validation(format!(
                    "MCP Server 名称重复：{}。",
                    server.name
                )));
            }
        }
        Ok(())
    }
}

pub fn mcp_config_path() -> XduduResult<PathBuf> {
    let home = if let Some(value) = env::var_os("XDUDU_CONFIG_HOME") {
        PathBuf::from(value)
    } else if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(value).join("xdudu")
    } else if cfg!(windows) {
        PathBuf::from(
            env::var_os("APPDATA").ok_or_else(|| XduduError::validation("无法确定 APPDATA。"))?,
        )
        .join("xdudu")
    } else {
        PathBuf::from(env::var_os("HOME").ok_or_else(|| XduduError::validation("无法确定 HOME。"))?)
            .join(".config/xdudu")
    };
    Ok(home.join("mcp.toml"))
}

pub fn load_mcp_config() -> XduduResult<McpConfigFile> {
    let path = mcp_config_path()?;
    let raw = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(McpConfigFile::default());
        }
        Err(error) => {
            return Err(XduduError::validation(format!(
                "无法读取 MCP 配置 {}：{error}",
                path.display()
            )));
        }
    };
    if raw.len() > 256 * 1024 {
        return Err(XduduError::validation("MCP 配置超过 256 KiB。"));
    }
    let config: McpConfigFile = toml::from_str(&raw)
        .map_err(|error| XduduError::validation(format!("MCP 配置无效：{error}")))?;
    config.validate()?;
    Ok(config)
}

pub fn save_mcp_config(config: &McpConfigFile) -> XduduResult<PathBuf> {
    config.validate()?;
    let path = mcp_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(XduduError::from)?;
    }
    let data = toml::to_string_pretty(config)
        .map_err(|error| XduduError::validation(format!("MCP 配置序列化失败：{error}")))?;
    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, data).map_err(XduduError::from)?;
    fs::rename(&temporary, &path).map_err(XduduError::from)?;
    Ok(path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Default)]
pub struct McpRegistrationReport {
    pub runtimes: Vec<Arc<McpServerRuntime>>,
    pub failures: Vec<String>,
}

struct StdioConnection {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    initialized: bool,
}

struct HttpConnection {
    client: Client,
    url: Url,
    token: Option<SecretString>,
    next_id: u64,
    initialized: bool,
    session_id: Option<String>,
}

enum Connection {
    // StdioConnection 含 tokio::process::Child，Windows 上显著大于 HttpConnection，
    // 装箱避免 clippy::large-enum-variant 的跨平台尺寸告警。
    Stdio(Box<StdioConnection>),
    Http(HttpConnection),
}

pub struct McpServerRuntime {
    config: McpServerConfig,
    connection: Mutex<Option<Connection>>,
}

impl std::fmt::Debug for McpServerRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpServerRuntime")
            .field("name", &self.config.name)
            .field("transport", &self.config.transport)
            .finish_non_exhaustive()
    }
}

impl McpServerRuntime {
    pub async fn new(config: McpServerConfig, store: &dyn SecretStore) -> XduduResult<Self> {
        config.validate()?;
        let token = if let Some(account) = config.credential_account() {
            store
                .get(&account)
                .await?
                .ok_or_else(|| {
                    XduduError::validation(format!(
                        "MCP Server {} 缺少系统凭据：{}。",
                        config.name, account
                    ))
                })?
                .into()
        } else {
            None
        };
        let connection = match config.transport {
            McpTransportKind::Stdio => Connection::Stdio(Box::new(spawn_stdio(&config).await?)),
            McpTransportKind::StreamableHttp => Connection::Http(build_http(&config, token).await?),
        };
        let runtime = Self {
            config,
            connection: Mutex::new(Some(connection)),
        };
        runtime.initialize(CancellationToken::new()).await?;
        Ok(runtime)
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    async fn initialize(&self, cancellation: CancellationToken) -> XduduResult<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion":MCP_PROTOCOL_VERSION,
                    "capabilities":{},
                    "clientInfo":{"name":"XDUDU","version":env!("CARGO_PKG_VERSION")}
                }),
                cancellation.clone(),
            )
            .await?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| XduduError::tool("MCP initialize 缺少 protocolVersion。"))?;
        if ![
            MCP_PROTOCOL_VERSION,
            "2025-06-18",
            "2025-03-26",
            "2024-11-05",
        ]
        .contains(&version)
        {
            return Err(XduduError::tool(format!(
                "MCP Server 返回不支持的协议版本：{version}。"
            )));
        }
        let capabilities = result
            .get("capabilities")
            .and_then(Value::as_object)
            .ok_or_else(|| XduduError::tool("MCP initialize 缺少 capabilities。"))?;
        if !capabilities.contains_key("tools") {
            return Err(XduduError::tool("MCP Server 未声明 tools 能力。"));
        }
        self.notify("notifications/initialized", json!({})).await?;
        let mut guard = self.connection.lock().await;
        match guard.as_mut() {
            Some(Connection::Stdio(connection)) => connection.initialized = true,
            Some(Connection::Http(connection)) => connection.initialized = true,
            None => return Err(XduduError::tool("MCP 连接已关闭。")),
        }
        Ok(())
    }

    pub async fn list_tools(
        &self,
        cancellation: CancellationToken,
    ) -> XduduResult<Vec<McpToolInfo>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |value| json!({"cursor":value}));
            let result = self
                .request("tools/list", params, cancellation.clone())
                .await?;
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| XduduError::tool("MCP tools/list 缺少 tools 数组。"))?;
            for item in page {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| XduduError::tool("MCP Tool 缺少 name。"))?;
                if name.is_empty() || name.len() > 128 {
                    return Err(XduduError::tool("MCP Tool 名称无效。"));
                }
                let input_schema = item
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object"}));
                if !input_schema.is_object() {
                    return Err(XduduError::tool(format!(
                        "MCP Tool {name} 的 inputSchema 不是对象。"
                    )));
                }
                tools.push(McpToolInfo {
                    name: name.to_owned(),
                    description: item
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("外部 MCP 工具")
                        .chars()
                        .take(4096)
                        .collect(),
                    input_schema,
                });
                if tools.len() > MAX_TOOLS {
                    return Err(XduduError::tool("MCP Tool 数量超过 1000。"));
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        Err(XduduError::tool("MCP tools/list 分页超过限制。"))
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> XduduResult<Value> {
        self.request(
            "tools/call",
            json!({"name":name,"arguments":arguments}),
            cancellation,
        )
        .await
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        cancellation: CancellationToken,
    ) -> XduduResult<Value> {
        let timeout = Duration::from_secs(self.config.timeout_seconds);
        let mut guard = self.connection.lock().await;
        let connection = guard
            .as_mut()
            .ok_or_else(|| XduduError::tool("MCP 连接已关闭。"))?;
        match connection {
            Connection::Stdio(connection) => tokio::time::timeout(
                timeout,
                stdio_request(connection, method, params, cancellation),
            )
            .await
            .map_err(|_| XduduError::tool(format!("MCP 请求 {method} 超时。")))?,
            Connection::Http(connection) => tokio::time::timeout(
                timeout,
                http_request(connection, method, params, cancellation),
            )
            .await
            .map_err(|_| XduduError::tool(format!("MCP 请求 {method} 超时。")))?,
        }
    }

    async fn notify(&self, method: &str, params: Value) -> XduduResult<()> {
        let mut guard = self.connection.lock().await;
        match guard
            .as_mut()
            .ok_or_else(|| XduduError::tool("MCP 连接已关闭。"))?
        {
            Connection::Stdio(connection) => {
                write_stdio(
                    connection,
                    &json!({"jsonrpc":"2.0","method":method,"params":params}),
                )
                .await
            }
            Connection::Http(connection) => http_notification(connection, method, params).await,
        }
    }
}

async fn spawn_stdio(config: &McpServerConfig) -> XduduResult<StdioConnection> {
    let mut command = Command::new(config.command.as_deref().unwrap_or_default());
    command
        .args(&config.args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for key in ["PATH", "SYSTEMROOT", "WINDIR"] {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    command.envs(&config.env);
    let mut child = command.spawn().map_err(|error| {
        XduduError::tool(format!("无法启动 MCP Server {}：{error}", config.name))
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| XduduError::tool("MCP Server stdin 不可用。"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| XduduError::tool("MCP Server stdout 不可用。"))?;
    Ok(StdioConnection {
        child,
        stdin,
        stdout: BufReader::new(stdout).lines(),
        next_id: 1,
        initialized: false,
    })
}

async fn write_stdio(connection: &mut StdioConnection, value: &Value) -> XduduResult<()> {
    let mut line = serde_json::to_vec(value).map_err(XduduError::from)?;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(XduduError::tool("MCP 请求超过 1 MiB。"));
    }
    line.push(b'\n');
    connection
        .stdin
        .write_all(&line)
        .await
        .map_err(XduduError::from)?;
    connection.stdin.flush().await.map_err(XduduError::from)
}

async fn stdio_request(
    connection: &mut StdioConnection,
    method: &str,
    params: Value,
    cancellation: CancellationToken,
) -> XduduResult<Value> {
    let id = connection.next_id;
    connection.next_id = connection.next_id.saturating_add(1);
    write_stdio(
        connection,
        &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
    )
    .await?;
    for _ in 0..1000 {
        let line = tokio::select! {
            _ = cancellation.cancelled() => {
                let _ = connection.child.start_kill();
                return Err(XduduError::tool("MCP 请求已取消。"));
            }
            line = connection.stdout.next_line() => line.map_err(XduduError::from)?,
        }
        .ok_or_else(|| XduduError::tool("MCP Server 在响应前关闭 stdout。"))?;
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(XduduError::tool("MCP 响应超过 1 MiB。"));
        }
        let message: Value = serde_json::from_str(&line)
            .map_err(|error| XduduError::tool(format!("MCP stdout 包含无效 JSON：{error}")))?;
        if let (Some(request_id), Some(request_method)) = (
            message.get("id").cloned(),
            message.get("method").and_then(Value::as_str),
        ) {
            let response = if request_method == "ping" {
                json!({"jsonrpc":"2.0","id":request_id,"result":{}})
            } else {
                json!({
                    "jsonrpc":"2.0",
                    "id":request_id,
                    "error":{"code":-32601,"message":"XDUDU 不支持该 MCP 客户端能力"}
                })
            };
            write_stdio(connection, &response).await?;
            continue;
        }
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        return rpc_result(message);
    }
    Err(XduduError::tool("MCP 响应消息数量超过限制。"))
}

async fn build_http(
    config: &McpServerConfig,
    token: Option<SecretString>,
) -> XduduResult<HttpConnection> {
    let url = Url::parse(config.url.as_deref().unwrap_or_default())
        .map_err(|error| XduduError::validation(format!("MCP URL 无效：{error}")))?;
    let client = if local_http(&url) {
        Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(config.timeout_seconds))
            .user_agent("XDUDU/0.8")
            .build()
            .map_err(|error| XduduError::tool(format!("创建 MCP HTTP 客户端失败：{error}")))?
    } else {
        crate::tools::pinned_client(&url, Duration::from_secs(config.timeout_seconds))
            .await
            .map_err(XduduError::tool)?
    };
    Ok(HttpConnection {
        client,
        url,
        token,
        next_id: 1,
        initialized: false,
        session_id: None,
    })
}

fn validate_http_url(value: &str) -> XduduResult<Url> {
    let url = Url::parse(value)
        .map_err(|error| XduduError::validation(format!("MCP URL 无效：{error}")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(XduduError::validation("MCP URL 不能包含认证信息。"));
    }
    if url.scheme() != "https" && !local_http(&url) {
        return Err(XduduError::validation(
            "远程 MCP 只允许 HTTPS；localhost 开发环境可使用 HTTP。",
        ));
    }
    Ok(url)
}

fn local_http(url: &Url) -> bool {
    url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host == "127.0.0.1"
                || host == "[::1]"
                || host == "::1"
        })
}

fn http_headers(connection: &HttpConnection) -> XduduResult<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "MCP-Protocol-Version",
        HeaderValue::from_static(MCP_PROTOCOL_VERSION),
    );
    if let Some(session_id) = &connection.session_id {
        headers.insert(
            "MCP-Session-Id",
            HeaderValue::from_str(session_id)
                .map_err(|_| XduduError::tool("MCP Session ID 包含非法字符。"))?,
        );
    }
    if let Some(token) = &connection.token {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token.expose()))
                .map_err(|_| XduduError::tool("MCP Token 包含非法字符。"))?,
        );
    }
    Ok(headers)
}

async fn http_request(
    connection: &mut HttpConnection,
    method: &str,
    params: Value,
    cancellation: CancellationToken,
) -> XduduResult<Value> {
    let id = connection.next_id;
    connection.next_id = connection.next_id.saturating_add(1);
    let request = connection
        .client
        .post(connection.url.clone())
        .headers(http_headers(connection)?)
        .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
        .send();
    let response = tokio::select! {
        _ = cancellation.cancelled() => return Err(XduduError::tool("MCP HTTP 请求已取消。")),
        response = request => response
            .map_err(|error| XduduError::tool(format!("MCP HTTP 请求失败：{error}")))?,
    };
    capture_session_id(connection, response.headers())?;
    if response.status().is_redirection() {
        return Err(XduduError::tool("MCP HTTP 不跟随重定向。"));
    }
    if !response.status().is_success() {
        return Err(XduduError::tool(format!(
            "MCP HTTP 返回状态 {}。",
            response.status().as_u16()
        )));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = bounded_http_body(response, cancellation).await?;
    let message = if content_type.starts_with("text/event-stream") {
        parse_sse_response(&bytes, id)?
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|error| XduduError::tool(format!("MCP HTTP JSON 无效：{error}")))?
    };
    rpc_result(message)
}

async fn bounded_http_body(
    response: reqwest::Response,
    cancellation: CancellationToken,
) -> XduduResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MESSAGE_BYTES as u64)
    {
        return Err(XduduError::tool("MCP HTTP 响应超过 1 MiB。"));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => return Err(XduduError::tool("MCP HTTP 请求已取消。")),
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else { break };
        let chunk =
            chunk.map_err(|error| XduduError::tool(format!("读取 MCP HTTP 响应失败：{error}")))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_MESSAGE_BYTES {
            return Err(XduduError::tool("MCP HTTP 响应超过 1 MiB。"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn http_notification(
    connection: &mut HttpConnection,
    method: &str,
    params: Value,
) -> XduduResult<()> {
    let response = connection
        .client
        .post(connection.url.clone())
        .headers(http_headers(connection)?)
        .json(&json!({"jsonrpc":"2.0","method":method,"params":params}))
        .send()
        .await
        .map_err(|error| XduduError::tool(format!("MCP HTTP 通知失败：{error}")))?;
    capture_session_id(connection, response.headers())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(XduduError::tool(format!(
            "MCP HTTP 通知返回状态 {}。",
            response.status().as_u16()
        )))
    }
}

fn capture_session_id(connection: &mut HttpConnection, headers: &HeaderMap) -> XduduResult<()> {
    if let Some(value) = headers.get("MCP-Session-Id") {
        let value = value
            .to_str()
            .map_err(|_| XduduError::tool("MCP Session ID 不是有效文本。"))?;
        if value.len() > 1024 {
            return Err(XduduError::tool("MCP Session ID 过长。"));
        }
        connection.session_id = Some(value.to_owned());
    }
    Ok(())
}

fn parse_sse_response(bytes: &[u8], id: u64) -> XduduResult<Value> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| XduduError::tool("MCP SSE 响应不是 UTF-8。"))?;
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let message: Value = serde_json::from_str(data.trim())
                .map_err(|error| XduduError::tool(format!("MCP SSE data 无效：{error}")))?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(message);
            }
        }
    }
    Err(XduduError::tool("MCP SSE 中没有匹配的响应。"))
}

fn rpc_result(message: Value) -> XduduResult<Value> {
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(XduduError::tool("MCP 响应不是 JSON-RPC 2.0。"));
    }
    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未知 MCP 错误");
        return Err(XduduError::tool(format!("MCP {code}：{message}")));
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| XduduError::tool("MCP 响应缺少 result。"))
}

pub struct McpTool {
    exposed_name: String,
    remote_name: String,
    description: String,
    input_schema: Value,
    runtime: Arc<McpServerRuntime>,
}

impl McpTool {
    fn new(runtime: Arc<McpServerRuntime>, info: McpToolInfo) -> Self {
        let server = sanitize_tool_component(runtime.name());
        let tool = sanitize_tool_component(&info.name);
        Self {
            exposed_name: format!("mcp__{server}__{tool}"),
            remote_name: info.name,
            description: format!("[MCP: {}] {}", runtime.name(), info.description),
            input_schema: info.input_schema,
            runtime,
        }
    }
}

fn sanitize_tool_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.exposed_name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            permission_level: PermissionLevel::FullAccess,
            side_effect: match self.runtime.config.transport {
                McpTransportKind::Stdio => SideEffectKind::ProcessExecution,
                McpTransportKind::StreamableHttp => SideEffectKind::NetworkAccess,
            },
            default_timeout: Duration::from_secs(self.runtime.config.timeout_seconds + 5),
        }
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        validate_schema_input(&self.input_schema, input)
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        context.report_progress(crate::tools::ToolProgressUpdate::phase(
            "mcp_call",
            format!("调用 {}", self.remote_name),
        ));
        match self
            .runtime
            .call_tool(&self.remote_name, input, context.cancellation.clone())
            .await
        {
            Ok(result) => {
                let is_error = result
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if is_error {
                    ToolResult::failure(
                        "MCP_TOOL_ERROR",
                        "MCP 工具返回执行错误。",
                        context.started_at,
                        result,
                    )
                } else {
                    ToolResult::success(
                        result,
                        context.started_at,
                        json!({
                            "server":self.runtime.name(),
                            "remoteTool":self.remote_name,
                        }),
                    )
                }
            }
            Err(error) => ToolResult::failure(
                "MCP_CALL_FAILED",
                error.message,
                context.started_at,
                json!({"server":self.runtime.name(),"remoteTool":self.remote_name}),
            ),
        }
    }
}

fn validate_schema_input(schema: &Value, input: &Value) -> Result<(), Vec<String>> {
    let Some(schema) = schema.as_object() else {
        return Err(vec!["MCP inputSchema 必须是对象。".into()]);
    };
    let mut issues = Vec::new();
    if schema.get("type").and_then(Value::as_str) == Some("object") && !input.is_object() {
        issues.push("输入必须是 JSON 对象。".into());
        return Err(issues);
    }
    if let Some(map) = input.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    issues.push(format!("缺少必填字段：{key}"));
                }
            }
        }
        if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
            && let Some(properties) = schema.get("properties").and_then(Value::as_object)
        {
            for key in map.keys() {
                if !properties.contains_key(key) {
                    issues.push(format!("不支持字段：{key}"));
                }
            }
        }
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

pub async fn register_configured_mcp_tools(
    registry: &mut ToolRegistry,
) -> XduduResult<McpRegistrationReport> {
    let config = load_mcp_config()?;
    let plugins = crate::load_plugin_manifests()?;
    let mut servers = config.servers;
    for plugin in plugins.into_iter().filter(|plugin| plugin.enabled) {
        servers.extend(plugin.mcp_servers);
    }
    servers.sort_by(|left, right| left.name.cmp(&right.name));
    for pair in servers.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(XduduError::validation(format!(
                "MCP Server 名称重复：{}",
                pair[0].name
            )));
        }
    }
    let store = KeyringSecretStore;
    let mut report = McpRegistrationReport::default();
    for server in servers.into_iter().filter(|server| server.enabled) {
        let server_name = server.name.clone();
        let runtime = match McpServerRuntime::new(server, &store).await {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                report
                    .failures
                    .push(format!("MCP Server {server_name}：{}", error.message));
                continue;
            }
        };
        let tools = match runtime.list_tools(CancellationToken::new()).await {
            Ok(tools) => tools,
            Err(error) => {
                report
                    .failures
                    .push(format!("MCP Server {server_name}：{}", error.message));
                continue;
            }
        };
        for tool in tools {
            registry.register(McpTool::new(Arc::clone(&runtime), tool))?;
        }
        report.runtimes.push(runtime);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn 配置拒绝明文密钥与远程_http() {
        let bad_env = McpServerConfig {
            name: "bad".into(),
            enabled: true,
            transport: McpTransportKind::Stdio,
            command: Some("node".into()),
            args: Vec::new(),
            env: BTreeMap::from([("API_TOKEN".into(), "secret".into())]),
            url: None,
            credential: None,
            timeout_seconds: 30,
        };
        assert!(bad_env.validate().is_err());
        assert!(validate_http_url("http://example.com/mcp").is_err());
        assert!(validate_http_url("http://127.0.0.1:3000/mcp").is_ok());
        assert!(validate_http_url("https://example.com/mcp").is_ok());
    }

    #[test]
    fn sse_只返回匹配请求_id() {
        let body =
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n";
        let value = parse_sse_response(body, 2).unwrap();
        assert_eq!(rpc_result(value).unwrap()["ok"], true);
    }

    #[test]
    fn 外部工具名称被安全命名空间化() {
        assert_eq!(sanitize_tool_component("git/status"), "git_status");
        let issues = validate_schema_input(
            &json!({
                "type":"object",
                "properties":{"path":{"type":"string"}},
                "required":["path"],
                "additionalProperties":false
            }),
            &json!({"extra":true}),
        )
        .unwrap_err();
        assert_eq!(issues.len(), 2);
    }

    #[test]
    fn mcp_工具始终进入最高权限和副作用审批() {
        let runtime = Arc::new(McpServerRuntime {
            config: McpServerConfig {
                name: "local".into(),
                enabled: true,
                transport: McpTransportKind::Stdio,
                command: Some("fixture".into()),
                args: Vec::new(),
                env: BTreeMap::new(),
                url: None,
                credential: None,
                timeout_seconds: 30,
            },
            connection: Mutex::new(None),
        });
        let tool = McpTool::new(
            runtime,
            McpToolInfo {
                name: "read".into(),
                description: "外部能力".into(),
                input_schema: json!({"type":"object"}),
            },
        );
        let definition = tool.definition();
        assert_eq!(definition.permission_level, PermissionLevel::FullAccess);
        assert_eq!(definition.side_effect, SideEffectKind::ProcessExecution);
        assert!(definition.name.starts_with("mcp__local__"));
    }

    #[tokio::test]
    async fn streamable_http_完成初始化发现和调用() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for sequence in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(header_end) =
                        request.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let header_end = header_end + 4;
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(str::trim)
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + length {
                            break;
                        }
                    }
                }
                let body_start = request
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let body: Value = serde_json::from_slice(&request[body_start..]).unwrap();
                let response = match sequence {
                    0 => json!({"jsonrpc":"2.0","id":body["id"],"result":{
                        "protocolVersion":MCP_PROTOCOL_VERSION,
                        "capabilities":{"tools":{}},
                        "serverInfo":{"name":"mock","version":"1"}
                    }}),
                    1 => {
                        assert_eq!(body["method"], "notifications/initialized");
                        let head = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        stream.write_all(head.as_bytes()).await.unwrap();
                        continue;
                    }
                    2 => json!({"jsonrpc":"2.0","id":body["id"],"result":{"tools":[{
                        "name":"echo","description":"回显","inputSchema":{"type":"object"}
                    }]}}),
                    _ => json!({"jsonrpc":"2.0","id":body["id"],"result":{
                        "content":[{"type":"text","text":"ok"}],"isError":false
                    }}),
                };
                let bytes = serde_json::to_vec(&response).unwrap();
                let session = if sequence == 0 {
                    "MCP-Session-Id: test-session\r\n"
                } else {
                    ""
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
            }
        });
        let runtime = McpServerRuntime::new(
            McpServerConfig {
                name: "mock".into(),
                enabled: true,
                transport: McpTransportKind::StreamableHttp,
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                url: Some(format!("http://{address}/mcp")),
                credential: None,
                timeout_seconds: 5,
            },
            &KeyringSecretStore,
        )
        .await
        .unwrap();
        let tools = runtime.list_tools(CancellationToken::new()).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let result = runtime
            .call_tool("echo", json!({"value":"ok"}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result["isError"], false);
        server.await.unwrap();
    }

    // ---------- stdio 传输 E2E ----------

    fn python_command() -> &'static str {
        if cfg!(windows) { "python" } else { "python3" }
    }

    struct StdioMock {
        path: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    /// 生成 mock stdio MCP server 脚本；mode 控制行为：
    /// `normal` 正常响应；`garbage` 注入非 JSON 与超大行；`silent` 对 tools/call 沉默。
    fn stdio_mock(mode: &str) -> StdioMock {
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            "import sys, json\n\
             sys.stdin.reconfigure(encoding='utf-8')\n\
             sys.stdout.reconfigure(encoding='utf-8')\n\
             mode = {mode:?}\n\
             for raw in sys.stdin:\n\
             \x20   raw = raw.strip()\n\
             \x20   if not raw:\n\
             \x20       continue\n\
             \x20   if mode == 'garbage':\n\
             \x20       sys.stdout.write('not-json\\n')\n\
             \x20       sys.stdout.write('x' * (1024 * 1024 + 100) + '\\n')\n\
             \x20       sys.stdout.flush()\n\
             \x20       continue\n\
             \x20   try:\n\
             \x20       msg = json.loads(raw)\n\
             \x20   except Exception:\n\
             \x20       continue\n\
             \x20   rid = msg.get('id')\n\
             \x20   method = msg.get('method')\n\
             \x20   if mode == 'silent' and method == 'tools/call':\n\
             \x20       continue\n\
             \x20   result = None\n\
             \x20   if method == 'initialize':\n\
             \x20       result = {{'protocolVersion': '2025-11-25', 'capabilities': {{'tools': {{}}}}, 'serverInfo': {{'name': 'mock', 'version': '1.0'}}}}\n\
             \x20   elif method == 'tools/list':\n\
             \x20       result = {{'tools': [{{'name': 'echo', 'description': '回显', 'inputSchema': {{'type': 'object', 'properties': {{}}}}}}]}}\n\
             \x20   elif method == 'tools/call':\n\
             \x20       result = {{'content': [{{'type': 'text', 'text': 'ok'}}]}}\n\
             \x20   if result is not None and rid is not None:\n\
             \x20       sys.stdout.write(json.dumps({{'jsonrpc': '2.0', 'id': rid, 'result': result}}) + '\\n')\n\
             \x20       sys.stdout.flush()\n"
        );
        let path = dir.path().join(format!("mock_stdio_{mode}.py"));
        std::fs::write(&path, script).unwrap();
        StdioMock { path, _dir: dir }
    }

    fn mock_stdio_config(mock: &StdioMock, timeout: u64) -> McpServerConfig {
        McpServerConfig {
            name: "mock".into(),
            enabled: true,
            transport: McpTransportKind::Stdio,
            command: Some(python_command().into()),
            args: vec![mock.path.to_string_lossy().into_owned()],
            env: BTreeMap::new(),
            url: None,
            credential: None,
            timeout_seconds: timeout,
        }
    }

    #[tokio::test]
    async fn stdio_mock_server_完成初始化发现与调用() {
        let mock = stdio_mock("normal");
        let runtime = McpServerRuntime::new(mock_stdio_config(&mock, 10), &KeyringSecretStore)
            .await
            .unwrap();
        let tools = runtime.list_tools(CancellationToken::new()).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let result = runtime
            .call_tool("echo", json!({}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result["content"][0]["text"], "ok");
    }

    #[tokio::test]
    async fn stdio_恶意畸形输出被拒绝且不崩溃() {
        let mock = stdio_mock("garbage");
        // 非 JSON 与超过 1 MiB 的行都会被当作协议错误：握手或发现阶段即拒绝，
        // 客户端不崩溃、不挂起。
        let runtime =
            McpServerRuntime::new(mock_stdio_config(&mock, 10), &KeyringSecretStore).await;
        match runtime {
            Ok(runtime) => {
                let result = runtime.list_tools(CancellationToken::new()).await;
                assert!(result.is_err());
            }
            Err(error) => {
                assert!(error.message.contains("无效 JSON"));
            }
        }
    }

    #[tokio::test]
    async fn stdio_调用超时返回错误() {
        let mock = stdio_mock("silent");
        let runtime = McpServerRuntime::new(mock_stdio_config(&mock, 1), &KeyringSecretStore)
            .await
            .unwrap();
        // 握手与发现正常（silent 只对 tools/call 沉默）。
        let tools = runtime.list_tools(CancellationToken::new()).await.unwrap();
        assert_eq!(tools.len(), 1);
        let started = std::time::Instant::now();
        let result = runtime
            .call_tool("echo", json!({}), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(started.elapsed().as_secs() >= 1);
    }

    #[tokio::test]
    async fn stdio_取消_终止调用() {
        let mock = stdio_mock("silent");
        let runtime = Arc::new(
            McpServerRuntime::new(mock_stdio_config(&mock, 10), &KeyringSecretStore)
                .await
                .unwrap(),
        );
        let _ = runtime.list_tools(CancellationToken::new()).await.unwrap();
        let cancellation = CancellationToken::new();
        let call = {
            let runtime = Arc::clone(&runtime);
            let cancellation = cancellation.clone();
            tokio::spawn(async move { runtime.call_tool("echo", json!({}), cancellation).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancellation.cancel();
        let result = call.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mcp_工具_拒绝审批时_不向服务器发送调用() {
        let mock = stdio_mock("silent");
        let runtime = Arc::new(
            McpServerRuntime::new(mock_stdio_config(&mock, 2), &KeyringSecretStore)
                .await
                .unwrap(),
        );
        let tools = runtime.list_tools(CancellationToken::new()).await.unwrap();
        let registry = crate::tools::ToolRegistry::with_runtime(
            Arc::new(crate::approval::DenyAllApprovalGate),
            Arc::new(crate::changes::NoopChangeLedger),
        );
        let mut registry = registry;
        registry
            .register(McpTool::new(Arc::clone(&runtime), tools[0].clone()))
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let result = registry
            .execute(
                "mcp__mock__echo",
                json!({}),
                uuid::Uuid::new_v4(),
                dir.path(),
                crate::permission::PermissionMode::FullAccess,
                CancellationToken::new(),
            )
            .await;
        // silent server 对 tools/call 沉默：若审批未拦截，这里会超时；
        // 立即返回 APPROVAL_DENIED 证明调用从未发送到服务器。
        assert_eq!(result.error.unwrap().code, "APPROVAL_DENIED");
    }
}
