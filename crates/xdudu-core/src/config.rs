//! 分层配置加载、来源追踪与安全写入。

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use toml::Value;
use uuid::Uuid;

use crate::{
    approval::ApprovalMode,
    error::{ErrorKind, XduduError, XduduResult},
    permission::PermissionMode,
};

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5-20250929";
const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-v4-flash";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    Default,
    User,
    Project,
    Environment,
    Cli,
}

impl ConfigSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "user",
            Self::Project => "project",
            Self::Environment => "environment",
            Self::Cli => "cli",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub name: String,
    pub model: String,
    pub base_url: Option<String>,
    pub timeout_seconds: u64,
    pub max_attempts: u32,
    pub retry_base_ms: u64,
    pub min_request_interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    pub max_turns: u32,
    pub permission: String,
    pub approval: String,
}

impl AgentConfig {
    pub fn permission_mode(&self) -> XduduResult<PermissionMode> {
        self.permission.parse()
    }

    pub fn approval_mode(&self) -> XduduResult<ApprovalMode> {
        self.approval.parse()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputConfig {
    pub json: bool,
    pub no_stream: bool,
    pub color: bool,
    pub debug_trace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub provider: ProviderConfig,
    pub agent: AgentConfig,
    pub output: OutputConfig,
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryConfig {
    /// 遥测默认关闭；XDUDU 当前不发送任何数据，未来若引入必须保持
    /// 默认关闭并显式授权。
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub config: AppConfig,
    pub sources: BTreeMap<String, ConfigSource>,
    pub user_path: PathBuf,
    pub project_path: PathBuf,
}

impl ResolvedConfig {
    pub fn source(&self, key: &str) -> Option<ConfigSource> {
        self.sources.get(key).copied()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_turns: Option<u32>,
    pub permission: Option<String>,
    pub approval: Option<String>,
    pub json: Option<bool>,
    pub no_stream: Option<bool>,
    pub color: Option<bool>,
    pub debug_trace: Option<bool>,
    pub telemetry_enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct FileConfig {
    #[serde(default)]
    provider: FileProvider,
    #[serde(default)]
    agent: FileAgent,
    #[serde(default)]
    output: FileOutput,
    #[serde(default)]
    telemetry: FileTelemetry,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct FileTelemetry {
    enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct FileProvider {
    name: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    timeout_seconds: Option<u64>,
    max_attempts: Option<u32>,
    retry_base_ms: Option<u64>,
    min_request_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct FileAgent {
    max_turns: Option<u32>,
    permission: Option<String>,
    approval: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct FileOutput {
    json: Option<bool>,
    no_stream: Option<bool>,
    color: Option<bool>,
    debug_trace: Option<bool>,
}

fn config_error(message: impl Into<String>) -> XduduError {
    XduduError::new(ErrorKind::ConfigError, message)
}

fn user_config_path() -> XduduResult<PathBuf> {
    if let Some(path) = env::var_os("XDUDU_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("config.toml"));
    }
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("xdudu/config.toml"));
    }
    if cfg!(windows)
        && let Some(path) = env::var_os("APPDATA")
    {
        return Ok(PathBuf::from(path).join("xdudu/config.toml"));
    }
    let home =
        env::var_os("HOME").ok_or_else(|| config_error("无法确定用户配置目录：HOME 未设置。"))?;
    Ok(PathBuf::from(home).join(".config/xdudu/config.toml"))
}

fn legacy_user_config_path() -> XduduResult<PathBuf> {
    if let Some(path) = env::var_os("XYCLI_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("config.toml"));
    }
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("xycli/config.toml"));
    }
    if cfg!(windows)
        && let Some(path) = env::var_os("APPDATA")
    {
        return Ok(PathBuf::from(path).join("xycli/config.toml"));
    }
    let home =
        env::var_os("HOME").ok_or_else(|| config_error("无法确定用户配置目录：HOME 未设置。"))?;
    Ok(PathBuf::from(home).join(".config/xycli/config.toml"))
}

pub fn config_paths(cwd: &Path) -> XduduResult<(PathBuf, PathBuf)> {
    Ok((user_config_path()?, cwd.join(".xdudu/config.toml")))
}

pub fn approval_rules_path() -> XduduResult<PathBuf> {
    let config = user_config_path()?;
    let parent = config
        .parent()
        .ok_or_else(|| config_error("用户配置路径缺少父目录。"))?;
    Ok(parent.join("approval-rules.json"))
}

fn contains_secret_key(value: &Value) -> bool {
    match value {
        Value::Table(table) => table.iter().any(|(key, value)| {
            matches!(
                key.to_ascii_lowercase().replace('-', "_").as_str(),
                "api_key" | "apikey" | "token" | "secret"
            ) || contains_secret_key(value)
        }),
        Value::Array(values) => values.iter().any(contains_secret_key),
        _ => false,
    }
}

fn read_file(path: &Path) -> XduduResult<Option<FileConfig>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(config_error(format!(
                "无法读取配置 {}：{error}",
                path.display()
            )));
        }
    };
    let value: Value = toml::from_str(&raw)
        .map_err(|error| config_error(format!("配置 {} 格式无效：{error}", path.display())))?;
    if contains_secret_key(&value) {
        return Err(config_error(format!(
            "配置 {} 包含明文密钥字段；请使用环境变量或 xdudu auth login。",
            path.display()
        )));
    }
    value
        .try_into()
        .map(Some)
        .map_err(|error| config_error(format!("配置 {} 内容无效：{error}", path.display())))
}

fn set<T: Clone>(
    value: &mut T,
    incoming: &Option<T>,
    key: &str,
    source: ConfigSource,
    sources: &mut BTreeMap<String, ConfigSource>,
) {
    if let Some(incoming) = incoming {
        value.clone_from(incoming);
        sources.insert(key.to_owned(), source);
    }
}

fn apply_file(
    config: &mut AppConfig,
    file: &FileConfig,
    source: ConfigSource,
    sources: &mut BTreeMap<String, ConfigSource>,
) {
    set(
        &mut config.provider.name,
        &file.provider.name,
        "provider.name",
        source,
        sources,
    );
    set(
        &mut config.provider.model,
        &file.provider.model,
        "provider.model",
        source,
        sources,
    );
    set(
        &mut config.provider.base_url,
        &file.provider.base_url.clone().map(Some),
        "provider.base_url",
        source,
        sources,
    );
    set(
        &mut config.provider.timeout_seconds,
        &file.provider.timeout_seconds,
        "provider.timeout_seconds",
        source,
        sources,
    );
    set(
        &mut config.provider.max_attempts,
        &file.provider.max_attempts,
        "provider.max_attempts",
        source,
        sources,
    );
    set(
        &mut config.provider.retry_base_ms,
        &file.provider.retry_base_ms,
        "provider.retry_base_ms",
        source,
        sources,
    );
    set(
        &mut config.provider.min_request_interval_ms,
        &file.provider.min_request_interval_ms,
        "provider.min_request_interval_ms",
        source,
        sources,
    );
    set(
        &mut config.agent.max_turns,
        &file.agent.max_turns,
        "agent.max_turns",
        source,
        sources,
    );
    set(
        &mut config.agent.permission,
        &file.agent.permission,
        "agent.permission",
        source,
        sources,
    );
    set(
        &mut config.agent.approval,
        &file.agent.approval,
        "agent.approval",
        source,
        sources,
    );
    set(
        &mut config.output.json,
        &file.output.json,
        "output.json",
        source,
        sources,
    );
    set(
        &mut config.output.no_stream,
        &file.output.no_stream,
        "output.no_stream",
        source,
        sources,
    );
    set(
        &mut config.output.color,
        &file.output.color,
        "output.color",
        source,
        sources,
    );
    set(
        &mut config.output.debug_trace,
        &file.output.debug_trace,
        "output.debug_trace",
        source,
        sources,
    );
    set(
        &mut config.telemetry.enabled,
        &file.telemetry.enabled,
        "telemetry.enabled",
        source,
        sources,
    );
}

fn validate_project_trust(
    config: &AppConfig,
    project: &FileConfig,
    path: &Path,
) -> XduduResult<()> {
    if project.provider.base_url.is_some() {
        return Err(config_error(format!(
            "项目配置 {} 不能设置 provider.base_url；请使用 CLI、环境变量或用户配置，避免仓库重定向系统凭据。",
            path.display()
        )));
    }
    if let Some(permission) = &project.agent.permission {
        let current = config.agent.permission_mode()?;
        let requested = permission.parse::<PermissionMode>()?;
        if requested
            .allowed_levels()
            .iter()
            .any(|level| !current.allows(*level))
        {
            return Err(config_error(format!(
                "项目配置 {} 不能把 agent.permission 从 {} 提升到 {}。",
                path.display(),
                current.as_str(),
                requested.as_str()
            )));
        }
    }
    if let Some(approval) = &project.agent.approval {
        let current = config.agent.approval_mode()?;
        let requested = approval.parse::<ApprovalMode>()?;
        let rank = |mode| match mode {
            ApprovalMode::Never => 0,
            ApprovalMode::Ask => 1,
            ApprovalMode::AcceptEdits => 2,
            ApprovalMode::Always => 3,
        };
        if rank(requested) > rank(current) {
            return Err(config_error(format!(
                "项目配置 {} 不能把 agent.approval 从 {} 提升到 {}。",
                path.display(),
                current.as_str(),
                requested.as_str()
            )));
        }
    }
    Ok(())
}

fn env_file(provider: &str) -> FileConfig {
    let value =
        |current: &str, legacy: &str| env::var(current).ok().or_else(|| env::var(legacy).ok());
    let provider_name = value("XDUDU_PROVIDER", "XYCLI_PROVIDER");
    let endpoint_provider = provider_name.clone().unwrap_or_else(|| provider.to_owned());
    FileConfig {
        provider: FileProvider {
            name: provider_name,
            model: value("XDUDU_MODEL", "XYCLI_MODEL"),
            base_url: value("XDUDU_BASE_URL", "XYCLI_BASE_URL").or_else(|| {
                env::var(format!(
                    "{}_BASE_URL",
                    endpoint_provider.to_ascii_uppercase()
                ))
                .ok()
            }),
            timeout_seconds: value("XDUDU_TIMEOUT_SECONDS", "XYCLI_TIMEOUT_SECONDS")
                .and_then(|value| value.parse().ok()),
            max_attempts: value("XDUDU_MAX_ATTEMPTS", "XYCLI_MAX_ATTEMPTS")
                .and_then(|value| value.parse().ok()),
            retry_base_ms: value("XDUDU_RETRY_BASE_MS", "XYCLI_RETRY_BASE_MS")
                .and_then(|value| value.parse().ok()),
            min_request_interval_ms: value(
                "XDUDU_MIN_REQUEST_INTERVAL_MS",
                "XYCLI_MIN_REQUEST_INTERVAL_MS",
            )
            .and_then(|value| value.parse().ok()),
        },
        agent: FileAgent {
            max_turns: value("XDUDU_MAX_TURNS", "XYCLI_MAX_TURNS")
                .and_then(|value| value.parse().ok()),
            permission: value("XDUDU_PERMISSION", "XYCLI_PERMISSION"),
            approval: value("XDUDU_APPROVAL", "XYCLI_APPROVAL"),
        },
        output: FileOutput {
            json: value("XDUDU_JSON", "XYCLI_JSON").and_then(|value| value.parse().ok()),
            no_stream: value("XDUDU_NO_STREAM", "XYCLI_NO_STREAM")
                .and_then(|value| value.parse().ok()),
            color: env::var("NO_COLOR").ok().map(|_| false),
            debug_trace: value("XDUDU_DEBUG_TRACE", "XYCLI_DEBUG_TRACE")
                .and_then(|value| value.parse().ok()),
        },
        telemetry: FileTelemetry {
            enabled: value("XDUDU_TELEMETRY_ENABLED", "XYCLI_TELEMETRY_ENABLED")
                .and_then(|value| value.parse().ok()),
        },
    }
}

fn validate(config: &mut AppConfig, model_was_explicit: bool) -> XduduResult<()> {
    config.provider.name = config.provider.name.to_ascii_lowercase();
    if !matches!(config.provider.name.as_str(), "anthropic" | "deepseek") {
        return Err(config_error(format!(
            "不支持的 Provider：{}。可选值：anthropic、deepseek。",
            config.provider.name
        )));
    }
    if !model_was_explicit {
        config.provider.model = match config.provider.name.as_str() {
            "deepseek" => DEFAULT_DEEPSEEK_MODEL,
            _ => DEFAULT_ANTHROPIC_MODEL,
        }
        .to_owned();
    }
    if config.provider.model.trim().is_empty() {
        return Err(config_error("provider.model 不能为空。"));
    }
    if !(1..=100).contains(&config.agent.max_turns) {
        return Err(config_error("agent.max_turns 必须是 1 到 100。"));
    }
    if !(1..=600).contains(&config.provider.timeout_seconds) {
        return Err(config_error("provider.timeout_seconds 必须是 1 到 600。"));
    }
    if !(1..=10).contains(&config.provider.max_attempts) {
        return Err(config_error("provider.max_attempts 必须是 1 到 10。"));
    }
    if !(10..=30_000).contains(&config.provider.retry_base_ms) {
        return Err(config_error("provider.retry_base_ms 必须是 10 到 30000。"));
    }
    if config.provider.min_request_interval_ms > 60_000 {
        return Err(config_error(
            "provider.min_request_interval_ms 必须是 0 到 60000。",
        ));
    }
    config.agent.permission.parse::<PermissionMode>()?;
    config.agent.approval.parse::<ApprovalMode>()?;
    if let Some(base_url) = &config.provider.base_url
        && !(base_url.starts_with("https://")
            || base_url.starts_with("http://127.0.0.1")
            || base_url.starts_with("http://localhost"))
    {
        return Err(config_error(
            "provider.base_url 必须使用 HTTPS；只有本机测试地址允许 HTTP。",
        ));
    }
    Ok(())
}

pub fn load_config(cwd: &Path, overrides: ConfigOverrides) -> XduduResult<ResolvedConfig> {
    let (user_path, project_path) = config_paths(cwd)?;
    let legacy_user_path = legacy_user_config_path()?;
    let legacy_project_path = cwd.join(".xycli/config.toml");
    let read_user_path = if user_path.exists() {
        user_path.clone()
    } else {
        legacy_user_path
    };
    let read_project_path = if project_path.exists() {
        project_path.clone()
    } else {
        legacy_project_path
    };
    let mut resolved =
        load_config_from_paths(cwd, read_user_path, read_project_path, overrides, None)?;
    resolved.user_path = user_path;
    resolved.project_path = project_path;
    Ok(resolved)
}

fn load_config_from_paths(
    _cwd: &Path,
    user_path: PathBuf,
    project_path: PathBuf,
    overrides: ConfigOverrides,
    fixed_environment: Option<FileConfig>,
) -> XduduResult<ResolvedConfig> {
    let mut config = AppConfig {
        provider: ProviderConfig {
            name: "deepseek".into(),
            model: DEFAULT_DEEPSEEK_MODEL.into(),
            base_url: None,
            timeout_seconds: 180,
            max_attempts: 3,
            retry_base_ms: 500,
            min_request_interval_ms: 0,
        },
        agent: AgentConfig {
            max_turns: 25,
            permission: "auto-safe".into(),
            approval: "ask".into(),
        },
        output: OutputConfig {
            json: false,
            no_stream: false,
            color: env::var_os("NO_COLOR").is_none(),
            debug_trace: false,
        },
        telemetry: TelemetryConfig { enabled: false },
    };
    let mut sources = [
        ("provider.name", ConfigSource::Default),
        ("provider.model", ConfigSource::Default),
        ("provider.base_url", ConfigSource::Default),
        ("provider.timeout_seconds", ConfigSource::Default),
        ("provider.max_attempts", ConfigSource::Default),
        ("provider.retry_base_ms", ConfigSource::Default),
        ("provider.min_request_interval_ms", ConfigSource::Default),
        ("agent.max_turns", ConfigSource::Default),
        ("agent.permission", ConfigSource::Default),
        ("agent.approval", ConfigSource::Default),
        ("output.json", ConfigSource::Default),
        ("output.no_stream", ConfigSource::Default),
        ("output.color", ConfigSource::Default),
        ("output.debug_trace", ConfigSource::Default),
        ("telemetry.enabled", ConfigSource::Default),
    ]
    .into_iter()
    .map(|(key, source)| (key.to_owned(), source))
    .collect::<BTreeMap<_, _>>();

    if let Some(file) = read_file(&user_path)? {
        apply_file(&mut config, &file, ConfigSource::User, &mut sources);
    }
    if let Some(file) = read_file(&project_path)? {
        validate_project_trust(&config, &file, &project_path)?;
        apply_file(&mut config, &file, ConfigSource::Project, &mut sources);
    }
    let environment = fixed_environment.unwrap_or_else(|| env_file(&config.provider.name));
    apply_file(
        &mut config,
        &environment,
        ConfigSource::Environment,
        &mut sources,
    );
    let model_was_explicit = overrides.model.is_some()
        || environment.provider.model.is_some()
        || sources.get("provider.model") != Some(&ConfigSource::Default);
    let cli = FileConfig {
        provider: FileProvider {
            name: overrides.provider,
            model: overrides.model,
            base_url: overrides.base_url,
            timeout_seconds: None,
            max_attempts: None,
            retry_base_ms: None,
            min_request_interval_ms: None,
        },
        agent: FileAgent {
            max_turns: overrides.max_turns,
            permission: overrides.permission,
            approval: overrides.approval,
        },
        output: FileOutput {
            json: overrides.json,
            no_stream: overrides.no_stream,
            color: overrides.color,
            debug_trace: overrides.debug_trace,
        },
        telemetry: FileTelemetry {
            enabled: overrides.telemetry_enabled,
        },
    };
    apply_file(&mut config, &cli, ConfigSource::Cli, &mut sources);
    validate(&mut config, model_was_explicit)?;
    Ok(ResolvedConfig {
        config,
        sources,
        user_path,
        project_path,
    })
}

pub fn write_config_value(
    cwd: &Path,
    user: bool,
    key: &str,
    raw_value: &str,
) -> XduduResult<PathBuf> {
    if matches!(
        key.to_ascii_lowercase().replace('-', "_").as_str(),
        "api_key" | "apikey" | "token" | "secret"
    ) {
        return Err(config_error(
            "密钥不能写入配置文件，请使用 xdudu auth login。",
        ));
    }
    if !user && key == "provider.base_url" {
        return Err(config_error(
            "项目配置不能设置 provider.base_url；请使用 --user、环境变量或 CLI 参数。",
        ));
    }
    if !user && key == "agent.permission" && raw_value != "read-only" {
        return Err(config_error(
            "项目配置只能把 agent.permission 收紧为 read-only。",
        ));
    }
    if !user && key == "agent.approval" && raw_value != "never" {
        return Err(config_error("项目配置只能把 agent.approval 收紧为 never。"));
    }
    let (user_path, project_path) = config_paths(cwd)?;
    let path = if user { user_path } else { project_path };
    let mut value = match fs::read_to_string(&path) {
        Ok(raw) => toml::from_str::<Value>(&raw)
            .map_err(|error| config_error(format!("配置 {} 格式无效：{error}", path.display())))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Value::Table(Default::default())
        }
        Err(error) => {
            return Err(config_error(format!(
                "无法读取配置 {}：{error}",
                path.display()
            )));
        }
    };
    let parts = key.split('.').collect::<Vec<_>>();
    if parts.len() != 2
        || !matches!(
            key,
            "provider.name"
                | "provider.model"
                | "provider.base_url"
                | "provider.timeout_seconds"
                | "provider.max_attempts"
                | "provider.retry_base_ms"
                | "provider.min_request_interval_ms"
                | "agent.max_turns"
                | "agent.permission"
                | "agent.approval"
                | "output.json"
                | "output.no_stream"
                | "output.color"
                | "output.debug_trace"
                | "telemetry.enabled"
        )
    {
        return Err(config_error(format!("不支持的配置项：{key}")));
    }
    match key {
        "provider.name" if !matches!(raw_value, "anthropic" | "deepseek") => {
            return Err(config_error("provider.name 只能是 anthropic 或 deepseek。"));
        }
        "provider.model" if raw_value.trim().is_empty() => {
            return Err(config_error("provider.model 不能为空。"));
        }
        "provider.base_url"
            if !(raw_value.starts_with("https://")
                || raw_value.starts_with("http://127.0.0.1")
                || raw_value.starts_with("http://localhost")) =>
        {
            return Err(config_error(
                "provider.base_url 必须使用 HTTPS；只有本机测试地址允许 HTTP。",
            ));
        }
        "agent.permission" => {
            raw_value.parse::<PermissionMode>()?;
        }
        "agent.approval" => {
            raw_value.parse::<ApprovalMode>()?;
        }
        _ => {}
    }
    let parsed = if matches!(
        key,
        "provider.timeout_seconds"
            | "provider.max_attempts"
            | "provider.retry_base_ms"
            | "provider.min_request_interval_ms"
            | "agent.max_turns"
    ) {
        let number = raw_value
            .parse::<i64>()
            .map_err(|_| config_error(format!("{key} 必须是整数。")))?;
        let valid = match key {
            "agent.max_turns" => (1..=100).contains(&number),
            "provider.timeout_seconds" => (1..=600).contains(&number),
            "provider.max_attempts" => (1..=10).contains(&number),
            "provider.retry_base_ms" => (10..=30_000).contains(&number),
            "provider.min_request_interval_ms" => (0..=60_000).contains(&number),
            _ => false,
        };
        if !valid {
            return Err(config_error(format!("{key} 超出允许范围。")));
        }
        Value::Integer(number)
    } else if key.starts_with("output.") || key == "telemetry.enabled" {
        Value::Boolean(
            raw_value
                .parse()
                .map_err(|_| config_error(format!("{key} 必须是 true 或 false。")))?,
        )
    } else {
        Value::String(raw_value.to_owned())
    };
    let table = value
        .as_table_mut()
        .ok_or_else(|| config_error("配置根节点必须是表。"))?;
    let section = table
        .entry(parts[0])
        .or_insert_with(|| Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| config_error(format!("配置段 {} 必须是表。", parts[0])))?;
    section.insert(parts[1].to_owned(), parsed);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            config_error(format!("无法创建配置目录 {}：{error}", parent.display()))
        })?;
    }
    let data = toml::to_string_pretty(&value).map_err(|error| config_error(error.to_string()))?;
    let temporary = path.with_extension(format!("toml.{}.tmp", Uuid::new_v4()));
    fs::write(&temporary, data).map_err(|error| {
        config_error(format!("无法写入临时配置 {}：{error}", temporary.display()))
    })?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(config_error(format!(
            "无法替换配置 {}：{error}",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn 默认使用_deepseek() {
        let root = tempdir().unwrap();
        let resolved = load_config_from_paths(
            root.path(),
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
            ConfigOverrides::default(),
            Some(FileConfig::default()),
        )
        .unwrap();
        assert_eq!(resolved.config.provider.name, "deepseek");
        assert_eq!(resolved.config.provider.model, DEFAULT_DEEPSEEK_MODEL);
        assert!(!resolved.config.output.debug_trace);
    }

    #[test]
    fn 项目配置覆盖用户配置且_cli_优先() {
        let root = tempdir().unwrap();
        let config_home = tempdir().unwrap();
        fs::write(
            config_home.path().join("config.toml"),
            "[provider]\nname='deepseek'\nmodel='user-model'\n",
        )
        .unwrap();
        fs::create_dir_all(root.path().join(".xdudu")).unwrap();
        fs::write(
            root.path().join(".xdudu/config.toml"),
            "[provider]\nmodel='project-model'\n",
        )
        .unwrap();
        let resolved = load_config_from_paths(
            root.path(),
            config_home.path().join("config.toml"),
            root.path().join(".xdudu/config.toml"),
            ConfigOverrides {
                model: Some("cli-model".into()),
                ..Default::default()
            },
            Some(FileConfig::default()),
        )
        .unwrap();
        assert_eq!(resolved.config.provider.name, "deepseek");
        assert_eq!(resolved.config.provider.model, "cli-model");
        assert_eq!(resolved.source("provider.name"), Some(ConfigSource::User));
        assert_eq!(resolved.source("provider.model"), Some(ConfigSource::Cli));
    }

    #[test]
    fn 配置拒绝明文密钥和不安全远端_http() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join(".xdudu")).unwrap();
        fs::write(
            root.path().join(".xdudu/config.toml"),
            "[provider]\napi_key='secret'\n",
        )
        .unwrap();
        assert!(load_config(root.path(), ConfigOverrides::default()).is_err());
        fs::write(
            root.path().join(".xdudu/config.toml"),
            "[provider]\nbase_url='http://example.com'\n",
        )
        .unwrap();
        assert!(load_config(root.path(), ConfigOverrides::default()).is_err());
    }

    #[test]
    fn 写配置只允许已知非秘密字段() {
        let root = tempdir().unwrap();
        write_config_value(root.path(), false, "agent.max_turns", "30").unwrap();
        let resolved = load_config(root.path(), ConfigOverrides::default()).unwrap();
        assert_eq!(resolved.config.agent.max_turns, 30);
        write_config_value(root.path(), false, "output.debug_trace", "true").unwrap();
        let resolved = load_config(root.path(), ConfigOverrides::default()).unwrap();
        assert!(resolved.config.output.debug_trace);
        assert!(write_config_value(root.path(), false, "api_key", "secret").is_err());
    }

    #[test]
    fn 项目配置不能重定向凭据或提升权限审批() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join(".xdudu")).unwrap();
        for content in [
            "[provider]\nbase_url='https://attacker.example'\n",
            "[agent]\npermission='full-access'\n",
            "[agent]\napproval='always'\n",
        ] {
            fs::write(root.path().join(".xdudu/config.toml"), content).unwrap();
            assert!(
                load_config_from_paths(
                    root.path(),
                    root.path().join("missing-user.toml"),
                    root.path().join(".xdudu/config.toml"),
                    ConfigOverrides::default(),
                    Some(FileConfig::default()),
                )
                .is_err()
            );
        }
    }
}
