# XDUDU 开发报告

> 更新时间：2026-08-10
> 当前版本：v0.8.0 开发分支

## 总结

XDUDU 已完成 M6 搜索、补丁、Git 与受限 Web，M7 的会话恢复、Plan 领域、结构化生成、
整份审批、自然语言修订、串行执行和中断恢复，以及 M11 的 Agent 编排能力
（子代理与并行执行）、Skills 技能系统、指令接通与命令白名单、LLM 分级上下文压缩、
记忆注入优化与 `web_read` 大页面阅读。仓库保持 Rust-only，DeepSeek 是默认主用
Provider，Anthropic 适配继续保留，并新增 OpenAI-compatible Provider。

本阶段把新工具统一纳入既有权限、审批、取消、脱敏、会话和恢复边界。旧 TypeScript
实现不再参与构建、测试或发布。

## 已完成

| 能力 | 状态 |
| --- | --- |
| Provider 目录化拆分 | 已完成 |
| Anthropic 与 DeepSeek SSE 流式文本、工具调用聚合 | 已完成 |
| OpenAI-compatible Provider（OpenAI wire 协议复用） | 已完成 |
| DeepSeek `reasoning_content` 思考闭环（不进入公开输出） | 已完成 |
| `temperature` / `max_output_tokens` / `reasoning` 配置化 | 已完成 |
| 分层配置、来源追踪和 TOML 写入 | 已完成 |
| 系统凭据存储、环境变量降级和秘密脱敏 | 已完成 |
| Provider Factory 与启动前校验 | 已完成 |
| AgentEvent、EventSink 和 CLI Renderer | 已完成 |
| JSON Lines、非流式和无颜色输出 | 已完成 |
| 错误分类、指数退避、抖动和 Retry-After | 已完成 |
| 最小请求间隔和取消感知 | 已完成 |
| doctor 与全局安装检查 | 已完成 |
| macOS、Linux、Windows CI | 既有基线完成；M11 变更待远端验证 |
| 多平台 Release 归档和 SHA-256 工作流 | 已完成 |
| 副作用分类、交互审批与非交互默认拒绝 | 已完成 |
| 会话、事件、错误和审批提示统一脱敏 | 已完成 |
| 文件变更账本、哈希冲突保护和安全撤销 | 已完成 |
| XYCLI 旧配置/会话只读兼容与凭据一次性迁移 | 已完成 |
| SQLite Schema、事务会话存储和旧 JSON 导入 | 已完成 |
| session list/show/resume | 已完成 |
| 工作区跨进程锁、崩溃恢复和工具防重放 | 已完成 |
| Token 预算、上下文压缩和计划保留 | 已完成 |
| 原生 `search_text` 与 `.gitignore`/Glob/regex 支持 | 已完成 |
| 结构化 `git_status` 与有界 `git_diff` | 已完成 |
| v2 多文件事务账本、崩溃恢复与 v1 兼容 | 已完成 |
| 严格 unified diff `apply_patch` 与整批 `undo` | 已完成 |
| 非阻塞工具进度事件与终端/JSON 渲染 | 已完成 |
| 网络权限、逐跳 SSRF 防御和受限 `web_fetch` | 已完成 |
| once/session/always 分级审批与永久规则管理 | 已完成 |
| Plan Schema v3、attempt/evidence、revision 快照与 SQLite Schema v4 迁移 | 已完成 |
| `submit_plan` 结构化生成与 `revise_plan` 完整修订协议 | 已完成 |
| `/plan` 最小 TUI 整份审批、修改、拒绝与 `/resume` 恢复审阅 | 已完成 |
| revision/status 乐观并发保护与 `PLAN_CONFLICT` | 已完成 |
| MCP stdio / Streamable HTTP 与声明式插件（统一安全链） | 已完成 |
| 用户/项目级指令与仓库约定（AGENTS.md/CLAUDE.md）接通主循环 | 已完成 |
| Skills 技能系统（六级发现、frontmatter、`skill` 工具、三档策略） | 已完成 |
| `terminal_exec` 三档前缀白名单（deny > allow > ask） | 已完成 |
| 子代理体系（`AgentProfile`、`task` 工具、并行委派、审计持久化） | 已完成 |
| 子代理任务图（DAG、依赖解锁、受控并发、失败传播与取消） | 本地验收通过 |
| 只读工具并行执行（批次 `join_all`、单进度通道按 call_id 分发） | 已完成 |
| 停滞检测与 auto/ask/off 恢复策略 | 已完成 |
| LLM 分级上下文压缩（`submit_context_summary` + 确定性回退） | 已完成 |
| 记忆注入管线（精排、去重、Token 预算） | 已完成 |
| `web_read` 有界流式读取 + 最多 8 次 LLM 提炼 | 本地验收通过 |

## 当前架构

