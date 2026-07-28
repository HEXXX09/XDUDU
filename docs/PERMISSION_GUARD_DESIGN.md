# PermissionGuard 与工具安全边界设计

> 当前生产权限矩阵位于 `crates/xdudu-core/src/permission.rs`，工具执行层位于 `crates/xdudu-core/src/tools/`。

## 1. 目标

模型返回工具调用后、任何副作用发生前，系统必须完成：

1. 工具权限级别检查；
2. 工具输入 Schema 校验；
3. 路径或命令动作级校验；
4. 拒绝结果的会话审计。

单独检查 `permissionLevel` 不足以形成安全边界。例如 `terminal_exec` 即使属于 `run-safe-commands`，仍必须检查具体命令和参数。

## 2. 权限级别与模式

```rust
pub enum PermissionLevel {
    ReadOnly,
    WriteFiles,
    RunSafeCommands,
    Network,
    FullAccess,
}

pub enum PermissionMode {
    ReadOnly,
    AutoSafe,
    FullAccess,
}
```

| 模式 | 允许级别 |
| --- | --- |
| `read-only` | `read-only` |
| `auto-safe` | `read-only`、`write-files`、`run-safe-commands` |
| `full-access` | 全部 |

允许关系采用显式矩阵，不使用数值大小比较。未来增加权限级别时，新级别默认不会被旧模式放行。

## 3. 权限判断接口

`PermissionMode::allows(PermissionLevel)` 使用显式匹配表达式判断。非法 CLI 模式由 clap 在进入运行时前拒绝，不能静默回退到更宽松模式。

## 4. Agent Loop 集成

执行顺序：

```text
Provider 返回 tool call
  → ToolRegistry 查找工具
  → PermissionGuard.evaluate(permissionLevel)
  ├── 拒绝：写入 denied 记录并回传结构化错误
  └── 允许：进入 ToolRegistry.execute()
```

未知工具不参与权限判断，交由 ToolRegistry 返回 `TOOL_NOT_FOUND`。

拒绝记录包含：

- tool call ID；
- 工具名和输入；
- 所需权限级别；
- 当前权限模式；
- 允许级别；
- `status: "denied"`。

## 5. ToolRegistry 输入校验

每个工具同时声明模型可见 JSON Schema 和 Rust 运行时校验逻辑。Registry 在执行入口统一处理：

- 成功：使用已验证的数据执行；
- 失败：返回 `INVALID_TOOL_INPUT` 和字段级问题；
- 不调用工具实现。

## 6. 文件动作级策略

`file_read` 和 `file_write` 始终限制在工作区内。即使使用 `full-access`，文件工具本身也不越界；需要访问工作区外资源时必须通过用户明确授权的其他能力。

防御项：

- 绝对路径越界；
- `../` 路径穿越；
- 工作区内符号链接指向外部；
- 待创建路径的中间目录为外部符号链接；
- 工作区根本身存在平台路径别名。

## 7. 命令动作级策略

`terminal_exec` 不使用 shell。示例：

```json
{
  "command": "git",
  "args": ["status", "--short"]
}
```

以下输入无效：

```json
{ "command": "git status" }
{ "command": "sh", "args": ["-c", "任意脚本"] }
```

第二个示例在 `full-access` 下可以执行，但在 `auto-safe` 下返回 `UNSAFE_COMMAND`。

安全白名单及参数限制定义在 `crates/xdudu-core/src/tools/terminal_exec.rs`。不能把 `cargo test` 视为只读安全命令，因为构建脚本和测试可以执行任意代码。

## 8. 测试矩阵

必须覆盖：

- 三种模式的权限矩阵；
- 非法模式；
- 未知工具；
- Schema 类型错误和未知字段；
- 绝对路径与 `..` 越界；
- 符号链接逃逸；
- shell 命令字符串；
- auto-safe 非白名单命令；
- full-access 显式可执行文件与参数；
- 拒绝调用的 Session 审计状态。

## 9. 当前增强与后续

v0.4.0 已增加副作用审批、审批记录持久化、敏感输出脱敏、文件变更账本和哈希保护撤销。

后续仍需增加：

- 项目级和用户级 allowlist/denylist；
- 更细粒度的命令语义风险扫描；
- 网络、MCP 和插件的统一策略；
- SQLite 跨进程事务与恢复。
