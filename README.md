# XDUDU

XDUDU 是一个使用 Rust 实现的终端 AI 编程助手。它把自然语言任务交给模型，通过受控的文件与终端工具完成读取、修改和验证，并将执行过程保存为本地会话。

## 当前状态

当前版本为 Rust-only 的 `v0.5.0`：

- DeepSeek Chat Completions API 为当前主用 Provider，保留已验证的 Anthropic 适配；
- 支持文本和工具调用的 SSE 流式响应；
- 可继续上下文的 Agent 工具调用循环；
- `file_read`、`file_write`、`terminal_exec` 三个内置工具；
- `read-only`、`auto-safe`、`full-access` 三种权限模式；
- `ask`、`never`、`always` 三种副作用审批模式，非交互环境默认拒绝待审批操作；
- 工作区路径隔离、符号链接逃逸防御和无 shell 命令执行；
- CLI、环境变量、项目文件、用户文件和默认值组成的分层配置；
- 环境变量或操作系统凭据库保存 API Key，普通配置文件拒绝明文密钥；
- 统一 Agent 事件、终端流式渲染、JSON Lines 和无颜色输出；
- Provider 指数退避、抖动、`Retry-After`、请求节流和取消；
- `auth`、`config`、`doctor` 命令；
- 会话级文件变更账本、哈希冲突保护和 `undo` 安全撤销；
- SQLite 会话存储、旧 JSON 自动迁移、跨进程锁和崩溃恢复；
- `session list/show/resume` 会话查询与恢复命令；
- 长会话 Token 预算、上下文压缩和关键计划保留；
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

交互命令包括 `/help`、`/new`、`/model <name>`、`/turns <n>` 和 `/exit`。

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

可配置项包括 `provider.name`、`provider.model`、`provider.base_url`、`provider.timeout_seconds`、`provider.max_attempts`、`provider.retry_base_ms`、`provider.min_request_interval_ms`、`agent.max_turns`、`agent.permission`、`agent.approval`、`output.json`、`output.no_stream` 和 `output.color`。

API Key 不属于普通配置。项目或用户 TOML 中出现 key、token、secret 等秘密字段时，加载器会拒绝该配置。

项目配置属于不可信输入：它不能设置 `provider.base_url`，也不能把用户级权限或审批策略调宽。自定义 Base URL 只能通过用户配置、环境变量或 CLI 显式提供。

`v0.4.0` 改名兼容层会优先读取 `XDUDU_*`、`.xdudu` 和 `xdudu` 系统凭据；新位置不存在时，可继续读取原 `XYCLI_*` 环境变量、`.xycli` 配置/会话。首次读取旧 `xycli` 系统凭据时，macOS 可能要求确认一次，允许后会立即复制到新的 `xdudu` 钥匙串项，此后启动不再访问旧项。所有新增数据只写入新名称位置。

`v0.5.0` 首次启动会在事务中把旧 `.xdudu/sessions/json` 和 `.xycli/sessions/json` 会话导入 `.xdudu/xdudu.db`。旧文件不会删除，可作为迁移备份。

## 会话查询与恢复

```bash
xdudu session list
xdudu session list --limit 50
xdudu session show <会话UUID>
xdudu session resume <会话UUID> "继续完成刚才的任务"
xdudu session resume <会话UUID>  # 进入交互模式
```

同一工作区只允许一个会修改状态的 XDUDU 进程运行。进程异常退出后，操作系统会自动释放锁；下次启动会把遗留的运行中会话标记为 `interrupted`。执行前已经记录但结果未知的工具调用会标记为 `cancelled`，不会被自动重放。

较长会话超过输入预算时，XDUDU 会压缩较早上下文并保留计划、关键消息和工具摘要。原始消息仍完整保存在 SQLite 中。

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
- `terminal_exec` 始终以“可执行文件 + 参数数组”运行，不经过 shell；
- 仅允许 `pwd`、`echo`、工作区内 `ls` 和受限只读 Git 子命令；
- 其他可执行文件需要显式使用 `--permission full-access`。

`full-access` 仍不会启用 shell 字符串拼接，但允许模型调用 PATH 中的任意程序，只应在任务和仓库可信时使用。

默认审批模式为 `ask`。交互终端会在文件写入或命令执行前展示已脱敏参数并等待确认；管道输入、一次性命令和 JSON 模式无法安全询问，因此默认拒绝。自动化场景只有在调用方明确承担风险时才应使用 `--approval always`。

每次成功的 `file_write` 都会在 `.xdudu/changes/json/` 创建受保护的变更记录：

```bash
xdudu undo                       # 撤销最近一次 Agent 文件写入
xdudu undo --change <变更UUID>   # 撤销指定变更
xdudu --session <会话UUID> undo  # 只撤销指定会话的最近变更
```

撤销前会比较当前文件与写入后哈希。文件被人工或其他程序修改后，XDUDU 会拒绝覆盖；`undo` 不需要 API Key。

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
  ├── Agent Loop + AgentEvent
  ├── Provider：Anthropic / DeepSeek / Stream / Retry
  ├── PermissionMode + ApprovalGate + ToolRegistry
  ├── file_read / file_write / terminal_exec
  ├── SqliteSessionStore + WorkspaceLock + Context Compression
  └── JsonChangeLedger + Undo
```

详细资料：

- [系统架构](docs/ARCHITECTURE.md)
- [详细设计](docs/DESIGN.md)
- [v0.3.0 阶段设计与验收](docs/NEXT_PHASE_DESIGN.md)
- [v0.4.0 安全治理设计与验收](docs/SAFETY_GOVERNANCE_DESIGN.md)
- [v0.5.0 会话恢复与上下文设计](docs/M5_SESSION_RECOVERY_DESIGN.md)
- [M5 技术难点与实现细节](docs/M5_TECHNICAL_IMPLEMENTATION.md)
- [产品需求](docs/PRD.md)
- [任务路线图](docs/TASKS.md)
- [Rust 迁移记录](docs/RUST_MIGRATION.md)

## 许可证

MIT
