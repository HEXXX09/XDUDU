//! `apply_patch`：严格解析 unified diff，并以事务方式增删改多个文本文件。

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::fs;
use uuid::Uuid;

use crate::{
    ChangeSetDraft, ChangeSetFileDraft, ChangeSetStatus, FileOperation, SideEffectKind,
    permission::PermissionLevel,
};

use super::path_policy::resolve_writable;
use super::{
    Tool, ToolContext, ToolDefinition, ToolProgressUpdate, ToolResult, object,
    reject_unknown_fields, required_string,
};

const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILES: usize = 20;
const MAX_HUNKS: usize = 200;
const MAX_CHANGED_LINES: usize = 100_000;

pub struct ApplyPatchTool;

#[derive(Debug, Clone)]
struct HunkLine {
    kind: char,
    text: String,
    no_newline: bool,
}

#[derive(Debug, Clone)]
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
struct FilePatch {
    old_path: Option<PathBuf>,
    new_path: Option<PathBuf>,
    hunks: Vec<Hunk>,
}

impl FilePatch {
    fn path(&self) -> &Path {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .expect("解析后的补丁必须包含路径")
    }

    fn operation(&self) -> FileOperation {
        match (&self.old_path, &self.new_path) {
            (None, Some(_)) => FileOperation::Created,
            (Some(_), None) => FileOperation::Deleted,
            _ => FileOperation::Modified,
        }
    }
}

fn safe_patch_path(raw: &str) -> Result<Option<PathBuf>, String> {
    let raw = raw.split('\t').next().unwrap_or(raw).trim();
    if raw == "/dev/null" {
        return Ok(None);
    }
    let raw = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    let path = Path::new(raw);
    if raw.is_empty()
        || raw.len() > 4096
        || raw.contains('\\')
        || raw.as_bytes().get(1) == Some(&b':')
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!("补丁包含不安全路径：{raw}"));
    }
    Ok(Some(path.to_path_buf()))
}

fn parse_range(value: Option<regex::Match<'_>>, default: usize) -> Result<usize, String> {
    value
        .map(|value| value.as_str().parse::<usize>())
        .transpose()
        .map_err(|_| "hunk 行号超出范围。".to_owned())
        .map(|value| value.unwrap_or(default))
}

