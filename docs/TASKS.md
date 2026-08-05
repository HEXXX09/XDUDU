# XDUDU 任务路线图

> 当前技术基线：Rust-only v0.8.0 开发分支。
> 状态更新时间：2026-08-04。

## 规划调整

路线图已按依赖和风险重新排序：

- 配置、凭据、安装、事件协议和 CI 提前到 M2；
- Provider 稳定性并入 M2；按当前产品决定，M3 Provider 扩展暂缓，近期只使用 DeepSeek 主路径；
- 审批、脱敏、变更账本和撤销提前到新增 Web/MCP 等高风险能力之前；
- SQLite 和上下文压缩先于跨会话记忆；
- Computer Use 移出 1.0 主线，待安全、恢复和跨平台基础成熟后再评估。

## 里程碑总览

| 里程碑 | 目标 | 状态 |
| --- | --- | --- |
| M1 | Rust 核心迁移与旧 TS 退役 | 已完成 |
| M2 | 产品化基础：配置、凭据、流式、CI | 已完成 |
| M3 | Provider 扩展与容错 | 暂缓，DeepSeek 优先 |
| M4 | 审批、脱敏、变更账本与撤销 | 已完成 |
| M5 | SQLite、恢复与上下文管理 | 已完成 |
| M6 | 搜索、补丁、Git 与受限 Web | 已完成 |
| M7 | Plan 模式与任务执行 | 已完成 |
| M8 | MCP 与插件 | 已完成 |
| M9 | 自定义指令、记忆与可选 RAG | 待评审 |
| M10 | 1.0 发布、诊断与兼容性 | 待开始 |

## M1：Rust 核心迁移与收尾

- [x] 建立 Cargo workspace、核心库和 CLI；
- [x] 迁移 Agent Loop、Provider、工具、权限和会话；
- [x] 实现 Anthropic 与 DeepSeek；
- [x] 实现路径隔离、安全命令、超时与取消；
- [x] 建立 Rust 单元、协议、安全和真实进程测试；
- [x] 删除旧 TypeScript 源码、测试、npm 依赖和构建链；
- [x] 文档切换为 Rust-only 基线。

