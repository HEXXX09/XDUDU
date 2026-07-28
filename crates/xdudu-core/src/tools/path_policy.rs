//! 工作区路径沙箱，阻止绝对路径、`..` 与符号链接逃逸。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{XduduError, XduduResult};

fn outside_error(input: &Path) -> XduduError {
    XduduError::tool(format!("路径超出工作区，已拒绝访问：{}", input.display()))
}

async fn canonical_root(cwd: &Path) -> XduduResult<PathBuf> {
    fs::canonicalize(cwd)
        .await
        .map_err(|error| XduduError::tool(format!("工作区路径无效：{error}")))
}

fn ensure_within(root: &Path, target: PathBuf, input: &Path) -> XduduResult<PathBuf> {
    if target.starts_with(root) {
        Ok(target)
    } else {
        Err(outside_error(input))
    }
}

pub async fn resolve_existing(input: &Path, cwd: &Path) -> XduduResult<PathBuf> {
    let root = canonical_root(cwd).await?;
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    let target = fs::canonicalize(&candidate)
        .await
        .map_err(|error| XduduError::tool(format!("无法访问路径 {}：{error}", input.display())))?;
    ensure_within(&root, target, input)
}

pub async fn resolve_writable(input: &Path, cwd: &Path) -> XduduResult<PathBuf> {
    let root = canonical_root(cwd).await?;
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    let mut ancestor = candidate.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| outside_error(input))?;
    }
    let real_ancestor = fs::canonicalize(ancestor).await.map_err(|error| {
        XduduError::tool(format!("无法解析父目录 {}：{error}", ancestor.display()))
    })?;
    ensure_within(&root, real_ancestor.clone(), input)?;
    let suffix = candidate
        .strip_prefix(ancestor)
        .map_err(|_| outside_error(input))?;
    let target = if suffix.as_os_str().is_empty() {
        real_ancestor
    } else {
        real_ancestor.join(suffix)
    };
    ensure_within(&root, target, input)
}

pub async fn resolve_directory(input: &Path, cwd: &Path) -> XduduResult<PathBuf> {
    let target = resolve_existing(input, cwd).await?;
    if !fs::metadata(&target).await?.is_dir() {
        return Err(XduduError::tool(format!(
            "工作目录不是文件夹：{}",
            input.display()
        )));
    }
    Ok(target)
}