fn parse_patch(patch: &str) -> Result<Vec<FilePatch>, String> {
    if patch.is_empty() || patch.len() > MAX_PATCH_BYTES {
        return Err(format!(
            "patch 必须是 1 到 {MAX_PATCH_BYTES} 字节的字符串。"
        ));
    }
    let forbidden = [
        "GIT binary patch",
        "Binary files ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "old mode ",
        "new mode ",
        "new file mode ",
        "deleted file mode ",
    ];
    if patch
        .lines()
        .any(|line| forbidden.iter().any(|prefix| line.starts_with(prefix)))
    {
        return Err("补丁包含二进制、重命名、复制或文件模式元数据，当前版本不支持。".into());
    }
    let hunk_header =
        Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("固定正则有效");
    let lines = patch.lines().collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut index = 0;
    let mut total_hunks = 0;
    let mut changed_lines = 0;
    while index < lines.len() {
        if !lines[index].starts_with("--- ") {
            index += 1;
            continue;
        }
        let old_path = safe_patch_path(&lines[index][4..])?;
        index += 1;
        let Some(new_header) = lines.get(index).and_then(|line| line.strip_prefix("+++ ")) else {
            return Err("每个 --- 文件头后必须紧跟 +++ 文件头。".into());
        };
        let new_path = safe_patch_path(new_header)?;
        if old_path.is_none() && new_path.is_none() {
            return Err("补丁文件的新旧路径不能同时为 /dev/null。".into());
        }
        if let (Some(old), Some(new)) = (&old_path, &new_path)
            && old != new
        {
            return Err("当前版本不支持通过补丁重命名文件。".into());
        }
        index += 1;
        let mut hunks = Vec::new();
        while index < lines.len() && !lines[index].starts_with("--- ") {
            if lines[index].starts_with("diff --git ") || lines[index].starts_with("index ") {
                index += 1;
                continue;
            }
            let Some(captures) = hunk_header.captures(lines[index]) else {
                if lines[index].is_empty() {
                    index += 1;
                    continue;
                }
                return Err(format!("补丁包含无法识别的行：{}", lines[index]));
            };
            let old_start = parse_range(captures.get(1), 0)?;
            let old_count = parse_range(captures.get(2), 1)?;
            let new_start = parse_range(captures.get(3), 0)?;
            let new_count = parse_range(captures.get(4), 1)?;
            index += 1;
            let mut hunk_lines: Vec<HunkLine> = Vec::new();
            while index < lines.len()
                && !lines[index].starts_with("@@ ")
                && !lines[index].starts_with("--- ")
                && !lines[index].starts_with("diff --git ")
            {
                let line = lines[index];
                if line == r"\ No newline at end of file" {
                    let Some(previous) = hunk_lines.last_mut() else {
                        return Err("无末尾换行标记前缺少补丁内容。".into());
                    };
                    previous.no_newline = true;
                    index += 1;
                    continue;
                }
                let mut chars = line.chars();
                let kind = chars
                    .next()
                    .ok_or_else(|| "hunk 中存在空行标记错误。".to_owned())?;
                if !matches!(kind, ' ' | '+' | '-') {
                    break;
                }
                if matches!(kind, '+' | '-') {
                    changed_lines += 1;
                }
                if changed_lines > MAX_CHANGED_LINES {
                    return Err(format!("补丁变更行数不能超过 {MAX_CHANGED_LINES}。"));
                }
                hunk_lines.push(HunkLine {
                    kind,
                    text: chars.collect(),
                    no_newline: false,
                });
                index += 1;
            }
            let actual_old = hunk_lines.iter().filter(|line| line.kind != '+').count();
            let actual_new = hunk_lines.iter().filter(|line| line.kind != '-').count();
            if actual_old != old_count || actual_new != new_count {
                return Err(format!(
                    "hunk 行数与头部不一致：声明 -{old_count}/+{new_count}，实际 -{actual_old}/+{actual_new}。"
                ));
            }
            total_hunks += 1;
            if total_hunks > MAX_HUNKS {
                return Err(format!("单次补丁最多包含 {MAX_HUNKS} 个 hunk。"));
            }
            hunks.push(Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                lines: hunk_lines,
            });
        }
        if hunks.is_empty() {
            return Err("每个文件补丁至少需要一个 hunk。".into());
        }
        files.push(FilePatch {
            old_path,
            new_path,
            hunks,
        });
        if files.len() > MAX_FILES {
            return Err(format!("单次补丁最多修改 {MAX_FILES} 个文件。"));
        }
    }
    if files.is_empty() {
        return Err("没有找到有效的 unified diff 文件头。".into());
    }
    let mut unique_paths = HashSet::new();
    if files
        .iter()
        .any(|file| !unique_paths.insert(file.path().to_path_buf()))
    {
        return Err("同一个文件在单次补丁中只能出现一次。".into());
    }
    Ok(files)
}

fn split_text(bytes: &[u8]) -> Result<(Vec<String>, bool, &'static str), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "补丁只支持 UTF-8 文本文件。")?;
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let normalized = text.replace("\r\n", "\n");
    let final_newline = normalized.ends_with('\n');
    let mut lines = normalized
        .split('\n')
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if final_newline {
        lines.pop();
    }
    Ok((lines, final_newline, newline))
}

