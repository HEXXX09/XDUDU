# XDUDU 系统架构

> 当前基线：Rust-only v0.4.0。旧 TypeScript 实现已退役，可通过 Git 历史审计。

## 1. 系统定位

XDUDU 是运行在开发者本机终端中的 AI 编程 Agent。它接收自然语言任务，通过模型推理、受控工具调用和本地会话持久化完成编码工作，不是常驻 Web 服务。

当前外部边界包括模型 API、系统凭据库、当前工作区文件系统和本地可执行程序。搜索、Web、MCP 和插件属于后续里程碑。

## 2. Rust 工作区

```text
Cargo workspace
├── crates/xdudu-cli
│   ├── main.rs          命令、参数、装配、REPL 和退出码
│   ├── renderer.rs      终端、JSON Lines 和非流式渲染
│   └── doctor.rs        安装、配置、凭据和工作区诊断
└── crates/xdudu-core
    ├── agent.rs         Agent 主循环和事件发送
    ├── config.rs        分层配置、来源追踪和 TOML 写入
    ├── credentials.rs   SecretStore、系统凭据和秘密类型
    ├── events.rs        AgentEvent 与 EventSink
    ├── approval.rs      副作用分类、审批请求和审批网关
    ├── changes.rs       文件变更账本与安全撤销
    ├── redaction.rs     敏感字符串和结构化字段脱敏
    ├── provider/
    │   ├── mod.rs       Provider trait 和领域类型
    │   ├── factory.rs   配置与凭据到 Provider 实例
    │   ├── anthropic.rs Anthropic 协议和 SSE
    │   ├── deepseek.rs  DeepSeek 协议和 SSE
    │   ├── stream.rs    流事件、Sink 和 SSE 解码
    │   └── retry.rs     安全重试、退避和请求节流
    ├── permission.rs    显式权限矩阵
    ├── tools/           注册中心和三个内置工具
    ├── session.rs       JSON 会话存储
    ├── prompt.rs        中文系统提示词
    └── error.rs         错误类别和退出码
```

`xdudu-core` 不读取终端输入，也不直接打印输出。CLI 只负责组合、输入和渲染，因此后续桌面端、服务端或测试程序可以复用同一 Agent 运行时。

## 3. 启动数据流

```text
命令行参数
  ↓
ConfigLoader：CLI > 环境 > 项目 > 用户 > 默认值
  ↓
SecretStore：Provider 环境变量 > 系统凭据库
  ↓
ProviderFactory：配置校验和具体 Provider 创建
  ↓
RetryingProvider：请求节流、重试、退避和取消
  ↓
Agent + ToolRegistry + SessionStore + EventSink
```

配置或凭据错误会在 Agent 创建之前失败。普通配置文件只接受非秘密参数；Base URL 默认必须为 HTTPS，只有 `localhost` 和回环地址允许 HTTP 测试。

## 4. Agent 运行数据流

1. CLI 创建或恢复会话并选择 Renderer；
2. Agent 构建历史消息、中文系统提示词和工具 JSON Schema；
3. Provider 通过统一流接口发送文本、工具调用、用量和终止原因；
4. Agent 将文本增量和状态变化转为 `AgentEvent`，自身不写 stdout；
5. 工具调用参数聚合完成后，ToolRegistry 检查权限并严格校验输入；
6. 有副作用的工具进入 ApprovalGate；交互确认或显式策略批准后才能继续；
7. 工具执行路径或命令级安全检查，并接受超时和取消信号；
8. 文件写入同步记录前镜像和前后哈希，结果与审批记录写入会话；
9. 持久化和渲染前统一脱敏，然后进入下一轮模型请求；
10. 正常结束、达到轮次、输出截断、中断和错误保存为明确终态。

## 5. 核心接口

| 接口 | 职责 |
| --- | --- |
| `Provider` | 统一非流式与流式模型请求 |
| `ProviderStreamSink` | 接收厂商无关的 Provider 流事件 |
| `ProviderFactory` | 校验配置、解析凭据并创建 Provider |
| `EventSink` | 接收 Agent 状态、文本、工具和用量事件 |
| `Tool` | 工具定义、运行时校验和异步执行 |
| `ToolRegistry` | 注册、权限、超时、取消和统一错误 |
| `ApprovalGate` | 对工作区写入和进程执行作出可审计决策 |
| `ChangeLedger` | 记录可撤销文件变化，隔离具体存储 |
| `SessionStore` | 会话创建、更新、读取和列表 |
| `run_agent` | 驱动模型与工具之间的多轮闭环 |

依赖通过结构体字段或 trait 引用传入，不使用全局单例，因此可以使用 MockProvider、内存 EventSink 和临时会话目录完成离线测试。

## 6. 配置与凭据

配置合并顺序：

```text
CLI 参数
  > 环境变量
  > 工作区 .xdudu/config.toml
  > ~/.config/xdudu/config.toml
  > 内置默认值
```

每个最终值都记录 `ConfigSource`，供 `config show` 和 `config explain` 展示。`config set` 只允许白名单内的非秘密字段，并通过临时文件和重命名写入 TOML。

