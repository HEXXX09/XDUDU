//! 声明式插件清单。
//!
//! M8 插件不能把代码加载进 XDUDU 进程，只能声明受统一安全边界管理的
//! MCP Server。签名字段仅保存和校验元数据，不代表已经建立信任。

use std::{env, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{McpServerConfig, XduduError, XduduResult};

const PLUGIN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginSignatureInfo {
    pub algorithm: String,
    pub key_id: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginManifest {
    #[serde(default = "plugin_schema_version")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<PluginSignatureInfo>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    #[serde(skip)]
    pub source_path: PathBuf,
}

fn plugin_schema_version() -> u32 {
    PLUGIN_SCHEMA_VERSION
}

impl PluginManifest {
    pub fn validate(&self) -> XduduResult<()> {
        if self.schema_version != PLUGIN_SCHEMA_VERSION {
            return Err(XduduError::validation(format!(
                "插件 {} 使用不支持的 Schema 版本。",
                self.id
            )));
        }
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .chars()
                .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || value == '-')
        {
            return Err(XduduError::validation(
                "插件 ID 必须为 1～64 个小写字母、数字或连字符。",
            ));
        }
        if self.name.is_empty() || self.name.len() > 128 {
            return Err(XduduError::validation("插件名称必须为 1～128 字节。"));
        }
        if self.version.is_empty() || self.version.len() > 64 {
            return Err(XduduError::validation("插件版本必须为 1～64 字节。"));
        }
        if self.description.len() > 4096 || self.mcp_servers.len() > 16 {
            return Err(XduduError::validation(
                "插件描述或 MCP Server 数量超过限制。",
            ));
        }
        if let Some(value) = &self.sha256
            && (value.len() != 64 || !value.chars().all(|character| character.is_ascii_hexdigit()))
        {
            return Err(XduduError::validation("插件 sha256 必须是 64 位十六进制。"));
        }
        if let Some(signature) = &self.signature
            && (signature.algorithm.is_empty()
                || signature.algorithm.len() > 64
                || signature.key_id.is_empty()
                || signature.key_id.len() > 256
                || signature.value.is_empty()
                || signature.value.len() > 16 * 1024)
        {
            return Err(XduduError::validation("插件签名元数据无效。"));
        }
        for server in &self.mcp_servers {
            server.validate()?;
        }
        Ok(())
    }
}

pub fn plugin_directory() -> XduduResult<PathBuf> {
    let root = if let Some(value) = env::var_os("XDUDU_CONFIG_HOME") {
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
    Ok(root.join("plugins"))
}

pub fn load_plugin_manifests() -> XduduResult<Vec<PluginManifest>> {
    let directory = plugin_directory()?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(XduduError::from(error)),
    };
    let mut manifests = Vec::new();
    for entry in entries {
        let entry = entry.map_err(XduduError::from)?;
        let metadata = entry.metadata().map_err(XduduError::from)?;
        let path = entry.path();
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("toml")
        {
            continue;
        }
        if metadata.len() > 256 * 1024 {
            return Err(XduduError::validation(format!(
                "插件清单过大：{}",
                path.display()
            )));
        }
        let raw = fs::read_to_string(&path).map_err(XduduError::from)?;
        let mut manifest: PluginManifest = toml::from_str(&raw).map_err(|error| {
            XduduError::validation(format!("插件清单 {} 无效：{error}", path.display()))
        })?;
        manifest.source_path = path;
        manifest.validate()?;
        manifests.push(manifest);
        if manifests.len() > 64 {
            return Err(XduduError::validation("插件数量超过 64。"));
        }
    }
    manifests.sort_by(|left, right| left.id.cmp(&right.id));
    for pair in manifests.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(XduduError::validation(format!(
                "插件 ID 重复：{}",
                pair[0].id
            )));
        }
    }
    Ok(manifests)
}

pub fn save_plugin_manifest(manifest: &PluginManifest) -> XduduResult<PathBuf> {
    manifest.validate()?;
    let directory = plugin_directory()?;
    fs::create_dir_all(&directory).map_err(XduduError::from)?;
    let path = if manifest.source_path.as_os_str().is_empty() {
        directory.join(format!("{}.toml", manifest.id))
    } else {
        manifest.source_path.clone()
    };
    if path.parent() != Some(directory.as_path()) {
        return Err(XduduError::validation("插件清单必须位于用户插件目录。"));
    }
    let data = toml::to_string_pretty(manifest)
        .map_err(|error| XduduError::validation(format!("插件清单序列化失败：{error}")))?;
    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, data).map_err(XduduError::from)?;
    fs::rename(&temporary, &path).map_err(XduduError::from)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 插件清单拒绝动态入口和无效摘要() {
        let unknown = toml::from_str::<PluginManifest>(
            "schemaVersion=1\nid='demo'\nname='Demo'\nversion='1.0.0'\nentry='bad.so'\n",
        );
        assert!(unknown.is_err());
        let mut manifest: PluginManifest = toml::from_str(
            "schemaVersion=1\nid='demo'\nname='Demo'\nversion='1.0.0'\nsha256='bad'\n",
        )
        .unwrap();
        manifest.source_path = PathBuf::new();
        assert!(manifest.validate().is_err());
    }
}
