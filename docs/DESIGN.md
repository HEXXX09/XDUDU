# XDUDU 详细设计

> 当前版本：Rust-only v0.5.0。本文描述已经实现并通过本地验收的设计边界。

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
Idle → Planning → Acting → Observing
                 ↑         │
                 └─────────┘

任意运行态 → Completed | Incomplete | Interrupted | Error
```

Agent 将状态、文本增量、工具开始、工具结束、用量和告警发送为 `AgentEvent`。Renderer 只是消费者，不能改变 Agent 领域状态。达到最大轮次或模型长度截断均为 `Incomplete`。

## 8. Renderer

CLI Renderer 有三种输出策略：

| 模式 | 行为 |
| --- | --- |
| 默认 | 文本增量立即写终端，工具事件给出状态提示 |
| `--no-stream` | 缓存文本，在完成点统一输出 |
| `--json` | 每个事件一行 JSON，末尾输出运行结果 |

`--no-color` 和 `NO_COLOR` 禁用颜色。JSON 字段不包含 ANSI，并适合管道消费。CLI 的打印错误也不得包含 Secret 原文。

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

`file_read` 仅访问工作区内路径并提供范围、截断和 SHA-256。`file_write` 支持 `expectedSha256` 冲突保护和原子写入。`terminal_exec` 只接受程序名与参数数组，不经过 shell，输出有界且可超时取消。

权限使用显式矩阵，新增级别必须默认拒绝。项目配置、提示词和模型输出都不能提升 CLI 选择的权限。

副作用分为 `none`、`workspace_write` 和 `process_execution`。默认 Gate 为拒绝；CLI 依据 `ask`、`never`、`always` 装配实现。`ask` 只有在可交互且非 JSON 的终端中读取确认，待审批输入先脱敏再展示。审批决定附加到工具结果并保存到会话。

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

`redact_text` 和 `redact_value` 覆盖 `sk-`、GitHub Token、Bearer Token、PEM 私钥以及 key、token、secret、password、authorization 等敏感结构字段。会话保存、Renderer 和顶层错误共用该边界，避免只在 UI 层遮盖。

`JsonChangeLedger` 保存 `file_write` 的前镜像、前后 SHA-256、会话 ID、工具调用 ID 和状态。账本失败时文件写入回滚。`undo` 在操作前重新校验工作区真实路径和当前哈希；不匹配时返回冲突错误，不覆盖用户的新修改。当前实现只覆盖 `file_write`，终端命令的外部副作用不可通用撤销。

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

1. 配置、凭据脱敏、审批、账本、事件、SSE 和重试单元测试；
2. MockProvider 驱动的 Agent 多轮、状态和会话测试；
3. 路径逃逸、符号链接、哈希冲突、命令注入和权限测试；
4. 本机临时 HTTP 服务验证两个 Provider 的请求与流式协议；
5. 真实 CLI 进程验证参数、stdin、输出模式、doctor、审批、撤销和退出码；
6. CI 在 macOS、Linux 和 Windows 运行质量门禁。

默认测试不使用真实 API Key，不请求公网模型。

## 13. 发布与演进约束

源码安装基线是 `cargo install --path crates/xdudu-cli --locked --force`。CI 执行 fmt、Clippy、全目标测试、Release 构建和安装检查；Release 工作流按平台打包二进制并生成 SHA-256。

后续约束：Provider 扩展当前冻结；未来 fallback 不得跨工具副作用边界；审批发生在输入校验之后、副作用之前；MCP 和插件必须进入统一 ToolRegistry；SQLite 替换 JSON 时保持 `SessionStore` 与 `ChangeLedger` 边界；Computer Use 在审批、审计、恢复和跨平台发布成熟前不进入主线。
