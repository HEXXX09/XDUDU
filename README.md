# XDUDU

XDUDU 是一个使用 Rust 实现的终端 AI 编程助手。它把自然语言任务交给模型，通过受控的文件与终端工具完成读取、修改和验证，并将执行过程保存为本地会话。

## 当前状态

当前代码为 Rust-only 的 `v0.8.0` 开发版本：

- DeepSeek Chat Completions API 为当前主用 Provider，保留已验证的 Anthropic 适配；
- 支持文本和工具调用的 SSE 流式响应；
- 可继续上下文的 Agent 工具调用循环；
- `file_read`、`file_write`、`search_text`、`apply_patch`、`git_status`、`git_diff`、`web_search`、`web_fetch` 和 `terminal_exec` 九个内置工具；
- `read-only`、`auto-safe`、`full-access` 三种权限模式；
- `ask`、`never`、`always` 三种副作用审批模式，非交互环境默认拒绝待审批操作；
- 工作区路径隔离、符号链接逃逸防御和无 shell 命令执行；
- CLI、环境变量、项目文件、用户文件和默认值组成的分层配置；
- 环境变量或操作系统凭据库保存 API Key，普通配置文件拒绝明文密钥；
- 真实交互终端自动使用 Claude 风格的完整界面：XDUDU 启动标识、消息时间线、实时工具活动、上下文内审批、固定 Composer 和终端 Markdown；非 TTY 自动使用顺序文本；
- 统一 Agent 事件、终端流式渲染、JSON Lines 和无颜色输出；
- Provider 指数退避、抖动、`Retry-After`、请求节流和取消；
- `auth`、`config`、`doctor` 命令；
- 多文件事务变更账本、崩溃恢复、哈希冲突保护和 `undo` 整批安全撤销；
- 原生工作区文本搜索，支持 literal、regex、Glob、上下文、`.gitignore` 和 Unicode 列号；
- 固定参数的结构化 Git 状态与差异查询，不调用外部 diff 或 textconv；
- 工具阶段进度事件，终端与 JSON Lines 均可实时观察；
- 仅限 `full-access` 且仍需审批的公网 HTTPS 抓取，包含逐跳 SSRF 防御、DNS 固定、内容类型和大小限制；
- SQLite 会话存储、旧 JSON 自动迁移、跨进程锁和崩溃恢复；
- `session list/show/resume` 会话查询与恢复命令；
- 长会话 Token 预算、上下文压缩和关键计划保留；
- 结构化 Plan 生成、整份审批、自然语言修订、串行 DAG 执行、暂停恢复与并发保护；
- stdio 与 Streamable HTTP MCP，外部工具统一进入权限、审批、超时、取消、脱敏和审计链；
- 只声明 MCP Server 的隔离插件清单，以及 `mcp`、`plugin` 管理和诊断命令；
- 密钥、Bearer Token、私钥和敏感结构字段的统一输出及会话脱敏；
- macOS、Linux、Windows CI 与多平台 Release 归档工作流。

旧 TypeScript 运行时及 npm 构建链已删除，项目只需要 Rust 工具链。

## 环境要求

- Rust stable；项目通过 `rust-toolchain.toml` 声明 `rustfmt` 和 `clippy`；
- DeepSeek API Key（默认）；如显式切换 Anthropic，则需要对应 Key；
- macOS、Linux 或 Windows。

安装 Rust：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

## 构建和首次运行

```bash
cd /你的源码目录/xdudu
cargo build --workspace --release
./target/release/xdudu --version
./target/release/xdudu doctor
```

推荐把密钥保存到系统凭据库，只需录入一次：

```bash
./target/release/xdudu auth login deepseek
./target/release/xdudu auth status
./target/release/xdudu --provider deepseek
```

也可以只给当前终端临时设置环境变量：

```bash
export DEEPSEEK_API_KEY='你的密钥'
./target/release/xdudu --provider deepseek
```

Anthropic 对应 `anthropic` 和 `ANTHROPIC_API_KEY`：

```bash
./target/release/xdudu auth login anthropic
./target/release/xdudu --provider anthropic
```

不提供 prompt 时进入交互模式；单次任务可以直接跟在命令后：

```bash
./target/release/xdudu --provider deepseek "读取 README.md 并总结"
./target/release/xdudu run --provider deepseek "运行测试并解释失败原因"
```

