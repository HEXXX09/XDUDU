# XDUDU 详细设计

> 当前版本：Rust-only v0.7.0。本文描述已经实现并通过本地验收的设计边界。

## 1. 依赖方向

```text
xdudu-cli → xdudu-core
CLI → Config + SecretStore + ProviderFactory
Agent → Provider trait + ToolRegistry + SessionStore + EventSink
Provider wrapper → 具体 Provider
Tool → PermissionMode + 工作区策略
ToolRegistry → ApprovalGate + ChangeLedger
Renderer → AgentEvent
```

核心库不读取终端输入、不输出 ANSI、不依赖具体界面。CLI 负责命令解析、依赖装配、交互输入和事件渲染。

## 2. 配置解析

`load_config` 接收工作目录和 CLI 覆盖项，按以下顺序覆盖：

```text
默认值 → 用户 TOML → 项目 TOML → 环境变量 → CLI
```

最终优先级即 CLI、环境、项目、用户、默认值。`ResolvedConfig` 同时保存每个点路径的 `ConfigSource`。

当前配置模型：

```text
provider.name
provider.model
provider.base_url
provider.timeout_seconds
provider.max_attempts
provider.retry_base_ms
provider.min_request_interval_ms
agent.max_turns
agent.permission
agent.approval
output.json
output.no_stream
output.color
```

关键校验：Provider 只能是 `anthropic` 或 `deepseek`；模型非空；轮次、超时、重试和节流必须在限定范围；Base URL 要求 HTTPS，本机回环地址例外；TOML 中发现 key、token、secret 等秘密字段时拒绝加载。项目配置不能设置 Base URL，也不能提升用户或默认权限、审批级别。

`config set` 只写白名单字段，先解析和校验用户输入，再写同目录临时文件并重命名。CLI 覆盖不会被隐式写回磁盘。

## 3. 凭据模型

`SecretStore` 隔离平台凭据实现：

```rust
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, provider: &str) -> XduduResult<Option<SecretString>>;
    async fn set(&self, provider: &str, value: SecretString) -> XduduResult<()>;
    async fn delete(&self, provider: &str) -> XduduResult<()>;
}
```

`KeyringSecretStore` 通过系统凭据服务读写，`auth login` 使用隐藏输入。运行时查找顺序是 Provider 专用环境变量优先、系统凭据其次。

改名兼容策略为“新名称优先、旧配置只读、旧凭据一次迁移”：优先读取 `xdudu` 凭据服务，缺失时读取 `xycli` 并立刻写入新的 `xdudu` 项；后续启动只访问新项。配置和会话可从旧位置读取，但更新统一写入 `.xdudu`。

`SecretString` 使用可清零内存容器；Debug 和 Display 只输出脱敏内容。状态和配置命令只报告来源或是否存在，绝不输出原文。系统凭据不可用时返回可操作提示，不创建明文 secret 文件。

## 4. Provider Factory

`DefaultProviderFactory` 接收已经解析的 Provider 配置和 Secret：

1. 校验 Provider、模型、URL 和超时；
2. 创建 Anthropic 或 DeepSeek 客户端；
3. 使用 `RetryingProvider` 包装具体实例；
4. 注入最大尝试次数、基础退避和最小请求间隔；
5. 以 trait object 交给 Agent。

Agent 不根据字符串判断厂商，厂商协议差异只存在于 Provider 目录。

## 5. Provider 接口和流

`Provider` 同时支持完整响应和流式响应。默认流实现可以把非流式结果转为统一事件，具体 Provider 覆盖 `stream_chat` 实现真实 SSE。

统一流事件包括文本增量、完成的响应和厂商无关错误。SSE 解码器允许网络块在任意字节位置切分，并以空行识别完整事件。

Anthropic 按 content block 索引聚合文本、工具名和 `input_json_delta`；DeepSeek 按 choice/tool-call 索引聚合 `delta.content`、函数名和 arguments。收到终止事件后生成一个完整 `ProviderResponse`，再由 Agent 决定是否执行工具。

以下情况视为协议错误：

- 流结束但缺少正常终止标记；
- 工具参数 JSON 不完整或无法解析；
- 必需字段缺失；
- HTTP 成功但事件格式不符合协议。

## 6. 安全重试和节流

重试边界固定为一次模型请求：

```text
等待最小请求间隔
  → 发起 Provider 请求
    → 成功：返回
    → 可重试且尚未产生有效输出：退避后重试
    → 不可重试、已输出或次数耗尽：返回错误
```