fn apply_hunks(source: Option<&[u8]>, file: &FilePatch) -> Result<Option<Vec<u8>>, String> {
    if file.operation() == FileOperation::Created && source.is_some() {
        return Err(format!("创建目标已经存在：{}", file.path().display()));
    }
    if file.operation() != FileOperation::Created && source.is_none() {
        return Err(format!("补丁目标不存在：{}", file.path().display()));
    }
    let (source_lines, source_final_newline, newline) = match source {
        Some(source) => split_text(source)?,
        None => (Vec::new(), true, "\n"),
    };
    let mut output = Vec::new();
    let mut cursor = 0;
    let mut final_newline = if file.operation() == FileOperation::Created {
        true
    } else {
        source_final_newline
    };
    for hunk in &file.hunks {
        let start = hunk.old_start.saturating_sub(1);
        if start < cursor || start > source_lines.len() {
            return Err(format!("hunk 起始行超出范围：{}", file.path().display()));
        }
        output.extend(source_lines[cursor..start].iter().cloned());
        if output.len() != hunk.new_start.saturating_sub(1) {
            return Err(format!(
                "hunk 新文件起始行不连续：{}。",
                file.path().display()
            ));
        }
        cursor = start;
        for line in &hunk.lines {
            match line.kind {
                ' ' => {
                    if source_lines.get(cursor) != Some(&line.text) {
                        return Err(format!(
                            "补丁上下文不匹配：{} 第 {} 行。",
                            file.path().display(),
                            cursor + 1
                        ));
                    }
                    output.push(line.text.clone());
                    cursor += 1;
                }
                '-' => {
                    if source_lines.get(cursor) != Some(&line.text) {
                        return Err(format!(
                            "补丁删除内容不匹配：{} 第 {} 行。",
                            file.path().display(),
                            cursor + 1
                        ));
                    }
                    cursor += 1;
                }
                '+' => {
                    output.push(line.text.clone());
                }
                _ => unreachable!(),
            }
        }
        if cursor == source_lines.len() {
            final_newline = hunk
                .lines
                .iter()
                .rev()
                .find(|line| line.kind != '-')
                .is_none_or(|line| !line.no_newline);
        }
        debug_assert_eq!(
            hunk.old_count,
            hunk.lines.iter().filter(|line| line.kind != '+').count()
        );
        debug_assert_eq!(
            hunk.new_count,
            hunk.lines.iter().filter(|line| line.kind != '-').count()
        );
    }
    output.extend(source_lines[cursor..].iter().cloned());
    if file.operation() == FileOperation::Deleted {
        if !output.is_empty() {
            return Err(format!(
                "删除文件补丁应用后仍有内容：{}。",
                file.path().display()
            ));
        }
        return Ok(None);
    }
    let mut text = output.join(newline);
    if final_newline {
        text.push_str(newline);
    }
    Ok(Some(text.into_bytes()))
}

