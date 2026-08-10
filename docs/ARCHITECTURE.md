# XDUDU 系统架构

> 当前基线：Rust-only v0.8.0 开发分支。旧 TypeScript 实现已退役，可通过 Git 历史审计。

## 1. 系统定位

XDUDU 是运行在开发者本机终端中的 AI 编程 Agent。它接收自然语言任务，通过模型推理、受控工具调用和本地会话持久化完成编码工作，不是常驻 Web 服务。

当前外部边界包括模型 API、受限公网 HTTPS、系统凭据库、当前工作区文件系统、本地可执行程序、stdio MCP 和 Streamable HTTP MCP。声明式插件只能组合 MCP Server，不能把动态代码加载进 XDUDU 进程。

## 2. Rust 工作区

```text
Cargo workspace
├── crates/xdudu-cli
│   ├── main.rs          命令、参数、装配、REPL 和退出码
│   ├── renderer.rs      终端、JSON Lines 和非流式渲染
│   ├── tui.rs           全屏对话、实时活动、状态栏和 Composer
│   ├── input_editor.rs  普通行式界面的安全输入编辑
│   └── doctor.rs        安装、配置、凭据和工作区诊断
└── crates/xdudu-core
    ├── agent.rs         Agent 主循环、事件发送、停滞检测与批次并行调度
    ├── config.rs        分层配置、来源追踪、TOML 写入与命令规则
    ├── credentials.rs   SecretStore、系统凭据和秘密类型
    ├── events.rs        AgentEvent 与 EventSink
    ├── approval.rs      副作用分类、审批请求和审批网关
    ├── changes.rs       多文件事务账本、恢复与安全撤销
    ├── redaction.rs     敏感字符串和结构化字段脱敏
    ├── provider/
    │   ├── mod.rs       Provider trait 和领域类型
    │   ├── factory.rs   配置与凭据到 Provider 实例
    │   ├── anthropic.rs Anthropic 协议和 SSE
    │   ├── deepseek.rs  DeepSeek 协议和 SSE
    │   ├── openai_wire.rs  OpenAI wire 协议复用层
    │   ├── openai_compatible.rs OpenAI-compatible Provider
    │   ├── stream.rs    流事件、Sink 和 SSE 解码
    │   └── retry.rs     安全重试、退避和请求节流
    ├── permission.rs    显式权限矩阵
    ├── mcp.rs           stdio/Streamable HTTP MCP、生命周期与工具适配
    ├── plugin.rs        声明式插件清单、加载与校验
    ├── instructions.rs  用户/项目级指令与仓库约定加载、提示词注入
    ├── skills.rs        Skills 发现、frontmatter 校验与优先级去重
    ├── subagent.rs      AgentProfile 档案与 task 子代理隔离循环
    ├── stall.rs         停滞检测与恢复策略
    ├── memories.rs       可审查记忆存储与 FTS5 检索
    ├── memory_suggestion.rs 会话结束记忆建议协议
    ├── tools/           注册中心、内置工具（含 skill/web_read）及路径策略
    ├── session.rs       会话领域模型与兼容 JSON 读取
    ├── sqlite_session.rs SQLite、迁移、恢复与工作区锁
    ├── prompt.rs        中文系统提示词
    └── error.rs         错误类别和退出码
```

`xdudu-core` 不读取终端输入，也不直接打印输出。CLI 只负责组合、输入和渲染，因此后续桌面端、服务端或测试程序可以复用同一 Agent 运行时。CLI 根据 TTY 能力自动决定渲染策略，领域事件及权限协议不依赖具体界面。

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
2. Agent 按 Token 预算构建历史消息、压缩摘要和中文系统提示词；工具 JSON Schema 仅通过 Provider 的结构化 `tools` 字段发送；
3. Provider 通过统一流接口发送文本、工具调用、用量和终止原因；
4. Agent 将文本增量和状态变化转为 `AgentEvent`，自身不写 stdout；
5. 工具调用参数聚合完成后，ToolRegistry 检查权限并严格校验输入；
6. 有副作用的工具进入 ApprovalGate；交互确认或显式策略批准后才能继续；
7. 工具执行路径或命令级安全检查，并接受超时和取消信号；
8. 长工具通过有界非阻塞通道发布 `ToolProgress`；进度只实时渲染，不进入会话上下文；
9. 文件写入先保存 `Prepared` 事务，再进入 `Applying` 和原子提交，成功后标记 `Applied`；
10. 持久化和渲染前统一脱敏，状态由 `Observing` 进入下一轮 `Reflecting`；
11. 正常结束、达到轮次、输出截断、未解决工具失败、中断和错误保存为明确终态。

