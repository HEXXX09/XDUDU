//! 统一错误模型与进程退出码。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// 错误类别与既有 CLI 退出码约定保持一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorKind {
    UserError,
    ValidationError,
    PermissionDenied,
    ProviderError,
    ToolError,
    ConfigError,
}

impl ErrorKind {
    /// 返回 CLI 进程退出码。
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::UserError => 1,
            Self::ValidationError | Self::ConfigError => 2,
            Self::PermissionDenied => 3,
            Self::ProviderError => 4,
            Self::ToolError => 5,
        }
    }
}

/// XDUDU 可序列化的统一错误。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct XduduError {
    pub kind: ErrorKind,
    pub message: String,
    pub retryable: bool,
    pub details: Value,
}

impl XduduError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: false,
            details: Value::Object(Default::default()),
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ValidationError, message)
    }

    pub fn provider(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind: ErrorKind::ProviderError,
            message: message.into(),
            retryable,
            details: Value::Object(Default::default()),
        }
    }

    pub fn tool(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ToolError, message)
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub const fn exit_code(&self) -> u8 {
        self.kind.exit_code()
    }
}

impl From<std::io::Error> for XduduError {
    fn from(error: std::io::Error) -> Self {
        Self::tool(error.to_string())
    }
}

impl From<serde_json::Error> for XduduError {
    fn from(error: serde_json::Error) -> Self {
        Self::validation(format!("JSON 数据无效：{error}"))
    }
}

pub type XduduResult<T> = Result<T, XduduError>;