fn hash(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

#[derive(Debug)]
struct PreparedFile {
    path: PathBuf,
    relative: PathBuf,
    operation: FileOperation,
    pre_image: Option<Vec<u8>>,
    post_image: Option<Vec<u8>>,
    pre_hash: Option<String>,
    post_hash: Option<String>,
    pre_mode: Option<u32>,
}

async fn restore_file(file: &PreparedFile, post: bool) -> Result<(), std::io::Error> {
    let image = if post {
        file.post_image.as_deref()
    } else {
        file.pre_image.as_deref()
    };
    if let Some(bytes) = image {
        let parent = file.path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "目标缺少父目录")
        })?;
        fs::create_dir_all(parent).await?;
        let temporary = file
            .path
            .with_extension(format!("xdudu-patch-{}", Uuid::new_v4()));
        fs::write(&temporary, bytes).await?;
        #[cfg(unix)]
        if let Some(mode) = file.pre_mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode)).await?;
        }
        #[cfg(windows)]
        if fs::try_exists(&file.path).await.unwrap_or(false) {
            fs::remove_file(&file.path).await?;
        }
        fs::rename(temporary, &file.path).await
    } else {
        match fs::remove_file(&file.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

async fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, std::io::Error> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

async fn rollback_files(
    prepared: &[PreparedFile],
    indices: impl IntoIterator<Item = usize>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for index in indices {
        if let Err(error) = restore_file(&prepared[index], false).await {
            errors.push(format!("{}：{error}", prepared[index].relative.display()));
        }
    }
    errors
}

fn rollback_suffix(errors: &[String]) -> String {
    if errors.is_empty() {
        String::new()
    } else {
        format!("；回滚失败：{}", errors.join("；"))
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch",
            description: "以事务方式应用标准 unified diff，可一次创建、修改或删除多个 UTF-8 文本文件；任一失败时整体回滚。",
            input_schema: json!({
                "type":"object",
                "properties":{"patch":{"type":"string","minLength":1,"maxLength":MAX_PATCH_BYTES}},
                "required":["patch"],
                "additionalProperties":false
            }),
            permission_level: PermissionLevel::WriteFiles,
            side_effect: SideEffectKind::WorkspaceWrite,
            default_timeout: Duration::from_secs(60),
        }
    }

    fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let map = object(input)?;
        let mut issues = Vec::new();
        reject_unknown_fields(map, &["patch"], &mut issues);
        if let Some(patch) = required_string(map, "patch", MAX_PATCH_BYTES, &mut issues)
            && let Err(error) = parse_patch(patch)
        {
            issues.push(error);
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    async fn preflight(&self, input: &Value, context: &ToolContext) -> Option<ToolResult> {
        let patch = input["patch"].as_str().unwrap_or_default();
        let parsed = match parse_patch(patch) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Some(ToolResult::failure(
                    "INVALID_PATCH",
                    error,
                    context.started_at,
                    json!({}),
                ));
            }
        };
        context.report_progress(ToolProgressUpdate::counted(
            "preflight",
            0,
            Some(parsed.len() as u64),
            "files",
        ));
        for (index, file) in parsed.iter().enumerate() {
            if context.cancellation.is_cancelled() {
                return Some(ToolResult::failure(
                    "TOOL_ABORTED",
                    "补丁预检已取消。",
                    context.started_at,
                    json!({}),
                ));
            }
            let relative = file.path();
            let resolved = match resolve_writable(relative, &context.cwd).await {
                Ok(path) => path,
                Err(error) => {
                    return Some(ToolResult::failure(
                        "PATH_OUTSIDE_WORKSPACE",
                        error.message,
                        context.started_at,
                        json!({"path":relative}),
                    ));
                }
            };
            if fs::symlink_metadata(&resolved)
                .await
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Some(ToolResult::failure(
                    "UNSUPPORTED_FILE_TYPE",
                    format!("补丁拒绝符号链接：{}", relative.display()),
                    context.started_at,
                    json!({"path":relative}),
                ));
            }
            let source = match read_optional(&resolved).await {
                Ok(source) => source,
                Err(error) => {
                    return Some(ToolResult::failure(
                        "PATCH_READ_ERROR",
                        error.to_string(),
                        context.started_at,
                        json!({"path":relative}),
                    ));
                }
            };
            if let Err(error) = apply_hunks(source.as_deref(), file) {
                return Some(ToolResult::failure(
                    "PATCH_CONTEXT_MISMATCH",
                    error,
                    context.started_at,
                    json!({"path":relative}),
                ));
            }
            context.report_progress(ToolProgressUpdate::counted(
                "preflight",
                (index + 1) as u64,
                Some(parsed.len() as u64),
                "files",
            ));
        }
        None
    }

    async fn execute(&self, input: Value, context: ToolContext) -> ToolResult {
        let patch = input["patch"].as_str().unwrap_or_default();
        let parsed = match parse_patch(patch) {
            Ok(parsed) => parsed,
            Err(error) => {
                return ToolResult::failure("INVALID_PATCH", error, context.started_at, json!({}));
            }
        };
        context.report_progress(ToolProgressUpdate::counted(
            "preparing",
            0,
            Some(parsed.len() as u64),
            "files",
        ));
        let mut prepared = Vec::new();
        for (file_index, file) in parsed.into_iter().enumerate() {
            if context.cancellation.is_cancelled() {
                return ToolResult::failure(
                    "TOOL_ABORTED",
                    "补丁应用已取消。",
                    context.started_at,
                    json!({}),
                );
            }
            let relative = file.path().to_path_buf();
            let resolved = match resolve_writable(&relative, &context.cwd).await {
                Ok(path) => path,
                Err(error) => {
                    return ToolResult::failure(
                        "PATH_OUTSIDE_WORKSPACE",
                        error.message,
                        context.started_at,
                        json!({"path":relative}),
                    );
                }
            };
            if fs::symlink_metadata(&resolved)
                .await
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return ToolResult::failure(
                    "UNSUPPORTED_FILE_TYPE",
                    format!("补丁拒绝符号链接：{}", relative.display()),
                    context.started_at,
                    json!({"path":relative}),
                );
            }
            let pre_image = match read_optional(&resolved).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return ToolResult::failure(
                        "PATCH_READ_ERROR",
                        error.to_string(),
                        context.started_at,
                        json!({"path":relative}),
                    );
                }
            };
            let post_image = match apply_hunks(pre_image.as_deref(), &file) {
                Ok(image) => image,
                Err(error) => {
                    return ToolResult::failure(
                        "PATCH_CONTEXT_MISMATCH",
                        error,
                        context.started_at,
                        json!({"path":relative}),
                    );
                }
            };
            #[cfg(unix)]
            let pre_mode = match fs::metadata(&resolved).await {
                Ok(metadata) => {
                    use std::os::unix::fs::PermissionsExt;
                    Some(metadata.permissions().mode())
                }
                Err(_) => None,
            };
            #[cfg(not(unix))]
            let pre_mode = None;
            prepared.push(PreparedFile {
                path: resolved,
                relative,
                operation: file.operation(),
                pre_hash: pre_image.as_deref().map(hash),
                post_hash: post_image.as_deref().map(hash),
                pre_image,
                post_image,
                pre_mode,
            });
            context.report_progress(ToolProgressUpdate::counted(
                "preparing",
                (file_index + 1) as u64,
                None,
                "files",
            ));
        }
        for file in &prepared {
            let current = match read_optional(&file.path).await {
                Ok(current) => current,
                Err(error) => {
                    return ToolResult::failure(
                        "PATCH_READ_ERROR",
                        format!("提交前无法读取 {}：{error}", file.relative.display()),
                        context.started_at,
                        json!({"path":file.relative}),
                    );
                }
            };
            if current.as_deref().map(hash) != file.pre_hash {
                return ToolResult::failure(
                    "HASH_MISMATCH",
                    format!("提交前文件发生变化：{}", file.relative.display()),
                    context.started_at,
                    json!({"path":file.relative}),
                );
            }
        }
        let transaction_id = match context
            .change_ledger
            .prepare_change_set(ChangeSetDraft {
                session_id: context.session_id,
                tool_call_id: context.call_id,
                files: prepared
                    .iter()
                    .map(|file| ChangeSetFileDraft {
                        path: file.relative.clone(),
                        operation: file.operation,
                        pre_image: file.pre_image.clone(),
                        post_image: file.post_image.clone(),
                        pre_image_sha256: file.pre_hash.clone(),
                        post_image_sha256: file.post_hash.clone(),
                        pre_mode: file.pre_mode,
                    })
                    .collect(),
            })
            .await
        {
            Ok(id) => id,
            Err(error) => {
                return ToolResult::failure(
                    "CHANGE_LEDGER_ERROR",
                    error.message,
                    context.started_at,
                    json!({}),
                );
            }
        };
        if let Some(id) = transaction_id
            && let Err(error) = context
                .change_ledger
                .set_change_set_status(id, ChangeSetStatus::Applying)
                .await
        {
            return ToolResult::failure(
                "CHANGE_LEDGER_ERROR",
                error.message,
                context.started_at,
                json!({"transactionId":id}),
            );
        }
        let mut committed = Vec::new();
        for (index, file) in prepared.iter().enumerate() {
            let current = match read_optional(&file.path).await {
                Ok(current) => current,
                Err(error) => {
                    let rollback_errors =
                        rollback_files(&prepared, committed.into_iter().rev()).await;
                    if let Some(id) = transaction_id {
                        let _ = context
                            .change_ledger
                            .set_change_set_status(
                                id,
                                if rollback_errors.is_empty() {
                                    ChangeSetStatus::RolledBack
                                } else {
                                    ChangeSetStatus::Conflict
                                },
                            )
                            .await;
                    }
                    return ToolResult::failure(
                        "PATCH_READ_ERROR",
                        format!(
                            "提交期间无法读取 {}：{error}{}",
                            file.relative.display(),
                            rollback_suffix(&rollback_errors)
                        ),
                        context.started_at,
                        json!({"path":file.relative}),
                    );
                }
            };
            if current.as_deref().map(hash) != file.pre_hash {
                let rollback_errors = rollback_files(&prepared, committed.into_iter().rev()).await;
                if let Some(id) = transaction_id {
                    let _ = context
                        .change_ledger
                        .set_change_set_status(
                            id,
                            if rollback_errors.is_empty() {
                                ChangeSetStatus::RolledBack
                            } else {
                                ChangeSetStatus::Conflict
                            },
                        )
                        .await;
                }
                return ToolResult::failure(
                    "HASH_MISMATCH",
                    format!(
                        "提交期间文件发生变化：{}{}",
                        file.relative.display(),
                        rollback_suffix(&rollback_errors)
                    ),
                    context.started_at,
                    json!({"path":file.relative}),
                );
            }
            if let Err(error) = restore_file(file, true).await {
                let mut rollback_errors =
                    rollback_files(&prepared, committed.into_iter().rev()).await;
                if let Err(rollback_error) = restore_file(file, false).await {
                    rollback_errors.push(format!("{}：{rollback_error}", file.relative.display()));
                }
                if let Some(id) = transaction_id {
                    let _ = context
                        .change_ledger
                        .set_change_set_status(
                            id,
                            if rollback_errors.is_empty() {
                                ChangeSetStatus::RolledBack
                            } else {
                                ChangeSetStatus::Conflict
                            },
                        )
                        .await;
                }
                return ToolResult::failure(
                    "PATCH_COMMIT_ERROR",
                    format!(
                        "提交补丁失败，已执行回滚：{error}{}",
                        rollback_suffix(&rollback_errors)
                    ),
                    context.started_at,
                    json!({"path":file.relative}),
                );
            }
            committed.push(index);
            context.report_progress(ToolProgressUpdate::counted(
                "applying",
                (index + 1) as u64,
                Some(prepared.len() as u64),
                "files",
            ));
        }
        if let Some(id) = transaction_id
            && let Err(error) = context
                .change_ledger
                .set_change_set_status(id, ChangeSetStatus::Applied)
                .await
        {
            let rollback_errors = rollback_files(&prepared, (0..prepared.len()).rev()).await;
            let _ = context
                .change_ledger
                .set_change_set_status(
                    id,
                    if rollback_errors.is_empty() {
                        ChangeSetStatus::RolledBack
                    } else {
                        ChangeSetStatus::Conflict
                    },
                )
                .await;
            return ToolResult::failure(
                "CHANGE_LEDGER_ERROR",
                format!(
                    "补丁已写入但无法完成账本，已执行回滚：{}{}",
                    error.message,
                    rollback_suffix(&rollback_errors)
                ),
                context.started_at,
                json!({"transactionId":id}),
            );
        }
        let created = prepared
            .iter()
            .filter(|file| file.operation == FileOperation::Created)
            .count();
        let modified = prepared
            .iter()
            .filter(|file| file.operation == FileOperation::Modified)
            .count();
        let deleted = prepared
            .iter()
            .filter(|file| file.operation == FileOperation::Deleted)
            .count();
        ToolResult::success(
            json!({
                "transactionId": transaction_id,
                "files": prepared.iter().map(|file| json!({
                    "path":file.relative,
                    "operation":file.operation.as_str(),
                    "preImageSha256":file.pre_hash,
                    "postImageSha256":file.post_hash,
                })).collect::<Vec<_>>(),
                "created":created,
                "modified":modified,
                "deleted":deleted,
            }),
            context.started_at,
            json!({"fileCount":prepared.len()}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(source: Option<&[u8]>, patch: &str) -> Option<Vec<u8>> {
        let files = parse_patch(patch).unwrap();
        assert_eq!(files.len(), 1);
        apply_hunks(source, &files[0]).unwrap()
    }

    #[test]
    fn 保留_crlf_并支持删除文件() {
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+three\n";
        assert_eq!(
            applied(Some(b"one\r\ntwo\r\n"), patch).unwrap(),
            b"one\r\nthree\r\n"
        );
        let deletion = "--- a/a.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n";
        assert_eq!(applied(Some(b"one\r\ntwo\r\n"), deletion), None);
    }

    #[test]
    fn 精确保留和改变末尾换行语义() {
        let remove_newline = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old
+new
\\ No newline at end of file
";
        assert_eq!(applied(Some(b"old\n"), remove_newline).unwrap(), b"new");

        let add_newline = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
";
        assert_eq!(applied(Some(b"old"), add_newline).unwrap(), b"new\n");
    }
}