单次任务采用隐藏推理的 ReAct 运行方式：

```text
Planning → Acting → Observing → Reflecting
              ↑                       │
              └───────────────────────┘
```

内部推理不作为 `Thought` 文本输出。工具拒绝后，同批后续副作用调用会被运行时阻止，不能借助另一工具绕过审批；不产生副作用的检查仍可继续。

## 5. 核心接口

| 接口 | 职责 |
| --- | --- |
| `Provider` | 统一非流式与流式模型请求 |
| `ProviderStreamSink` | 接收厂商无关的 Provider 流事件 |
| `ProviderFactory` | 校验配置、解析凭据并创建 Provider |
| `EventSink` | 接收 Agent 状态、文本、工具和用量事件 |
| `Tool` | 工具定义、运行时校验和异步执行 |
| `ToolRegistry` | 注册、权限、超时、取消和统一错误 |
| `ApprovalGate` | 对工作区写入、进程执行和网络访问作出可审计决策 |
| `ChangeLedger` | 记录可恢复、可整批撤销的文件事务，隔离具体存储 |
| `SessionStore` | 会话创建、更新、读取和列表 |
| `PlanStore` | 当前计划、不可变 revision 快照、乐观并发更新和会话关联查询 |
| `generate_plan` | 通过隔离的 Provider 协议生成、校验并持久化 Draft 计划 |
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

项目改名后的读取顺序优先使用 `XDUDU_*` 和 `.xdudu`，新位置缺失时兼容读取旧 `XYCLI_*` 和 `.xycli`。系统凭据保持原 XYCLI 的原生 `keyring` 读写模型，只使用固定服务名 `xdudu`，不自动读取或迁移其他服务。这样凭据的创建、读取和删除都由同一套跨平台后端完成，行为简单且可预测。

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
- `ToolStarted`、`ToolProgress`、`ToolFinished`：工具生命周期和实时阶段；
- `DebugTrace`：高级模式下的安全运行时元数据，不包含模型思维链或工具正文；
- `UsageUpdated`：Token 用量；
- `Warning`：可恢复告警。

CLI Renderer 自动决定具体表现：交互 TTY 启用完整界面，保留图标、状态栏、消息时间线和固定输入区；非 TTY、管道和自动化环境顺序输出纯文本。`--no-stream` 聚合文本，`--json` 输出 JSON Lines，`--no-color` 或 `NO_COLOR` 禁用颜色。

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
| `full-access` | 所有工具级别、受审批网络访问及任意本地可执行文件 |

文件策略会解析真实工作区和符号链接，阻止越界。命令不调用 shell；`pwd`、`echo`、`ls`
由 Rust 内建实现以保持三平台一致，其余命令始终通过 `tokio::process::Command` 的程序名和
参数数组执行。`terminal_exec` 在 `auto-safe` 下按三档前缀白名单决策：deny（默认
`sudo`/`mkfs`/`rm`）立即拒绝，allow（默认内建命令、只读 Git 子命令及
`cargo`/`npm`/`python3`/`go` 常见检查命令）直接执行，未匹配命令进入审批门（ask 档）；
项目配置只能追加 deny 与 ask，不能追加 allow。超时或取消会终止子进程，stdout 和 stderr
均有保留上限。

审批模式为 `ask`、`never`、`always`。工作区写入、进程执行和网络访问都进入同一审批链。`ask` 在交互 TTY 中提供单次、当前会话和永久三种批准作用域；规则按工具名与副作用类型精确匹配。永久规则保存在用户配置目录，项目配置不能创建或扩大规则。非交互与 JSON 模式只使用已有永久规则，否则默认拒绝。项目配置只能收紧权限与审批，不能设置 Provider Base URL，避免仓库配置把凭据导向其他端点。

`search_text` 使用 Rust `ignore`、`globset` 和 `regex`，不依赖外部 `rg`；它遵守 `.gitignore`，跳过二进制、符号链接及内部目录，并对扫描文件数、字节数和结果体积设硬上限。`git_status` 与 `git_diff` 只运行内部固定参数 Git 命令，仓库根必须位于工作区，且禁用外部 diff 和 textconv。

