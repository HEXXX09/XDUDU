//! 安装、配置、凭据和工作区诊断。

use std::{env, path::Path};

use serde::Serialize;
use xdudu_core::{ConfigOverrides, KeyringSecretStore, SecretStore, XduduError, load_config};

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorCheck {
    name: &'static str,
    status: CheckStatus,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorReport {
    version: &'static str,
    ok: bool,
    checks: Vec<DoctorCheck>,
}

fn executable_on_path(executable: &Path) -> bool {
    let Some(parent) = executable.parent() else {
        return false;
    };
    env::split_paths(&env::var_os("PATH").unwrap_or_default()).any(|path| path == parent)
}

fn command_on_path(name: &str) -> bool {
    env::split_paths(&env::var_os("PATH").unwrap_or_default()).any(|directory| {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return true;
        }
        cfg!(windows) && directory.join(format!("{name}.exe")).is_file()
    })
}

fn environment_secret_present(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.to_string_lossy().trim().is_empty())
}

pub async fn run_doctor(
    cwd: &Path,
    overrides: ConfigOverrides,
    json: bool,
) -> Result<u8, XduduError> {
    let mut checks = Vec::new();
    let resolved = load_config(cwd, overrides)?;
    checks.push(DoctorCheck {
        name: "config",
        status: CheckStatus::Pass,
        message: format!(
            "配置有效；Provider={}，模型={}",
            resolved.config.provider.name, resolved.config.provider.model
        ),
    });

    match cwd.canonicalize() {
        Ok(path) => checks.push(DoctorCheck {
            name: "workspace",
            status: CheckStatus::Pass,
            message: format!("工作区可访问：{}", path.display()),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "workspace",
            status: CheckStatus::Fail,
            message: format!("工作区不可访问：{error}"),
        }),
    }

    let provider = &resolved.config.provider.name;
    let environment_name = match provider.as_str() {
        "deepseek" => "DEEPSEEK_API_KEY",
        _ => "ANTHROPIC_API_KEY",
    };
    let environment_secret = env::var_os(environment_name);
    if environment_secret_present(environment_secret.as_deref()) {
        checks.push(DoctorCheck {
            name: "credential",
            status: CheckStatus::Pass,
            message: format!("{provider} 密钥来自环境变量。"),
        });
    } else {
        match KeyringSecretStore.get(provider).await {
            Ok(Some(_)) => checks.push(DoctorCheck {
                name: "credential",
                status: CheckStatus::Pass,
                message: format!("{provider} 密钥来自系统凭据存储。"),
            }),
            Ok(None) => checks.push(DoctorCheck {
                name: "credential",
                status: CheckStatus::Warn,
                message: format!("未配置 {provider} 密钥；运行 xdudu auth login {provider}。"),
            }),
            Err(error) => checks.push(DoctorCheck {
                name: "credential",
                status: CheckStatus::Warn,
                message: format!("系统凭据存储不可用：{}", error.message),
            }),
        }
    }

    match env::current_exe() {
        Ok(path) if executable_on_path(&path) => checks.push(DoctorCheck {
            name: "installation",
            status: CheckStatus::Pass,
            message: format!("当前二进制目录已在 PATH：{}", path.display()),
        }),
        Ok(path) => checks.push(DoctorCheck {
            name: "installation",
            status: CheckStatus::Warn,
            message: format!(
                "当前二进制目录不在 PATH：{}；可运行 cargo install --path crates/xdudu-cli --locked --force。",
                path.display()
            ),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "installation",
            status: CheckStatus::Warn,
            message: format!("无法定位当前二进制：{error}"),
        }),
    }

    checks.push(DoctorCheck {
        name: "cargo",
        status: if command_on_path("cargo") {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        message: if command_on_path("cargo") {
            "Cargo 已在 PATH。".into()
        } else {
            "Cargo 不在 PATH；已安装二进制仍可运行，源码开发需配置 $HOME/.cargo/bin。".into()
        },
    });

    match xdudu_core::SqliteSessionStore::new(cwd) {
        Ok(_) => checks.push(DoctorCheck {
            name: "database",
            status: CheckStatus::Pass,
            message: "会话数据库可访问。".into(),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "database",
            status: CheckStatus::Fail,
            message: format!("会话数据库不可访问：{}", error.message),
        }),
    }

    checks.push(DoctorCheck {
        name: "telemetry",
        status: if resolved.config.telemetry.enabled {
            CheckStatus::Warn
        } else {
            CheckStatus::Pass
        },
        message: if resolved.config.telemetry.enabled {
            "遥测已显式开启（默认关闭）。".into()
        } else {
            "遥测保持默认关闭，XDUDU 不发送任何数据。".into()
        },
    });

    let ok = !checks
        .iter()
        .any(|check| matches!(check.status, CheckStatus::Fail));
    let report = DoctorReport {
        version: env!("CARGO_PKG_VERSION"),
        ok,
        checks,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(XduduError::from)?
        );
    } else {
        println!("XDUDU v{} 诊断结果", report.version);
        for check in &report.checks {
            let marker = match check.status {
                CheckStatus::Pass => "通过",
                CheckStatus::Warn => "提示",
                CheckStatus::Fail => "失败",
            };
            println!("[{marker}] {}：{}", check.name, check.message);
        }
    }
    Ok(if ok { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_检测只匹配完整目录() {
        let executable = Path::new("/tmp/xdudu-bin/xdudu");
        // 该函数只比较标准 PATH 分段，不使用字符串前缀。
        let result = executable_on_path(executable);
        assert_eq!(
            result,
            env::split_paths(&env::var_os("PATH").unwrap_or_default())
                .any(|path| path == Path::new("/tmp/xdudu-bin"))
        );
    }

    #[test]
    fn 空环境变量不算有效密钥() {
        assert!(!environment_secret_present(None));
        assert!(!environment_secret_present(Some(std::ffi::OsStr::new(
            "  "
        ))));
        assert!(environment_secret_present(Some(std::ffi::OsStr::new(
            "secret"
        ))));
    }
}
