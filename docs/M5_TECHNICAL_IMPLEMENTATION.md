# XDUDU M5 技术难点与实现细节

本文面向希望阅读、调试和继续维护 XDUDU 的开发者，解释 v0.5.0 M5 的关键实现，而不是只描述产品功能。

## 1. 代码入口

| 文件 | 主要职责 |
| --- | --- |
| `crates/xdudu-core/src/session.rs` | 会话领域对象、状态枚举、`SessionStore` trait 和旧 JSON 兼容读取 |
| `crates/xdudu-core/src/sqlite_session.rs` | SQLite Schema、事务、旧数据迁移、工作区锁和崩溃恢复 |
| `crates/xdudu-core/src/agent.rs` | 会话创建/恢复、工具防重放、Token 预算和上下文压缩 |
| `crates/xdudu-cli/src/main.rs` | SQLite 运行时装配及 `session list/show/resume` 命令 |
| `crates/xdudu-cli/tests/cli.rs` | 真实 CLI 进程、会话查询与恢复 E2E |

核心依赖：

```toml
rusqlite = { version = "0.32.1", features = ["bundled"] }
fs2 = "0.4.3"
```

选择 `bundled` 是为了让 macOS、Linux 和 Windows 编译同一版本的 SQLite，避免用户先安装系统 SQLite 开发包。

## 2. 难点一：同步 SQLite 与异步 Agent

`rusqlite::Connection` 是同步接口，不能在 Tokio 异步任务中直接执行可能阻塞的数据库操作。实现采用以下结构：

```text
异步 SessionStore 方法
  → clone 数据库路径和会话值
  → tokio::task::spawn_blocking
  → 为本次操作打开 Connection
  → 配置 PRAGMA
  → 开启事务
  → 执行 SQL
  → 提交事务
```

这样做有三个原因：

1. 不把不可跨线程共享的 `Connection` 放进全局互斥锁；
2. 不阻塞 Agent 所在的 Tokio worker；
3. 每次连接都经过同一个安全配置入口，不会漏掉外键或超时设置。

连接统一执行：

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
```

并设置五秒 `busy_timeout`。虽然工作区锁已经阻止两个 XDUDU 写进程并行运行，SQLite 事务和忙等待仍用于防御同进程任务、诊断工具或未来只读客户端造成的短暂竞争。

## 3. 难点二：Schema 可演进，同时兼容现有领域模型

M5 没有立刻把 `Message`、`ToolCallRecord` 和 `ApprovalRecord` 拆成大量关系表，而是采用“核心元数据列 + 完整 JSON”：

```text
sessions
├── id
├── title
├── cwd
├── status
├── current_state
├── provider_name
├── model
├── context_summary
├── summarized_message_count
├── session_json
├── created_at
├── updated_at
└── completed_at
```

这是一个刻意的兼容性选择：

- `list` 可以使用索引和元数据排序；
- `show/resume` 可以从完整 JSON 恢复领域对象；
- 现有序列化格式无需一次性大改；
- 以后如果要查询消息或工具调用，可以通过新 Schema 迁移逐步拆表。

`schema_migrations` 记录结构版本，`data_migrations` 记录一次性数据迁移。两者分开是因为“数据库结构已经升级”和“某批旧数据已经导入”不是同一件事。

## 4. 难点三：旧 JSON 必须非破坏迁移

旧会话可能位于：

```text
.xdudu/sessions/json
.xycli/sessions/json
```

迁移实现先读取并解析所有候选文件，然后才开启写事务。这样可以避免已经写入一半时才发现后面的 JSON 损坏。

```text
扫描全部文件
  → JSON 解析
  → 按 Session ID 去重
  → BEGIN TRANSACTION
  → INSERT OR IGNORE
  → 写 data_migrations 标记
  → COMMIT
```

关键不变量：

- 任一 JSON 无法解析时，本轮迁移失败；
- 不会留下部分导入结果；
- 不会写入完成标记；
- 不会删除或重命名原始 JSON；
- 修复损坏文件后可以重新迁移。

写入 SQLite 前复用 `sanitized_session`，所以旧记录即使包含类似 API Key 的文本，也会先脱敏。

## 5. 难点四：跨进程锁不能只依赖进程内 Mutex

旧 `JsonSessionStore` 的 `tokio::sync::Mutex` 只能协调同一进程里的任务。两个终端分别运行 XDUDU 时，各自拥有不同的 Mutex，仍可能覆盖同一会话或变更账本。

M5 使用 `.xdudu/workspace.lock`：

```text
打开锁文件
  → fs2::FileExt::try_lock_exclusive
  → 成功：持有 File 直到 Runtime drop
  → 失败：拒绝启动第二个状态写入进程
```

为什么保存 `File` 而不是只保存路径：文件锁和打开的文件描述符绑定。`WorkspaceLock` 持有 `File`，其 `Drop` 释放锁；即使进程崩溃，操作系统也会回收描述符并释放锁。

当前策略是“一个工作区一个 XDUDU 状态写入进程”，比“允许多个进程同时修改不同会话”更保守，但能同时保护 SQLite 会话和仍使用 JSON 的文件变更账本。

`undo` 不创建 Agent Runtime，因此它会显式获取同一个工作区锁，避免在 Agent 写入文件和账本时并行撤销。

## 6. 难点五：崩溃发生在副作用工具中间

最危险的时间窗口是：

```text
文件已经写入
  → 进程崩溃
  → 会话还没记录工具成功
  → 恢复后模型再次请求同一工具