API Key 查找顺序为 Provider 专用环境变量优先、系统凭据库其次。`SecretString` 在内存中使用可清零容器，Debug 和 Display 均脱敏。系统凭据库不可用时只提示使用环境变量，不降级创建明文 secret 文件。

项目改名后的读取顺序优先使用 `XDUDU_*`、`.xdudu` 和 `xdudu` 凭据服务；新位置缺失时兼容读取旧 `XYCLI_*`、`.xycli` 和 `xycli` 凭据。新配置、会话和凭据只写入新位置，兼容层不会反向覆盖旧数据。

## 7. Provider、流式与重试

Anthropic 使用 `/v1/messages`、`x-api-key` 和 `anthropic-version`；DeepSeek 使用 `/chat/completions` 和 Bearer Token。两者使用 `reqwest` 的 rustls 后端。

厂商 SSE 先映射为统一 `ProviderStreamEvent`：

- 文本按到达顺序增量发送；
- 工具调用按索引聚合名称和 JSON 参数片段；
- 只有完整参数才进入 Agent 工具执行；
- 用量和结束原因归一化；
- 半截 JSON、缺少终止事件和流中断返回协议错误。

`RetryingProvider` 只包围当前模型请求。连接失败、超时、408、409、429 和 5xx 可重试；认证、参数、Schema 和内容错误不可重试。默认使用带抖动的指数退避并尊重 `Retry-After`，取消令牌可中断等待。流已经输出有效文本后不会重试，避免重复输出；已完成的工具副作用永远不在重试闭包内。

## 8. 事件与输出

Agent 发出以下领域事件：

- `StateChanged`：运行状态变化；
- `AssistantDelta`：助手文本增量；
- `ToolStarted`、`ToolFinished`：工具生命周期；
- `UsageUpdated`：Token 用量；
- `Warning`：可恢复告警。

CLI Renderer 决定具体表现：默认终端流式输出，`--no-stream` 聚合文本，`--json` 输出 JSON Lines，`--no-color` 或 `NO_COLOR` 禁用颜色。领域层不感知 TTY 样式。

## 9. 权限、审批与安全边界

```text
PermissionMode 显式允许矩阵
  → Tool 输入类型、长度与未知字段校验
    → ApprovalGate 副作用审批
      → 文件真实路径或命令动作策略
```

| 模式 | 允许能力 |
| --- | --- |
| `read-only` | 仅只读工具 |
| `auto-safe` | 工作区文件读写和受限安全命令 |
| `full-access` | 所有工具级别及任意本地可执行文件 |

文件策略会解析真实工作区和符号链接，阻止越界。命令始终通过 `tokio::process::Command` 的程序名和参数数组执行，不调用 shell；`auto-safe` 只允许受限的 `pwd`、`echo`、`ls` 和只读 Git 子命令；超时或取消会终止子进程，stdout 和 stderr 均有保留上限。

审批模式为 `ask`、`never`、`always`。`ask` 仅在交互 TTY 中询问；非交互与 JSON 模式按默认拒绝处理。项目配置只能收紧权限与审批，不能设置 Provider Base URL，避免仓库配置把凭据导向其他端点。

会话、终端文本、JSON 事件、工具输入输出和顶层错误在持久化或展示前经过同一脱敏函数。文件变更前镜像只存于 `.xdudu/changes/json`，Unix 权限设为 `0600`，不进入模型上下文。

## 10. 会话、变更账本与终态

会话保存在 `.xdudu/sessions/json/<uuid>.json`。字段使用 `camelCase`，写入采用同目录临时文件后原子重命名，单进程内有异步互斥。恢复会话要求工作目录一致。

成功的 `file_write` 在 `.xdudu/changes/json/<uuid>.json` 记录路径、会话、工具调用、前镜像及前后 SHA-256。`xdudu undo` 只选择 `applied` 记录；当前内容必须仍匹配写入后哈希，否则拒绝撤销。恢复旧文件后记录改为 `undone`；Agent 新建的文件则安全删除。

| 状态 | 退出码 | 含义 |
| --- | ---: | --- |
| `completed` | 0 | 模型正常确认结束 |
| `incomplete` / `interrupted` | 1 | 未完成或用户中断 |
| 参数或配置错误 | 2 | CLI、配置或启动校验失败 |
| 权限错误 | 3 | 顶层权限错误 |
| Provider 错误 | 4 | 模型、协议或网络失败 |
| Tool 致命错误 | 5 | 工具运行时错误 |

## 11. 构建与分发

Cargo 是唯一构建入口。CI 在 Linux、macOS 和 Windows 执行格式、Clippy、测试、Release 构建和源码安装验证。`v*` 标签触发多平台归档和 SHA-256 生成；实际发布仍由 GitHub 环境和仓库权限控制。

## 12. 后续演进

Provider 扩展按当前决定暂缓，DeepSeek 保持主路径。下一阶段进入 M5：SQLite、会话查询与恢复、跨进程锁和上下文压缩。之后新增的搜索、Web、MCP 和插件必须进入同一 ToolRegistry、权限、审批、脱敏和审计链。