连接失败、超时、408、409、429 和 5xx 可重试；400、401、403、404、配置错误、Schema 错误和内容错误不可重试。退避为带抖动的指数增长并尊重 `Retry-After`，所有等待接受取消信号。

工具执行不在重试闭包中。上一轮成功执行工具后，下一次模型调用是新的请求；其网络重试不会重新执行上一轮工具。SSE 已发送文本后发生错误时直接失败，避免终端收到重复前缀。

## 7. Agent 运行时与事件

`AgentRunConfig` 输入 prompt、model、max_turns、cwd、Provider、ToolRegistry、SessionStore、权限模式、取消令牌、EventSink 和可选会话 ID。

状态机：

```text
Idle → Planning → Acting → Observing → Reflecting
                 ↑                       │
                 └───────────────────────┘

任意运行态 → Completed | Incomplete | Interrupted | Error
```

首轮模型请求处于 `Planning`，执行工具时进入 `Acting`，工具结果落入上下文后进入 `Observing`，下一次携带观察结果请求模型时进入 `Reflecting`。系统提示词只提供简短工具索引；结构化输入 Schema 只通过 Provider 的 `tools` 字段发送，避免重复占用上下文。模型不输出内部思维链，只通过工具调用、进度和最终结论表现 ReAct 循环。

Agent 将状态、文本增量、工具开始、工具进度、工具结束、用量和告警发送为 `AgentEvent`。每个工具调用拥有容量为 64 的非阻塞进度通道；通道满时丢弃中间更新，工具执行不能因 UI 变慢而阻塞。同批工具中一旦出现拒绝，后续副作用工具会被阻止，只读工具仍可执行。Renderer 只是消费者，不能改变 Agent 领域状态。达到最大轮次、模型长度截断、未执行工具调用或仍有未解决的工具失败均为 `Incomplete`。

## 8. Renderer

CLI Renderer 有四种输出策略：

| 模式 | 行为 |
| --- | --- |
| 交互 TTY | 全屏对话时间线；文本、工具、状态和用量事件原位更新，底部 Composer 独占输入 |
| 非交互默认 | 文本增量立即写终端，工具事件给出已脱敏的阶段提示 |
| `--no-stream` | 缓存助手文本，但仍实时显示工具阶段 |
| `--json` | 每个事件一行 JSON，末尾输出运行结果 |

所有模式先经过统一脱敏。交互 TTY 不允许核心层直接写终端，TUI 根据 `AgentEvent` 维护静态对话、流式回复、工具活动和底部状态；不支持 TUI 的终端自动降级为行式输入。`--no-color` 和 `NO_COLOR` 禁用颜色。JSON 字段不包含 ANSI，并适合管道消费。CLI 的打印错误也不得包含 Secret 原文。

## 9. 工具和权限

ToolRegistry 的固定顺序是：

```text
查找工具
  → 检查 PermissionMode
  → 工具输入严格校验
  → 对副作用调用 ApprovalGate
  → 创建超时与子取消令牌
  → 执行工具
  → 写入变更账本
  → 归一化 ToolResult
```

`file_read` 仅访问工作区内路径并提供范围、截断和 SHA-256。`file_write` 支持 `expectedSha256` 冲突保护和事务写入。`terminal_exec` 只接受程序名与参数数组，不经过 shell，输出有界且可超时取消。

`search_text` 在 Rust 进程内完成 literal/regex 搜索、Glob 过滤和 `.gitignore` 遍历。路径、单文件、扫描总量、返回行和输出 JSON 均有独立上限；列号按 Unicode 字符从 1 计算。

`git_status` 固定执行 `git status --porcelain=v2 --branch -z` 并解析为结构化状态。`git_diff` 固定禁用外部 diff 与 textconv，并在 `--` 后追加经过校验的工作区相对路径。两者是可信只读工具，不经过进程执行审批，但仍校验 Git 仓库根位于工作区。

`apply_patch` 先完整解析全部 unified diff，再读取前镜像、精确应用 hunk、检查并发哈希并准备 v2 事务。写入通过同目录临时文件和原子替换完成；任一提交或账本失败都会整批回滚，回滚本身失败则事务标记为 `Conflict`。

权限使用显式矩阵，新增级别必须默认拒绝。项目配置、提示词和模型输出都不能提升 CLI 选择的权限。

副作用分为 `none`、`workspace_write`、`process_execution` 和 `network_access`。默认 Gate 为拒绝；CLI 依据 `ask`、`never`、`always` 装配实现。`ask` 的交互选择包括 `once`、`session` 和 `always`：单次规则不缓存；会话规则以会话 UUID、工具名和副作用类型为键；永久规则以工具名和副作用类型为键，原子写入用户级 `approval-rules.json`。待审批输入先脱敏再展示，作用域随 `ApprovalRecord` 保存到会话；旧记录缺少作用域时按 `once` 读取。