真实交互终端中自动启用带 XDUDU 图标、状态栏、消息时间线和固定输入区的完整界面。管道、重定向、CI 和 `TERM=dumb` 会自动降级为无光标控制的顺序文本，用户无需选择渲染模式。
输入支持左右移动、Home/End、Ctrl+A/E、Ctrl+U/K/W、上下浏览本会话历史、
Ctrl+C 清空当前输入和空行 Ctrl+D 退出。历史仅保存在当前进程内，不会把输入写入额外的历史文件。
交互命令包括 `/help`、`/new`、`/resume`、`/plan <目标>`、`/model [name]`、`/mcp`、`/plugins`、`/transcript`、`/copy`、`/export`、`/rename`、`/turns <n>` 和 `/exit`。交互界面支持 Shift+Enter/Ctrl+J 多行输入、Ctrl+R 历史检索、`/` 命令候选和 `@` 工作区文件候选。DeepSeek 当前提供 `deepseek-v4-flash` 和 `deepseek-v4-pro`。

## MCP 与插件

XDUDU 支持本地 stdio 和远程 Streamable HTTP MCP。远程地址默认必须使用 HTTPS；`localhost` 和回环地址允许使用 HTTP 进行开发测试。外部工具不会绕过 XDUDU，仍需满足当前权限模式并经过审批。

```bash
# 本地 stdio Server（command 和 args 直接执行，不经过 shell）
xdudu mcp add-stdio filesystem npx -y @modelcontextprotocol/server-filesystem /你的工作区

# 远程 Streamable HTTP Server；Token 存入系统凭据，不进入配置文件
xdudu mcp add-http team https://mcp.example.com/mcp --auth
xdudu mcp login team

xdudu mcp list
xdudu mcp doctor team
xdudu mcp disable team
```

声明式插件位于 `~/.config/xdudu/plugins/*.toml`，只能声明 MCP Server，不能加载动态库或进程内 Python/Rust 代码：

```bash
xdudu plugin list
xdudu plugin show team-tools
xdudu plugin enable team-tools
xdudu plugin doctor team-tools
```

Python 扩展可以实现为独立 stdio/HTTP MCP Server，由 XDUDU 隔离启动或连接。完整协议与安全边界见 `docs/M8_MCP_PLUGIN_DESIGN.md`。

## 安装为全局命令

```bash
cd /你的源码目录/xdudu
cargo install --path crates/xdudu-cli --locked --force
xdudu --version
xdudu doctor
```

