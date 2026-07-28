//! 显式权限矩阵，避免通过数值大小比较造成意外放行。

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{XduduError, XduduResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionLevel {
    ReadOnly,
    WriteFiles,
    RunSafeCommands,
    Network,
    FullAccess,
}

impl PermissionLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WriteFiles => "write-files",
            Self::RunSafeCommands => "run-safe-commands",
            Self::Network => "network",
            Self::FullAccess => "full-access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    ReadOnly,
    #[default]
    AutoSafe,
    FullAccess,
}

impl PermissionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::AutoSafe => "auto-safe",
            Self::FullAccess => "full-access",
        }
    }

    pub const fn allows(self, level: PermissionLevel) -> bool {
        match self {
            Self::ReadOnly => matches!(level, PermissionLevel::ReadOnly),
            Self::AutoSafe => matches!(
                level,
                PermissionLevel::ReadOnly
                    | PermissionLevel::WriteFiles
                    | PermissionLevel::RunSafeCommands
            ),
            Self::FullAccess => true,
        }
    }

    pub fn allowed_levels(self) -> &'static [PermissionLevel] {
        const READ_ONLY: &[PermissionLevel] = &[PermissionLevel::ReadOnly];
        const AUTO_SAFE: &[PermissionLevel] = &[
            PermissionLevel::ReadOnly,
            PermissionLevel::WriteFiles,
            PermissionLevel::RunSafeCommands,
        ];
        const FULL_ACCESS: &[PermissionLevel] = &[
            PermissionLevel::ReadOnly,
            PermissionLevel::WriteFiles,
            PermissionLevel::RunSafeCommands,
            PermissionLevel::Network,
            PermissionLevel::FullAccess,
        ];
        match self {
            Self::ReadOnly => READ_ONLY,
            Self::AutoSafe => AUTO_SAFE,
            Self::FullAccess => FULL_ACCESS,
        }
    }
}

impl FromStr for PermissionMode {
    type Err = XduduError;

    fn from_str(value: &str) -> XduduResult<Self> {
        match value {
            "read-only" => Ok(Self::ReadOnly),
            "auto-safe" => Ok(Self::AutoSafe),
            "full-access" => Ok(Self::FullAccess),
            _ => Err(XduduError::validation(format!(
                "非法权限模式：{value}。可选值：read-only、auto-safe、full-access。"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 权限矩阵严格匹配() {
        assert!(PermissionMode::ReadOnly.allows(PermissionLevel::ReadOnly));
        assert!(!PermissionMode::ReadOnly.allows(PermissionLevel::WriteFiles));
        assert!(PermissionMode::AutoSafe.allows(PermissionLevel::RunSafeCommands));
        assert!(!PermissionMode::AutoSafe.allows(PermissionLevel::Network));
        assert!(PermissionMode::FullAccess.allows(PermissionLevel::FullAccess));
    }

    #[test]
    fn 拒绝未知权限模式() {
        assert!("unknown".parse::<PermissionMode>().is_err());
    }
}
