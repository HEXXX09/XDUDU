//! API 密钥的安全封装、系统凭据存储和环境变量优先解析。

use std::{env, fmt};

use async_trait::async_trait;
use zeroize::{Zeroize, Zeroizing};

use crate::error::{ErrorKind, XduduError, XduduResult};

const KEYRING_SERVICE: &str = "xdudu";

pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(value: impl Into<String>) -> XduduResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(XduduError::new(
                ErrorKind::ConfigError,
                "API Key 不能为空。",
            ));
        }
        Ok(Self(Zeroizing::new(value)))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub fn masked(&self) -> String {
        let suffix = self
            .0
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        format!("****{suffix}")
    }

    pub fn into_exposed(mut self) -> String {
        std::mem::take(&mut *self.0)
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([已脱敏])")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[已脱敏]")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSource {
    Environment,
    SystemStore,
}

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, provider: &str) -> XduduResult<Option<SecretString>>;
    async fn set(&self, provider: &str, value: SecretString) -> XduduResult<()>;
    async fn delete(&self, provider: &str) -> XduduResult<bool>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringSecretStore;

fn entry(service: &str, provider: &str) -> XduduResult<keyring::Entry> {
    keyring::Entry::new(service, provider).map_err(|error| {
        XduduError::new(
            ErrorKind::ConfigError,
            format!("无法访问系统凭据存储：{error}"),
        )
    })
}

#[async_trait]
impl SecretStore for KeyringSecretStore {
    async fn get(&self, provider: &str) -> XduduResult<Option<SecretString>> {
        let provider = provider.to_owned();
        tokio::task::spawn_blocking(move || {
            match entry(KEYRING_SERVICE, &provider)?.get_password() {
                Ok(value) => SecretString::new(value).map(Some),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(XduduError::new(
                    ErrorKind::ConfigError,
                    format!("读取系统凭据失败：{error}"),
                )),
            }
        })
        .await
        .map_err(|error| {
            XduduError::new(ErrorKind::ConfigError, format!("凭据任务失败：{error}"))
        })?
    }

    async fn set(&self, provider: &str, mut value: SecretString) -> XduduResult<()> {
        let provider = provider.to_owned();
        tokio::task::spawn_blocking(move || {
            let result = entry(KEYRING_SERVICE, &provider)?
                .set_password(value.expose())
                .map_err(|error| {
                    XduduError::new(ErrorKind::ConfigError, format!("保存系统凭据失败：{error}"))
                });
            value.0.zeroize();
            result
        })
        .await
        .map_err(|error| {
            XduduError::new(ErrorKind::ConfigError, format!("凭据任务失败：{error}"))
        })?
    }

    async fn delete(&self, provider: &str) -> XduduResult<bool> {
        let provider = provider.to_owned();
        tokio::task::spawn_blocking(move || {
            match entry(KEYRING_SERVICE, &provider)?.delete_credential() {
                Ok(()) => Ok(true),
                Err(keyring::Error::NoEntry) => Ok(false),
                Err(error) => Err(XduduError::new(
                    ErrorKind::ConfigError,
                    format!("删除系统凭据失败：{error}"),
                )),
            }
        })
        .await
        .map_err(|error| {
            XduduError::new(ErrorKind::ConfigError, format!("凭据任务失败：{error}"))
        })?
    }
}

fn env_name(provider: &str) -> XduduResult<&'static str> {
    match provider {
        "anthropic" => Ok("ANTHROPIC_API_KEY"),
        "deepseek" => Ok("DEEPSEEK_API_KEY"),
        "openai-compatible" => Ok("OPENAI_API_KEY"),
        _ => Err(XduduError::new(
            ErrorKind::ConfigError,
            format!("不支持的 Provider：{provider}"),
        )),
    }
}

pub async fn resolve_secret(
    provider: &str,
    store: &dyn SecretStore,
) -> XduduResult<(SecretString, SecretSource)> {
    let variable = env_name(provider)?;
    if let Ok(value) = env::var(variable) {
        return Ok((SecretString::new(value)?, SecretSource::Environment));
    }
    match store.get(provider).await {
        Ok(Some(value)) => return Ok((value, SecretSource::SystemStore)),
        Ok(None) => {}
        Err(error) => {
            return Err(XduduError::new(
                ErrorKind::ConfigError,
                format!(
                    "{variable} 未设置，且系统凭据存储不可用：{}。请设置环境变量或运行：xdudu auth login {provider}",
                    error.message
                ),
            ));
        }
    }
    Err(XduduError::new(
        ErrorKind::ConfigError,
        format!(
            "{variable} 未设置，系统凭据中也没有 {provider} 密钥。请运行：xdudu auth login {provider}"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use super::*;

    #[derive(Default)]
    struct MemoryStore(Mutex<HashMap<String, String>>);

    struct FailingStore;

    #[async_trait]
    impl SecretStore for MemoryStore {
        async fn get(&self, provider: &str) -> XduduResult<Option<SecretString>> {
            self.0
                .lock()
                .unwrap()
                .get(provider)
                .cloned()
                .map(SecretString::new)
                .transpose()
        }

        async fn set(&self, provider: &str, value: SecretString) -> XduduResult<()> {
            self.0
                .lock()
                .unwrap()
                .insert(provider.into(), value.expose().into());
            Ok(())
        }

        async fn delete(&self, provider: &str) -> XduduResult<bool> {
            Ok(self.0.lock().unwrap().remove(provider).is_some())
        }
    }

    #[async_trait]
    impl SecretStore for FailingStore {
        async fn get(&self, _provider: &str) -> XduduResult<Option<SecretString>> {
            Err(XduduError::new(
                ErrorKind::ConfigError,
                "测试凭据后端不可用",
            ))
        }

        async fn set(&self, _provider: &str, _value: SecretString) -> XduduResult<()> {
            unreachable!()
        }

        async fn delete(&self, _provider: &str) -> XduduResult<bool> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn 密钥显示始终脱敏且存储可读写删除() {
        let store = MemoryStore::default();
        let secret = SecretString::new("sk-test-1234").unwrap();
        assert_eq!(format!("{secret}"), "[已脱敏]");
        assert_eq!(format!("{secret:?}"), "SecretString([已脱敏])");
        assert_eq!(secret.masked(), "****1234");
        store.set("deepseek", secret).await.unwrap();
        assert_eq!(
            store.get("deepseek").await.unwrap().unwrap().expose(),
            "sk-test-1234"
        );
        assert!(store.delete("deepseek").await.unwrap());
        assert!(store.get("deepseek").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 凭据后端不可用时仍提供环境变量和登录指引() {
        let error = resolve_secret("deepseek", &FailingStore).await.unwrap_err();
        assert!(error.message.contains("DEEPSEEK_API_KEY"));
        assert!(error.message.contains("xdudu auth login deepseek"));
        assert!(error.message.contains("系统凭据存储不可用"));
    }
}