如果新终端找不到 `xdudu`，把 Cargo 二进制目录加入 `PATH`：

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> "$HOME/.zshrc"
source "$HOME/.zshrc"
```

之后可在任意项目目录直接运行：

```bash
cd /你的/项目目录
xdudu --provider deepseek
```

## 配置

配置优先级固定为：

```text
CLI 参数 > 环境变量 > .xdudu/config.toml > ~/.config/xdudu/config.toml > 内置默认值
```

常用命令：

```bash
xdudu config show
xdudu config explain provider.model
xdudu config path
xdudu config set provider.name deepseek --user
xdudu config set agent.max_turns 30 --project
xdudu config set agent.approval ask --user
```

可配置项包括 `provider.name`、`provider.model`、`provider.base_url`、`provider.timeout_seconds`、`provider.max_attempts`、`provider.retry_base_ms`、`provider.min_request_interval_ms`、`agent.max_turns`、`agent.permission`、`agent.approval`、`output.json`、`output.no_stream`、`output.color` 和 `output.debug_trace`。

XDUDU 不展示 Provider 的原始思维链或隐藏推理字段。默认界面只展示必要的计划、工具调用、进度、实际结果和完成证据。需要排查运行时状态时可以临时启用高级轨迹：

```bash
xdudu --debug-trace
xdudu --json --debug-trace "检查当前项目"
xdudu config set --user output.debug_trace true
```

调试轨迹是运行时生成的结构化元数据，只包含状态、轮次、工具名称、成功状态、耗时、错误码、Token 数和证据索引等；不包含模型思维链、助手正文、工具输入、工具输出或证据正文，并继续经过统一敏感信息脱敏。

API Key 不属于普通配置。项目或用户 TOML 中出现 key、token、secret 等秘密字段时，加载器会拒绝该配置。

项目配置属于不可信输入：它不能设置 `provider.base_url`，也不能把用户级权限或审批策略调宽。自定义 Base URL 只能通过用户配置、环境变量或 CLI 显式提供。

`v0.4.0` 改名兼容层会优先读取 `XDUDU_*` 和 `.xdudu`；新位置不存在时，可继续读取原 `XYCLI_*` 环境变量和 `.xycli` 配置/会话。系统凭据沿用原 XYCLI 的原生 `keyring` 实现，仅把固定服务名改为 `xdudu`，不进行隐式跨服务迁移。首次使用时运行 `xdudu auth login deepseek` 创建 XDUDU 凭据。

`v0.5.0` 起，首次启动会在事务中把旧 `.xdudu/sessions/json` 和 `.xycli/sessions/json` 会话导入 `.xdudu/xdudu.db`。旧文件不会删除，可作为迁移备份。

## 会话查询与恢复

```bash
xdudu session list
xdudu session list --limit 50
xdudu session show <会话UUID>
xdudu session resume <会话UUID> "继续完成刚才的任务"
xdudu session resume <会话UUID>  # 进入交互模式
```

在全屏交互界面中也可以直接输入：

```text
/resume
/resume <会话UUID>
```

不带 ID 时会显示最近会话列表，可使用 `↑/↓` 选择、Enter 恢复、Esc 取消。恢复后当前界面会重新加载历史用户和助手消息。

同一工作区只允许一个会修改状态的 XDUDU 进程运行。进程异常退出后，操作系统会自动释放锁；下次启动会把遗留的运行中会话标记为 `interrupted`。执行前已经记录但结果未知的工具调用会标记为 `cancelled`，不会被自动重放。

较长会话超过输入预算时，XDUDU 会压缩较早上下文并保留计划、关键消息和工具摘要。原始消息仍完整保存在 SQLite 中。

## 计划生成与审阅

在全屏交互界面输入：

```text
/plan 为当前项目设计一套可靠的发布检查流程
```

XDUDU 会生成结构化步骤并展示整份计划。你可以批准、请求自然语言修改或拒绝；Esc 只关闭审阅，之后使用 `/plan` 或 `/resume` 可以重新打开。批准后使用 `/plan run` 串行执行 DAG；步骤只有通过内部 `complete_step` 提交覆盖全部完成条件的证据后才会完成。文件、命令和网络操作始终经过原有工具权限与审批。

计划因审批拒绝、模型/协议错误、Ctrl+C 或未知工具结果暂停后，使用 `/resume` 查看现场，也可以使用 `/plan retry` 创建新的步骤尝试。XDUDU 不会自动重放结果未知的工具调用，取消计划也不会撤销已经产生的副作用。

```bash
xdudu plan create "分析并修复登录失败"
xdudu plan list
xdudu plan show <PLAN_ID>
xdudu plan approve <PLAN_ID> --reason "方案确认"
xdudu plan run <PLAN_ID>
xdudu plan retry <PLAN_ID>
xdudu plan revisions <PLAN_ID>
xdudu plan cancel <PLAN_ID>
```

## 输出模式

```bash
xdudu --provider deepseek "总结项目"              # 终端流式输出
xdudu --provider deepseek --no-stream "总结项目"  # 聚合后输出
xdudu --provider deepseek --json "总结项目"       # JSON Lines 事件
NO_COLOR=1 xdudu --provider deepseek               # 禁用颜色
```

Provider 发生连接失败、超时、HTTP 408、409、429 或 5xx 时，可以在尚未产生有效流式输出的前提下安全重试。已输出内容后不会自动重放，避免重复文本和副作用。

## 常用参数

```text
--provider <provider>   anthropic 或 deepseek
--model <model>         覆盖配置中的模型
--base-url <url>        覆盖 Provider 地址
--max-turns <1-100>     单次任务最大 Agent 循环次数
--permission <mode>     read-only、auto-safe 或 full-access
--approval <mode>       ask、never 或 always
--session <uuid>        继续已有会话
--json                  输出 JSON Lines 事件
--no-stream             禁用流式终端渲染
--no-color              禁用颜色
-i, --interactive       强制进入交互模式
```

## 权限和安全

默认使用 `auto-safe`：

- 文件读写仅允许在启动工作区内；
- 真实路径校验会阻止绝对路径、`..` 和符号链接逃逸；
- `search_text`、`git_status`、`git_diff` 是只读工具，无需审批；
- `apply_patch` 和 `file_write` 使用工作区写入审批，并进入同一事务账本；
- `terminal_exec` 始终以“可执行文件 + 参数数组”运行，不经过 shell；
- 仅允许 `pwd`、`echo`、工作区内 `ls` 和受限只读 Git 子命令；
- `web_search` 和 `web_fetch` 在三种权限模式下都可请求，但始终按 `ask`、`never`、`always` 独立审批；搜索返回候选来源，抓取只允许公网 HTTPS，不允许私网、认证、Cookie 或下载，并兼容代理 Fake-IP DNS；
- 其他可执行文件需要显式使用 `--permission full-access`。

`full-access` 仍不会启用 shell 字符串拼接，但允许模型调用 PATH 中的任意程序，只应在任务和仓库可信时使用。

默认审批模式为 `ask`。交互终端会在文件写入、命令执行或网络访问前用一行展示已脱敏的关键操作摘要，并提供仅本次、本会话、永久允许和拒绝四种选择。菜单使用 `↑/↓` 移动、`Enter` 确认，默认选中拒绝，`Esc` 或 `Ctrl-C` 也会安全拒绝。作用域按“工具名 + 副作用类型”匹配，批准 `web_fetch` 不会放行 `terminal_exec`。

交互界面不会截断可见历史，也不会捕获鼠标；可直接使用终端滚轮、搜索和复制。Web Search 等已完成工具会压缩成一行摘要写入历史。

`Allow always` 保存到用户配置目录的 `approval-rules.json`，不会写入项目仓库。管道输入、一次性命令和 JSON 模式无法安全询问：没有匹配永久规则时默认拒绝；有匹配规则时可以执行。永久规则可随时查看或撤销：

```bash
xdudu approval list
xdudu approval revoke web_fetch
xdudu approval clear
```

自动化场景只有在调用方明确承担风险时才应使用全局 `--approval always`。

每次成功的 `file_write` 或 `apply_patch` 都会在 `.xdudu/changes/json/` 创建受保护的事务记录：

```bash
xdudu undo                       # 撤销最近一个完整文件事务
xdudu undo --change <事务UUID>   # 撤销指定事务
xdudu --session <会话UUID> undo  # 只撤销指定会话的最近变更
```

撤销前会预检事务内全部文件。任何文件被人工或其他程序修改后，整批撤销都会拒绝且零文件变化；`undo` 不需要 API Key。启动时会检查 `Prepared` 或 `Applying` 事务：可判定的未完成写入恢复到前镜像，用户内容冲突则标记为 `Conflict` 并停止静默继续。

## 测试与质量检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --release --locked
./target/release/xdudu --help
./target/release/xdudu doctor --json
```