`xdudu approval list/revoke/clear` 用于管理永久规则。`never` 始终拒绝，显式全局 `--approval always` 仍表示调用方批准全部当前运行操作，不依赖细粒度规则。

`web_search` 和 `web_fetch` 都是本地只读、网络有副作用的工具，因此三种权限模式都可以提出调用；其 `NetworkAccess` 副作用始终进入 ApprovalGate，由 `ask`、`never`、`always` 单独决定是否联网。`web_search` 使用固定搜索入口，查询、结果数量、响应大小和超时均有硬限制，只返回可继续抓取的 HTTPS 链接。Agent 遇到通用查询或本地搜索无结果时，可在 ReAct 循环中执行“网络搜索 → 抓取相关来源 → 综合回答”，网络拒绝或无可靠资料时才结束并说明限制。

`web_fetch` 仅发起 GET，不转发代理凭据并禁用自动重定向；每一跳重新解析 DNS、拒绝任一非公网地址并把已验证地址固定给 reqwest，防止 DNS 重绑定。系统 DNS 使用代理 Fake-IP 时通过固定公网 DoH 获取真实记录，结果仍接受同一公网检查。响应只接受 HTML、纯文本和 JSON，大小、超时、重定向次数均有硬限制。

## 10. 会话、脱敏与变更账本

`SqliteSessionStore` 将会话写入 `.xdudu/xdudu.db`：

- SQLite 使用 bundled 构建，避免三平台系统库差异；
- WAL、外键、忙等待、Schema 版本和事务统一初始化；
- 旧 JSON 会话事务导入，导入失败整体回滚，原文件始终保留；
- 工作区独占锁阻止跨进程并发覆盖，并由操作系统在崩溃后释放；
- 遗留运行状态自动恢复为 `interrupted`；
- 工具执行前保存 `pending`，结果未知时恢复为 `cancelled` 且不自动重放；
- `session list/show/resume` 提供查询和恢复入口；
- 输入预算超限后压缩较早上下文，但完整原始消息仍保存在数据库。

交互 TTY 支持在当前界面输入 `/resume` 打开最近会话选择器，使用上下键选择、Enter 恢复、Esc 取消；`/resume <UUID>` 可直接恢复。恢复只读取当前工作区数据库，并重建用户与助手时间线；工具结果仍保存在会话中，但不会作为普通对话块重复展示。

## 10.1 Plan 领域基础

M7 显式计划使用独立的 `Plan`、`PlanStep`、`PlanRevision` 和 `PlanStore`，与单次 Agent 请求内部的隐藏 ReAct 状态分离。`Plan` 当前 Schema 版本为 3，除 revision 和审阅信息外还包含 executionVersion、当前步骤、开始时间和暂停原因；步骤通过 `PlanStepAttempt` 保存每次尝试、工具调用 ID、结果、错误及逐项完成证据。每个内容 revision 保存完整不可变快照。

计划依赖必须引用同一计划中存在的步骤，不允许自身依赖、重复依赖或环，单个计划最多 100 个步骤。领域层只允许显式状态迁移：

```text
draft → pending_approval → approved → running
  ↑            │                         ├─ completed
  └────────────┘                         ├─ failed
                                        └─ cancelled
```

步骤状态为 `pending`、`ready`、`running`、`completed`、`failed`、`blocked`、`skipped` 或 `cancelled`。只有全部依赖已完成或跳过的待执行步骤才会被识别为可运行；计划存在未完成步骤时不能标记为完成。

SQLite Schema v4 在 `plans` 表同时保存 revision 和 execution_version，并保留 `plan_revisions` 快照表。审阅阶段使用 revision/status 乐观并发保护；执行阶段使用 revision/execution_version/status 的条件更新，在同一事务中写入 Plan 和 Session 检查点。落盘前对目标、描述、完成条件、审阅原因、执行摘要、证据和错误统一脱敏。旧会话中的 `plan` JSON 字段仅保留兼容，不再重复保存完整 Plan。

## 10.2 结构化计划生成

M7.2 使用独立于普通 ReAct Agent 的规划 Prompt。规划请求只向 Provider 暴露一个协议工具 `submit_plan`；该工具不会注册进 `ToolRegistry`，不能读取文件、执行命令、访问网络或产生其他副作用。

