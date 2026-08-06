# XDUDU 版本兼容与迁移策略

> 目标版本：v1.0.0。本文定义 1.0 之后对 CLI、配置、会话与存储的兼容承诺与迁移路径。

## 1. 兼容承诺（1.0 冻结）

以下约定自 v1.0.0 起冻结，后续版本不得破坏：

| 面 | 冻结内容 |
| --- | --- |
| CLI 命令 | 顶层命令与子命令名称、参数语义、`--help` 输出结构保持稳定；新增命令不得复用已有名称 |
| 退出码 | `0` 完成 / `1` 未完成·中断 / `2` 参数·配置 / `3` 权限 / `4` Provider / `5` 工具，永久稳定 |
| 配置键 | `provider.*`、`agent.*`、`output.*`、`telemetry.enabled` 键名与取值范围稳定；新增键必须带默认值且旧配置不写也能启动 |
| 环境变量 | `XDUDU_*` 优先、`XYCLI_*` 只读兼容（新位置缺失时） |
| 会话存储 | SQLite 库结构由 `schema_migrations` 版本化；新版本只追加迁移，不重写旧行 |
| 数据格式 | 记忆、计划、审批规则的序列化字段使用 `camelCase` 并保留 `schemaVersion`，读取路径兼容旧版本 |

## 2. 配置迁移策略

- 配置优先级固定：`CLI > 环境 > 项目 > 用户 > 默认`，每个最终值记录来源；
- 新增配置键：只允许带默认值（`#[serde(default)]`），旧配置文件缺失该键时按默认值启动；
- 键值变化（改名/拆分）：保留旧键的兼容读取，新键优先；至少保留一个主版本周期；
- 废弃：先警告（`config show` 标注 deprecated），一个主版本后移除；
- 写回：`config set` 只写白名单键，临时文件 + 原子重命名，CLI 覆盖不隐式落盘。

### 已知历史迁移

- v0.4.0：`XYCLI_*`/`.xycli` → `XDUDU_*`/`.xdudu`（新位置优先，旧位置只读兼容）；
- v0.5.0：JSON 会话 → SQLite 事务导入，旧文件保留为备份；
- Plan Schema v1→v3、SQLite Schema v2→v5：单事务迁移，损坏记录整体回滚。

## 3. 会话与存储迁移

- SQLite 使用 WAL + 外键 + `schema_migrations` 版本表；迁移必须：
  - 单事务内完成，失败整体回滚；
  - 兼容读取旧行（如 `deserialize_plan_compatible` 补填缺失字段）；
  - 不删除旧数据，迁移标记成功后旧内容才视为已升级；
- 变更账本：v1 单文件记录与 v2 多文件事务共存读取，`undo` 对两者透明；
- 跨目录：会话绑定工作区 `cwd`，恢复时校验一致（防跨项目上下文污染）。

## 4. 升级、降级与回滚

- **升级**：`cargo install --path crates/xdudu-cli --locked --force` 或下载发布归档（`xdudu-<tag>-<平台>.tar.gz/zip` + `.sha256` 校验）；
- **降级**：安装旧版本归档即可；数据库向前兼容（旧二进制忽略新迁移标记之外的新表/新列——SQLite 容忍多余列；若旧版本不识别新 schema 结构，以发布说明为准）；
- **配置回滚**：配置为单文件，升级前可备份 `~/.config/xdudu/config.toml` 与项目 `.xdudu/config.toml`；
- **文件变更回滚**：`xdudu undo` 整批恢复 Agent 文件变更；人工修改冲突时整批拒绝；
- **来源证明**：发布归档附带 SHA-256 校验和；GitHub Actions 生成 SBOM/attestation 后，`gh attestation verify` 可验证来源。

## 5. 发布纪律

- 版本号遵循语义化版本；`v*` tag 触发 Release 工作流；
- 三平台 CI（fmt/clippy/test/release/install 验证）全绿才允许打 tag；
- 发布说明必须列出：破坏性变更、配置迁移步骤、数据库迁移版本、升级/降级路径。

## 6. 诊断

`xdudu doctor` 提供 config / workspace / credential / installation / cargo / database / telemetry 七项检查，用于安装与升级后自检；`--json` 输出结构化报告供脚本消费。