`web_search` 和 `web_fetch` 在三种权限模式下都能提出调用，但 `NetworkAccess` 始终进入独立审批。`web_search` 通过固定公网搜索入口返回有界的标题、HTTPS 链接和摘要；Agent 对通用知识、研究或时效性问题可以先搜索候选来源，再用 `web_fetch` 阅读相关页面。本地搜索无结果时，如果任务不限于本地资料，ReAct 循环会优先尝试网络检索而不是直接结束。

`web_fetch` 的每一跳 URL 都必须是无认证信息的 HTTPS；DNS 返回的全部地址都要通过公网检查，并使用已经验证的地址固定连接，TLS SNI 与证书校验仍使用原域名。系统 DNS 结果全部落入 `198.18.0.0/15` Fake-IP 网段时，使用固定公网地址的 HTTPS DoH 获取真实记录，结果仍执行同一公网校验。网络客户端不转发代理凭据、Cookie、认证、自定义 Header 或请求体，也不自动重试。

会话、终端文本、JSON 事件、工具输入输出和顶层错误在持久化或展示前经过同一脱敏函数。
Provider 内部 reasoning 只为工具闭环保存在脱敏后的会话记录中，`session show`、TUI、导出和
调试轨迹均不返回原始正文。文件变更前镜像只存于 `.xdudu/changes/json`，Unix 权限设为
`0600`，不进入模型上下文。

## 10. 会话、变更账本与终态

会话保存在 `.xdudu/xdudu.db`。SQLite 启用 WAL、外键约束、五秒忙等待和显式事务；`schema_migrations` 记录数据库版本。Unix 下数据库、WAL、共享内存和锁文件收紧为 `0600`。首次启动在单个事务中导入 `.xdudu/sessions/json` 和 `.xycli/sessions/json`，成功后只写迁移标记，不删除旧文件。

`.xdudu/workspace.lock` 使用操作系统独占文件锁，阻止两个 XDUDU 进程同时修改同一工作区。锁在进程退出或崩溃时自动释放；下次启动将遗留的 `running` 或 `waiting_approval` 会话标记为 `interrupted`。工具执行前先持久化 `pending` 记录，恢复时将结果未知的调用改为 `cancelled` 并补充错误观察，不自动重放副作用。

上下文预算默认限制输入估算值为 24,000 Token。超过预算时，本地确定性压缩较早消息，保留会话计划、角色、工具名称与受限长度结果，并继续携带最近完整消息。压缩只影响发给 Provider 的窗口；SQLite 中的原始消息不删除。

成功的 `file_write` 与 `apply_patch` 在 `.xdudu/changes/json/<uuid>.json` 写入 `schemaVersion: 2` 事务，记录全部路径、操作类型、权限、前后镜像及 SHA-256。启动时会恢复 `prepared` 或 `applying` 事务；若当前内容既不匹配前哈希也不匹配后哈希，则标记 `conflict` 并阻止静默继续。`xdudu undo` 先预检事务中全部文件，全部匹配后才整批恢复；旧 v1 单文件记录仍可读取和撤销。

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

Provider 扩展按当前决定暂缓，DeepSeek 保持主路径。M6 功能与 macOS、Linux、Windows CI 验收已完成。M7 已完成会话恢复、Plan Schema v3、SQLite Schema v4、结构化生成、整份审批/修订、串行 DAG 执行和恢复。M8 已完成 stdio 与 Streamable HTTP MCP 客户端、声明式插件清单、动态工具注册与统一权限/审批/脱敏链，并通过 stdio/HTTP 恶意输入、越权、超时、取消与审批链 E2E 及三平台 CI 验收。M9 已完成用户级/项目级指令注入、可审查记忆（任务完成建议→TUI 逐条确认→SQLite FTS5 存储与检索→上下文注入），默认不自动写入，未引入向量 RAG。`submit_plan` 创建 Draft，`revise_plan` 生成完整新 revision，`complete_step` 以逐项证据确认当前步骤；执行期通过 `revision + execution_version + status` 原子检查点避免并发覆盖。Plan 不保存隐藏推理，也不替代单次请求内部的 ReAct；批准 Plan 不放行任何工具副作用，崩溃后也不会自动重放结果未知的工具。

M11 功能开发和本地门禁已完成，等待远端三平台 CI：OpenAI-compatible Provider 与思考闭环；
停滞检测；`task` 子代理委派（隔离上下文、运行时受限工具集、同批并行、审计持久化）；只读
工具批次并行；Skills 技能系统；仓库指令；跨平台命令白名单；LLM 分级上下文压缩；记忆注入
管线；以及具备响应体和提炼次数硬上限的 `web_read`。三平台通过前不把 M11 标记为发布完成。