`submit_plan` 返回步骤 key、标题、描述、依赖 key 和完成条件。解析器要求 Provider 以 `tool_calls` 结束、只调用一次正确工具且不夹带普通文本；DTO 启用未知字段拒绝。随后运行时将稳定 key 映射为 UUID，校验数量、文本上限、重复 key、未知依赖、自身依赖、重复依赖和循环依赖。只有全部校验通过，才通过 `PlanStore` 原子创建 `draft` 计划；截断、内容过滤或协议错误不会留下计划记录。

M7.3 在结构化生成之后加入独立的整份 Plan 审阅服务。`/plan <目标>` 生成 Draft 后提交为 `pending_approval`；用户可在 TUI 中批准、请求自然语言修订或拒绝，Esc 只关闭界面。自然语言修订使用仅供 Provider 的 `revise_plan` 协议，新版本重新生成 Step UUID、revision 加一并重新审批。审批和修订都使用 revision/status 乐观并发保护，协议失败不会改变原计划。

Plan 审批与工具审批是两条不同边界：`approved` 仅表示用户认可方案，不授予文件、进程或网络能力。`PlanExecutor` 按原始顺序串行选择 DAG 中 Ready 的步骤，真实工具仍由 ToolRegistry、PermissionMode 和 ApprovalGate 控制。模型必须单独调用内部 `complete_step`，并为全部完成条件提交唯一、有效的证据索引；普通文本、未处理工具失败或缺失证据都不能完成步骤。

执行中的 Provider 错误、审批拒绝、协议错误、轮次上限和 Ctrl+C 会立即把当前 attempt 与 Plan 持久化为失败/中断和 Paused。启动时发现 Running Plan 会将运行中 attempt 标记 Interrupted、步骤标记 Blocked，并保留现场；恢复只在用户明确选择后创建新 attempt，绝不自动重放未知结果工具。

`redact_text` 和 `redact_value` 覆盖 `sk-`、GitHub Token、Bearer Token、PEM 私钥以及 key、token、secret、password、authorization 等敏感结构字段。会话保存、Renderer 和顶层错误共用该边界，避免只在 UI 层遮盖。

`JsonChangeLedger` 的新记录使用 `schemaVersion: 2`，一个工具调用只生成一个事务 ID。事务保存文件操作、原权限、前后镜像和 SHA-256，并经历 `Prepared → Applying → Applied`；失败可进入 `RolledBack`，用户冲突进入 `Conflict`，撤销后进入 `Undone`。启动恢复会检查未完成事务全部文件的当前哈希；只在内容可判定时恢复前镜像，不能判定时不覆盖用户文件。`undo` 先预检全部后哈希再整批恢复，同时保持 v1 单文件账本兼容。终端命令与网络访问的外部副作用不可通用撤销。

## 11. CLI 命令和退出码

```text
xdudu [prompt]
xdudu run [prompt]
xdudu auth login|status|logout
xdudu config show|explain|set|path
xdudu session list|show|resume
xdudu doctor
xdudu undo [--change <uuid>]
xdudu --version
```

| 退出码 | 含义 |
| ---: | --- |
| 0 | 正常完成 |
| 1 | 未完成、中断或一般运行错误 |
| 2 | 参数、配置或启动校验错误 |
| 3 | 顶层权限错误 |
| 4 | Provider、协议或网络错误 |
| 5 | 工具致命错误 |

## 12. 测试分层

1. 配置、凭据脱敏、审批、事务账本、进度事件、SSE 和重试单元测试；
2. MockProvider 驱动的 Agent 多轮、状态和会话测试；
3. 路径逃逸、符号链接、哈希冲突、补丁原子性、Git 边界、SSRF、命令注入和权限测试；
4. 本机临时 HTTP 服务验证两个 Provider 的请求与流式协议；
5. 真实 CLI 进程验证参数、stdin、输出模式、doctor、审批、撤销和退出码；
6. CI 在 macOS、Linux 和 Windows 运行质量门禁。

默认测试不使用真实 API Key，不请求公网模型。

## 13. 发布与演进约束

源码安装基线是 `cargo install --path crates/xdudu-cli --locked --force`。CI 执行 fmt、Clippy、全目标测试、Release 构建和安装检查；Release 工作流按平台打包二进制并生成 SHA-256。

后续约束：Provider 扩展当前冻结；未来 fallback 不得跨工具副作用边界；审批发生在输入校验之后、副作用之前；M7 Plan 只编排现有受控工具，不能创建绕过权限的执行通道；MCP 和插件必须进入统一 ToolRegistry；Computer Use 在审批、审计、恢复和跨平台发布成熟前不进入主线。
