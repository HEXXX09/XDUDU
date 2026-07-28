# XDUDU v0.5.0 会话恢复与上下文设计

## 1. 阶段目标

M5 把会话从“单进程 JSON 记录”升级为“可查询、可恢复、跨进程一致”的本地运行状态，并为长任务建立明确的上下文预算。Provider 扩展不属于本阶段，DeepSeek 继续作为主路径。

## 2. 设计结论

- 主存储使用 bundled SQLite，数据库位于 `.xdudu/xdudu.db`；
- `.xdudu/workspace.lock` 对整个工作区加操作系统独占锁；
- 旧 JSON 会话只迁移、不删除，失败时事务整体回滚；
- 工具调用执行前写入 `pending`，崩溃后不自动重放；
- 原始消息永久保存在数据库，压缩只改变 Provider 输入窗口；
- 上下文摘要使用本地确定性算法，不额外调用模型或消耗额度。

## 3. SQLite Schema

数据库包含：

| 表 | 用途 |
| --- | --- |
| `schema_migrations` | 记录已应用的数据库结构版本 |
| `data_migrations` | 记录旧 JSON 等一次性数据迁移 |
| `sessions` | 保存可查询元数据和完整脱敏会话 JSON |

`sessions` 的索引字段包括会话 ID、标题、工作目录、状态、Agent 状态、Provider、模型、摘要、消息压缩位置和时间；`session_json` 保存完整领域对象。这样既能稳定恢复现有结构，又能避免 CLI 列表反序列化无关记录。

每次创建和更新都使用显式事务。连接统一启用：

```text
foreign_keys = ON
journal_mode = WAL
busy_timeout = 5 秒
```

## 4. 旧数据迁移

首次打开数据库时依次扫描：

```text
.xdudu/sessions/json
.xycli/sessions/json
```

迁移流程：

1. 读取并解析全部 JSON；
2. 按会话 ID 去重；
3. 在单个事务中写入 SQLite；
4. 写入 `legacy_json_sessions_v1` 标记；
5. 提交事务。

任一文件无法解析时不写迁移标记，事务不产生部分结果。原 JSON 文件无论成功或失败都不会删除。

## 5. 跨进程锁和恢复

每个需要修改工作区状态的 XDUDU 进程必须持有独占文件锁。第二个进程会立即收到明确错误，不进入模型或工具执行。

锁由操作系统维护，进程结束后自动释放。新进程获得锁并打开数据库时：

1. 查找 `running` 和 `waiting_approval` 会话；
2. 将会话改为 `interrupted`；
3. 将 `pending` 或 `running` 工具调用改为 `cancelled`；
4. 补充“结果未知、不会自动重试”的工具观察；
5. 事务提交恢复结果。

工具调用正常路径为：

```text
保存 pending
  → 执行工具
    → 保存 success / failed / denied
      → 写入工具观察
```

因此崩溃发生在工具执行前后都不会把未知副作用当成未开始操作而自动重放。

## 6. 会话命令

```bash
xdudu session list [--limit N]
xdudu session show <UUID>
xdudu session resume <UUID> [prompt]
```

`list` 按更新时间倒序；`show` 输出完整 JSON；`resume` 校验工作目录并追加新的用户消息。省略 prompt 时进入交互模式。

## 7. 上下文预算

当前默认输入估算预算为 24,000 Token，并为输出和协议开销预留空间。没有厂商 tokenizer 时使用偏保守的字符估算。

预算计算包含：

- 系统提示词；
- 工具 Schema；
- 压缩摘要；
- 最近完整消息；
- 固定安全余量。

超限时从最近消息向前选择完整窗口，并把较早消息压缩为角色化摘要。摘要保留：

- `Session.plan` 中的当前目标和计划；
- 用户与助手的重要文本；
- 工具名称和受限长度输入；
- 工具结果或错误摘要。

不会删除 `Session.messages`，所以查询、审计和未来重新摘要仍可使用完整历史。

## 8. 安全边界

- SQLite 写入前复用统一脱敏函数；
- Unix 下数据库、WAL、共享内存和锁文件权限收紧为 `0600`；
- API Key 不进入 Schema 或迁移标记；
- 工作区锁不扩大工具权限；
- 恢复会话不能跨工作目录；
- 摘要不是新用户指令，发送给 Provider 时带明确隔离说明；
- 结果未知的副作用调用默认不重试。

## 9. 验收

- SQLite 创建、更新、读取和列表测试；
- 旧 JSON 事务迁移且原文件保留；
- 第二个工作区锁实例被拒绝；
- 运行中会话重开后标记为中断；
- 未完成工具调用标记为取消并补充观察；
- `session list/show/resume` 真实 CLI 测试；
- 长会话压缩后保留计划、最近消息和全部原始记录；
- 格式、Clippy、单元、协议、安全和 CLI E2E 全部通过。

## 10. 后续边界

M6 可以在此基础上增加原生搜索、补丁、Git 和受限 Web 工具。所有新工具仍必须经过统一权限、审批、脱敏、超时、审计和恢复链。
