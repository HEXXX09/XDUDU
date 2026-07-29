# XDUDU 开发报告

> 更新时间：2026-07-29
> 当前版本：v0.6.0

## 总结

XDUDU 已完成 M6 搜索、补丁、Git 与受限 Web 的本地实现和自动化测试。仓库保持 Rust-only，DeepSeek 是默认主用 Provider，Anthropic 适配继续保留但不扩展 Provider 范围。

本阶段把新工具统一纳入既有权限、审批、取消、脱敏、会话和恢复边界。旧 TypeScript 实现不再参与构建、测试或发布。

## 已完成

| 能力 | 状态 |
| --- | --- |
| Provider 目录化拆分 | 已完成 |
| Anthropic 与 DeepSeek SSE 流式文本、工具调用聚合 | 已完成 |
| 分层配置、来源追踪和 TOML 写入 | 已完成 |
| 系统凭据存储、环境变量降级和秘密脱敏 | 已完成 |
| Provider Factory 与启动前校验 | 已完成 |
| AgentEvent、EventSink 和 CLI Renderer | 已完成 |
| JSON Lines、非流式和无颜色输出 | 已完成 |
| 错误分类、指数退避、抖动和 Retry-After | 已完成 |
| 最小请求间隔和取消感知 | 已完成 |
| doctor 与全局安装检查 | 已完成 |
| macOS、Linux、Windows CI | 已完成 |
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

## 当前架构

```text
CLI 命令与 Renderer
  → 配置解析 + 凭据解析 + Provider Factory
    → RetryingProvider
      → Anthropic / DeepSeek 流式 Provider
        → Agent Loop + AgentEvent
          → PermissionMode + ApprovalGate + ToolRegistry
            → file_read / file_write / search_text / apply_patch
            → git_status / git_diff / web_fetch / terminal_exec
          → SqliteSessionStore + WorkspaceLock + Context Compression
          → Transactional JsonChangeLedger + ToolProgress + Redaction
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
- Agent 文件事务可在全部哈希未变化时整批安全撤销；
- 未完成事务启动时恢复；用户内容冲突时明确阻止静默继续；
- Git 专用工具只执行固定只读参数并限制仓库根；
- 网络读取在三种权限模式下均可请求，但仍需独立审批，只访问经过逐跳校验和 DNS 固定的公网 HTTPS；
- Web 不携带 Cookie、认证、代理或 Provider 密钥，不下载文件；
- 同一工作区只允许一个状态写入进程，崩溃后锁自动释放；
- 结果未知的工具调用恢复为取消状态，不自动重放；
- 长会话只压缩 Provider 输入窗口，不删除本地原始消息；
- CI 和默认测试不需要真实 API Key，也不请求公网模型。

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

- 当前主用 DeepSeek，Provider 扩展、熔断和 fallback 按产品决定暂缓；
- `undo` 覆盖 `file_write` 与 `apply_patch`，无法通用撤销终端命令或网络的外部副作用；
- `web_fetch` 只读取 HTML、纯文本和 JSON，不支持登录态、文件下载或私网；
- 尚未实现 Plan 模式、MCP 与插件；
- Release 工作流已定义，实际各平台结果需在 GitHub Actions 运行后确认。

## 下一阶段建议

用户完成本地 M6 运行验收并确认推送后，进入 M7 Plan 模式。M7 只编排当前已经受控的工具，新增计划状态必须持久化、可恢复并继续遵守权限、审批、脱敏与事务边界。