```

如果只在工具完成后保存记录，就无法区分“从未执行”和“执行了但结果没保存”。

M5 把工具生命周期改为：

```text
1. 保存 ToolCallRecord(status = pending)
2. 提交 SQLite 事务
3. 执行工具
4. 更新为 succeeded / failed / denied
5. 保存工具观察消息
```

下次启动获得工作区锁后，会扫描遗留的 `running` 或 `waiting_approval` 会话：

```text
Session: running → interrupted
AgentLoopState: 任意运行态 → error
ToolCall: pending/running → cancelled
Tool error: 写入“结果未知，不会自动重试”
Messages: 补充对应 ToolResult 错误观察
```

这里不能把未知调用直接标记为 `failed`，因为工具可能实际上已经完成；`cancelled` 表示 XDUDU 无法确认最终状态。恢复逻辑也不会自动重放它，用户应先检查工作区。

## 7. 难点六：上下文压缩不能删除审计历史

`Session.messages` 继续保存全部原始消息。新增两个字段：

```rust
context_summary: String
summarized_message_count: usize
```

前者保存较早上下文摘要，后者表示摘要覆盖了消息数组的前多少项。构造 Provider 请求时使用：

```text
压缩摘要
+ messages[summarized_message_count..]
```

数据库中的 `messages` 不截断，因此：

- `session show` 仍能看到完整历史；
- 可以重新生成更好的摘要；
- 审批和工具调用仍可审计；
- Provider 输入大小与本地记录大小解耦。

### 7.1 Token 估算

不同 Provider 的 tokenizer 不同，目前不额外引入厂商 tokenizer，而是使用偏保守字符估算：

```text
estimated_tokens ≈ 字符数 / 2 + 固定消息开销
```

预算默认是 24,000，并额外预留系统提示词、工具 Schema、输出和协议空间。它不是账单级精确统计，而是防止请求无限增长的安全上界。

### 7.2 选择保留窗口

算法从消息尾部向前累计估算成本：

```text
固定成本 = system + tools + 安全余量
可用尾部预算 = 总预算 - 固定成本 - 现有摘要

从最后一条消息倒序加入
  → 超过尾部预算时停止
  → 如果切在 ToolResult 上，回退到对应 Assistant ToolUse
  → 较早消息生成摘要
```

回退 ToolUse 是必要的。如果 Provider 只收到 ToolResult 而没有对应工具调用 ID，Anthropic 或 DeepSeek 可能拒绝消息结构。

### 7.3 摘要保留内容

本地摘要保留：

- `Session.plan`；
- 用户和助手文本；
- 工具名称；
- 限长后的工具输入；
- 限长后的工具结果与错误。

每项和总摘要都有字符上限，避免“摘要本身无限增长”。发送给 Provider 时会明确说明摘要只是历史上下文，不是新的用户指令。

## 8. 难点七：会话查询不能要求 API Key

`session list` 和 `session show` 只访问本地 SQLite，不应创建 Provider 或读取系统钥匙串。因此 CLI 在创建 Runtime 之前分流：

```text
config/auth/doctor/undo/session list/session show
  → 本地命令直接处理

run/session resume
  → 加载配置和凭据
  → 创建 Provider、ToolRegistry 和 SqliteSessionStore
  → 运行 Agent
```

`session resume` 仍需要调用模型，所以它进入正常 Agent 路径，并继续执行：

- 会话必须存在；
- 当前工作目录必须与原会话相同；
- 追加新的用户消息；
- 清除旧完成时间；
- 状态切回 `running`。

## 9. 本地文件权限

会话可能包含源码片段、命令输出和业务信息。Unix 下以下文件会被收紧为 `0600`：

```text
.xdudu/xdudu.db
.xdudu/xdudu.db-wal
.xdudu/xdudu.db-shm
.xdudu/workspace.lock
```

Windows 使用系统文件 ACL，不套用 Unix mode。数据库内容在写入前仍会经过统一密钥脱敏；文件权限和内容脱敏是两层不同的保护。

## 10. 测试策略

M5 的自动化覆盖：

| 类型 | 验证点 |
| --- | --- |
| SQLite 单元测试 | 创建、更新、读取、倒序列表 |
| 迁移测试 | 旧 JSON 导入、文件保留、损坏数据不部分提交 |
| 安全测试 | API Key 脱敏、Unix `0600` |
| 锁测试 | 第二个工作区实例被拒绝、释放后可重新获取 |
| 恢复测试 | 运行中会话变中断，pending 工具变 cancelled |
| 上下文测试 | 原始消息不删除、计划保留、最近消息保留 |
| CLI E2E | `session list/show/resume` 真实子进程 |
| 全项目回归 | Provider、SSE、审批、撤销和工具安全不退化 |

本地验收命令：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --release --locked
./target/release/xdudu --version
./target/release/xdudu session list
```

## 11. 当前取舍与后续改进

M5 有意保留以下边界：

- `sessions` 仍包含完整 JSON，尚未把消息和工具调用完全关系化；
- Token 预算是保守估算，不是 Provider 官方 tokenizer 的精确值；
- 工作区采用独占写锁，尚未支持多进程分别运行不同会话；
- 文件变更账本仍是 JSON，但已受同一个工作区锁保护；
- 摘要是确定性本地压缩，没有引入额外模型调用。

这些取舍优先保证可恢复、跨平台和不额外消耗 API 额度。未来只有在出现明确的查询、性能或并发需求后，才应通过新的 Schema migration 逐步演进。