Provider 协议测试使用本机临时 HTTP 服务，不访问真实模型 API，也不会消耗额度。

## 架构

```text
xdudu CLI
  ├── clap 命令、交互模式和 Ctrl+C
  ├── Renderer：终端 / JSON Lines / 非流式
  └── doctor、auth、config
        ↓
xdudu-core
  ├── Config + SecretStore + ProviderFactory
  ├── Agent Loop + AgentEvent + ToolProgress
  ├── Provider：Anthropic / DeepSeek / Stream / Retry
  ├── PermissionMode + ApprovalGate + ToolRegistry
  ├── file_read / file_write / search_text / apply_patch
  ├── git_status / git_diff / web_search / web_fetch / terminal_exec
  ├── SqliteSessionStore + WorkspaceLock + Context Compression
  ├── Plan + PlanStep + PlanRevision + PlanStore + PlanGenerator/Reviewer
  └── JsonChangeLedger + Undo
```

详细资料：

- [系统架构](docs/ARCHITECTURE.md)
- [详细设计](docs/DESIGN.md)
- [v0.3.0 阶段设计与验收](docs/NEXT_PHASE_DESIGN.md)
- [v0.4.0 安全治理设计与验收](docs/SAFETY_GOVERNANCE_DESIGN.md)
- [v0.5.0 会话恢复与上下文设计](docs/M5_SESSION_RECOVERY_DESIGN.md)
- [v0.6.0 工具与网络安全设计](docs/M6_TOOLING_WEB_DESIGN.md)
- [M7 Plan 基础设计](docs/M7_PLAN_FOUNDATION_DESIGN.md)
- [产品需求](docs/PRD.md)
- [任务路线图](docs/TASKS.md)
- [Rust 迁移记录](docs/RUST_MIGRATION.md)

## 许可证

MIT