```text
CLI 命令与 Renderer
  → 配置解析 + 凭据解析 + Provider Factory
    → RetryingProvider
      → Anthropic / DeepSeek / OpenAI-Compatible 流式 Provider
        → Agent Loop + AgentEvent + 停滞检测 + 只读并行批次
          → PermissionMode + ApprovalGate + ToolRegistry + CommandRules
            → file_read / file_write / search_text / apply_patch
            → git_status / git_diff / web_fetch / web_read / terminal_exec
            → skill（Skills 加载）/ task（子代理委派，隔离上下文）
          → SqliteSessionStore + WorkspaceLock + 分级 Context Compression
          → PlanGenerator + PlanReviewer + PlanRevision Store
          → Transactional JsonChangeLedger + ToolProgress + Redaction
          → Instructions（用户/项目/仓库约定）+ Memories（精排注入）
```

## 可靠性与安全不变量

- 配置优先级为 CLI、环境、项目、用户、默认值；
- 普通 TOML 不允许保存 API Key、Token 或 Secret；
- Secret 的 Debug、Display 和配置输出不会显示原文；
- Base URL 默认要求 HTTPS，仅本机协议测试允许 HTTP；
- 重试只包围单次模型请求，不重放已经执行成功的工具；
- SSE 已产生文本后发生中断时不自动重试；
- 工具仍通过权限矩阵、严格输入校验和工作区安全策略；
- 文件写入与进程执行必须经过副作用审批，非交互 `ask` 默认拒绝；
- 项目配置不能提升权限、自动批准或重定向 Provider Base URL；
- 项目配置不能追加命令 allow、放宽 skills 或设置非只读自定义档案；
- Agent 文件事务可在全部哈希未变化时整批安全撤销；
- 未完成事务启动时恢复；用户内容冲突时明确阻止静默继续；
- Git 专用工具只执行固定只读参数并限制仓库根；
- 网络读取在三种权限模式下均可请求，但仍需独立审批，只访问经过逐跳校验和 DNS 固定的公网 HTTPS；
- Web 不携带 Cookie、认证、代理或 Provider 密钥，不下载文件；
- 同一工作区只允许一个状态写入进程，崩溃后锁自动释放；
- 结果未知的工具调用恢复为取消状态，不自动重放；
- 长会话只压缩 Provider 输入窗口，不删除本地原始消息；
- 子代理不获得父会话没有的权限，审计记录随父会话持久化；
- 并行只读工具不触碰变更账本，ChangeLedger 保持单写者；
- CI 和默认测试不需要真实 API Key，也不请求公网模型。
- Plan 审批不授予工具权限，批准计划不会绕过文件、进程或网络审批；
- 当前 Plan 与 revision 快照原子写入，陈旧审批不会覆盖较新决定；
- Provider 修订失败时原 PendingApproval 计划保持不变。

## 验收命令

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --release --locked
./target/release/xdudu --version
./target/release/xdudu --help
./target/release/xdudu doctor --json
cargo install --path crates/xdudu-cli --locked --force
```

## 当前边界

- 当前主用 DeepSeek，Provider 熔断和 fallback 按产品决定暂缓；
- `undo` 覆盖 `file_write` 与 `apply_patch`，无法通用撤销终端命令或网络的外部副作用；
- `web_fetch` 只读取 HTML、纯文本和 JSON，不支持登录态、文件下载或私网；`web_read` 同样不携带凭据；
- Plan 已支持串行 DAG 执行、`complete_step` 证据、暂停/重试/取消和崩溃恢复；
- 默认输出不包含 Provider 原始思维链；高级 `--debug-trace` 仅输出脱敏后的状态机与执行元数据，Plan 完成时逐条显示验证证据；
- 子代理内部工具以审计记录随父会话持久化，不写入父会话消息历史（隔离上下文）；
- 子代理运行时再次校验档案工具白名单，不能通过异常 Provider 响应绕过；
- `task_graph` 只并行显式只读档案；任何非只读节点独占执行，避免审批与文件事务并发；
- 图节点失败时依赖后继进入 blocked，独立分支按策略继续；崩溃后整图结果未知且不会自动重放；
- Skills 正文只注入后续 Provider system 提示词，工具结果只保留加载元数据；
- `web_read` 单次响应最多读取 1 MiB、单次调用最多提炼 8 个文本块，并复用 SSRF 边界；
- 停滞恢复不重放结果未知的工具调用；恢复后若仍连续失败，恢复尝试次数持续累计直至阈值；
- RAG 尚未实现（M9 评审后决定是否引入）。

## 下一阶段建议

下一步先推送并确认 M11 的 macOS、Linux、Windows CI；通过后再评估 `web_read` 提炼 Token
计入会话持久化、子代理进度细分与 TUI 卡片，最后执行 v1.0.0 发布前冻结。仍必须复用现有
工具权限、审批、脱敏、计划检查点和文件事务边界。