M1 验收：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace --release
./target/release/xdudu --help
```

## M2：产品化基础

详细设计见 `NEXT_PHASE_DESIGN.md`。

- [x] M2-T01：拆分 Provider 模块，不改变外部行为；
- [x] M2-T02：实现分层配置、来源追踪和 config 命令；
- [x] M2-T03：实现系统凭据存储、auth 命令和秘密脱敏；
- [x] M2-T04：实现 Provider Factory；
- [x] M2-T05：实现 AgentEvent 与 EventSink；
- [x] M2-T06：实现无颜色、非流式和 JSON Renderer；
- [x] M2-T07：实现 Anthropic 与 DeepSeek 流式协议；
- [x] M2-T08：实现错误分类、退避、限流和安全重试；
- [x] M2-T09：实现 doctor 与全局安装检查；
- [x] M2-T10：建立三平台 CI 和发布产物草案。

M2 已由 `main` 的 macOS、Linux 和 Windows CI 矩阵确认。

## M3：Provider 扩展与容错

按 2026-07-28 的产品决策暂缓。本阶段不新增 Provider、不实现 fallback，保持 DeepSeek 为主用路径；以下任务保留为未来候选，不阻塞 M4、M5。

- [ ] M3-T01：实现 OpenAI Provider；
- [ ] M3-T02：实现 OpenAI-compatible 自定义网关；
- [ ] M3-T03：实现 Provider 能力探测；
- [ ] M3-T04：实现熔断器；
- [ ] M3-T05：实现显式 fallback 策略；
- [ ] M3-T06：完成跨 Provider、重试、熔断和 fallback E2E。

## M4：审批、脱敏、变更账本与撤销

详细设计见 `SAFETY_GOVERNANCE_DESIGN.md`。

- [x] M4-T01：在 ToolRegistry 建立“权限 → 校验 → 审批 → 执行 → 审计”统一策略链；
- [x] M4-T02：定义副作用分类和 ApprovalGate；
- [x] M4-T03：实现交互审批、非交互默认拒绝和审批记录；
- [x] M4-T04：实现密钥、Token、私钥和敏感结构字段脱敏；
- [x] M4-T05：实现会话级文件变化账本；
- [x] M4-T06：实现基于哈希保护的 `undo`；
- [x] M4-T07：完成审批、拒绝、脱敏、冲突和撤销 E2E。

## M5：SQLite、恢复与上下文管理

详细设计见 `M5_SESSION_RECOVERY_DESIGN.md`。

- [x] M5-T01：建立 SQLite Schema、版本和迁移机制；
- [x] M5-T02：实现 SQLite SessionStore；
- [x] M5-T03：实现 session list/show/resume 命令；
- [x] M5-T04：实现跨进程锁和崩溃恢复；
- [x] M5-T05：实现 Token 预算与上下文压缩；
- [x] M5-T06：保留关键约束、计划和工具摘要；
- [x] M5-T07：完成迁移、恢复和长会话 E2E。

## M6：搜索、Web 与 Git 专用工具

详细设计见 `M6_TOOLING_WEB_DESIGN.md`。

- [x] M6-T01：实现原生 `search_text`；
- [x] M6-T02：把变更账本升级为兼容 v1 的多文件事务模型；
- [x] M6-T03：实现事务型 `apply_patch` 与整批 `undo`；
- [x] M6-T04：实现专用 `git_status` 和 `git_diff`；
- [x] M6-T05：实现工具进度事件、非阻塞通道和终端/JSON 渲染；
- [x] M6-T06：建立网络权限、逐跳 SSRF 防御和受限 `web_fetch`；
- [x] M6-T07：完成搜索、Git、补丁、恢复、进度和网络安全本地测试；
- [x] M6-T08：完成用户本地运行验收与三平台远端 CI；GitHub Actions 运行 `30618483367` 的 macOS、Linux、Windows 作业全部通过。

## M7：Plan 模式与任务执行

- [x] M7-T00：优化系统提示词，建立 Planning、Acting、Observing、Reflecting 的 ReAct 运行基线，并实现本地无结果后的受控网络检索闭环；
- [x] M7-T00A：实现 TUI `/resume` 会话选择器、直接 UUID 恢复和历史消息重建；
- [x] M7-T01：定义版本化 Plan、Step、依赖 DAG、状态迁移与 SQLite v2 持久化基础；
- [x] M7-T02：实现隔离的规划提示词、`submit_plan` 结构化协议、严格解析、DAG 校验与 Draft 持久化；
- [x] M7-T03：实现整份 Plan 审批、拒绝、自然语言修订、revision 快照、乐观并发保护与最小 TUI 审阅；
- [x] M7-T04：实现串行 DAG 执行、`complete_step` 证据协议、原子检查点、暂停/重试/取消和崩溃恢复；
- [x] M7-T05：实现 `/plan` TUI 恢复流程和 `xdudu plan` 非交互 CLI；
- [x] M7-T06：交互终端自动使用完整 TUI，增加上下文内审批、统一 Markdown、多行 Composer 与会话体验收口；
- [x] M7-T06：完成生成、修改、拒绝、执行、暂停和恢复测试，并发布 v0.7.0。
- [x] M7-T07：建立“无原始思维链”的公开输出边界，默认展示计划/工具/进度/结果/证据，并提供脱敏结构化 `--debug-trace`。

## M8：MCP 与插件

- [x] M8-T01：实现 MCP stdio 与 Streamable HTTP 客户端；
- [x] M8-T02：实现 MCP Server 配置、初始化、工具发现、调用、超时和取消；
- [x] M8-T03：实现仅声明 MCP Server 的插件清单 Schema、签名元数据和严格校验；
- [x] M8-T04：通过统一 ToolRegistry 动态加载并命名空间化外部工具；
- [x] M8-T05：把 MCP/插件映射到现有权限、审批、脱敏和审计链；
- [x] M8-T06：实现 mcp/plugin list、show、enable、disable、凭据和 doctor 命令；
- [x] M8-T07：完成 stdio/HTTP 恶意输入、越权、超时、取消 E2E 和三平台 CI。
  - stdio E2E：完整生命周期、畸形/超大输出拒绝、调用超时、取消终止子进程；
  - 审批链 E2E：审批拒绝时 MCP 工具调用不发送到服务器；
  - 修复 Windows `clippy::large-enum-variant`（`Connection::Stdio` 装箱）。

## M9：自定义指令、记忆与可选 RAG

M9 在 M5 长上下文数据完成后重新评审，避免过早引入向量数据库。

- [ ] M9-T01：实现用户级与项目级指令加载；
- [ ] M9-T02：定义可审查的记忆建议和确认流程；
- [ ] M9-T03：实现 memory list/add/remove；
- [ ] M9-T04：实现相关性检索和上下文注入；
- [ ] M9-T05：评估本地全文检索是否已满足需求；
- [ ] M9-T06：仅在有真实语料和评测集时增加向量 RAG；
- [ ] M9-T07：完成泄漏、污染、删除和召回质量测试。

## M10：1.0 发布、诊断与兼容性

- [ ] M10-T01：完善 doctor、版本和升级提示；
- [ ] M10-T02：建立版本兼容和配置迁移策略；
- [ ] M10-T03：生成三平台二进制、校验和与来源证明；
- [ ] M10-T04：建立 Release 自动化和回滚流程；
- [ ] M10-T05：遥测保持默认关闭并提供显式授权；
- [ ] M10-T06：完成安装、升级、降级和卸载 E2E；
- [ ] M10-T07：冻结 1.0 CLI、配置和会话兼容约定。

## 1.0 之后再评估

- 持久终端与 PTY；
- 浏览器自动化；
- 截图和桌面操作；
- Computer Use；
- 云同步和多人协作。

这些能力扩大操作系统权限面，不应与 1.0 基础能力并行推进。

## 最终验收清单

- [ ] 默认模式不能越过工作区或执行任意命令；
- [ ] 副作用、网络、MCP 和插件统一经过策略、审批和审计；
- [ ] 支持中断、恢复、上下文压缩和安全撤销；
- [ ] 密钥不会进入配置明文、日志、会话或遥测；
- [ ] macOS、Linux、Windows 安装和核心测试通过；
- [ ] README、PRD、架构、设计、命令帮助和实际行为一致。
