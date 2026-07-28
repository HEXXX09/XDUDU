# XDUDU v0.4.0 安全治理设计与验收

> 状态：本地实现与测试完成，待当前分支推送后由远端三平台 CI 验证。

## 1. 阶段目标

M4 在继续增加工具能力前，先把“允许什么、谁来确认、留下什么记录、如何恢复”收敛为一条默认拒绝的执行链。Provider 扩展不属于本阶段；按当前产品决定，DeepSeek 保持主用路径。

本版本同时完成产品名从 XYCLI 到 XDUDU 的迁移。新名称的数据源优先，旧配置和会话仅作为兼容读取来源；旧系统凭据读取成功后一次性迁移到新的 `xdudu` 钥匙串项，避免每次启动重复申请访问 `xycli`。所有新写入使用 XDUDU 名称。

## 2. 统一执行链

```text
工具存在
  → PermissionMode 能力检查
  → JSON 输入严格校验
  → SideEffectKind 分类
  → ApprovalGate 决策
  → 工具内部路径或命令策略
  → 执行与超时控制
  → ChangeLedger / ToolCallRecord
  → Redaction
  → 会话持久化与终端输出
```

任何环节失败都会停止后续步骤。默认构造的 `ToolRegistry` 使用拒绝型 ApprovalGate，测试或上层调用方必须显式注入放行策略。

## 3. 权限与审批

权限回答“这类能力原则上是否允许”，审批回答“这一次副作用是否执行”，两者不能互相替代。

| 工具 | 权限级别 | 副作用 |
| --- | --- | --- |
| `file_read` | 只读 | `none` |
| `file_write` | 文件写入 | `workspace_write` |
| `terminal_exec` | 安全命令或完整执行 | `process_execution` |

审批模式：

- `ask`：默认值；交互终端逐次询问；
- `never`：拒绝所有副作用；
- `always`：自动批准，适合调用方已建立外部隔离的自动化；
- `ask` 在管道、一次性命令和 JSON 模式中按拒绝处理，不能因为没有 TTY 而隐式放行。

每次审批记录请求时间、决定时间、副作用类型、结果和原因，并进入会话工具调用记录。

## 4. 项目配置信任边界

工作区中的 `.xdudu/config.toml` 可能来自不可信仓库，因此：

- 不能设置 `provider.base_url`，避免把用户凭据发送到仓库指定端点；
- 不能把当前 `agent.permission` 调宽；
- 不能把当前 `agent.approval` 调宽；
- 仍然禁止任何 key、token、secret、password 等秘密字段。

用户配置、环境变量和 CLI 属于用户主动控制的更高信任层，但最终仍需通过 URL、范围和枚举校验。

## 5. 脱敏边界

统一脱敏覆盖：

- `sk-`、`xai-` 和常见 GitHub Token 前缀；
- `Bearer` Authorization；
- PEM 私钥块；
- API Key、Token、Secret、Password、Authorization 等结构化字段。

脱敏发生在会话落盘、终端文本、JSON Lines 事件和顶层错误输出之前。审批提示也只展示脱敏后的工具输入。变更账本为了精确撤销需要保存原文件前镜像，因此独立存放，不回传模型；Unix 上账本文件权限设置为 `0600`。

## 6. 文件变更账本

`file_write` 成功落盘后立即记录：

- 变更 ID、会话 ID、工具调用 ID；
- 工作区相对路径；
- 变更前镜像和 SHA-256；
- 变更后 SHA-256；
- `applied` 或 `undone` 状态与时间。

若账本写入失败，工具会尝试恢复原文件并返回 `CHANGE_LEDGER_ERROR`，不会把不可审计写入报告为成功。

## 7. 安全撤销

```bash
xdudu undo
xdudu undo --change <变更UUID>
xdudu --session <会话UUID> undo
```

撤销命令不初始化 Provider，也不读取 API Key。执行顺序：

1. 选择指定记录，或指定会话/全局最近一条 `applied` 记录；
2. 拒绝绝对路径、父目录和当前指向工作区外的路径；
3. 读取当前文件并比较写入后 SHA-256；
4. 哈希不同则拒绝，保留用户或其他程序的后续修改；
5. 有前镜像则恢复，原本不存在则删除 Agent 新建文件；
6. 账本状态更新为 `undone`，重复撤销被拒绝。

当前撤销边界仅覆盖 `file_write`。`terminal_exec` 可能产生任意外部副作用，不能声称可自动回滚。

## 8. 验收覆盖

- 默认 Gate 拒绝副作用；
- `read-only` 在审批之前拒绝写入；
- 非交互 `ask` 拒绝，显式 `always` 可执行；
- 审批结果写入会话；
- 密钥、Bearer Token、私钥和敏感 JSON 字段不进入会话或 Renderer；
- 项目配置不能替换 Base URL、提升权限或自动批准；
- 写入成功后生成变更记录；
- 撤销可恢复旧文件或删除 Agent 新建文件；
- 文件后续被修改时撤销失败且内容不变；
- `undo` 在没有 Provider API Key 时可独立运行。

本地质量门禁：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --release --locked
```

## 9. 下一阶段

M5 优先解决 SQLite、跨进程一致性、会话查询/恢复和上下文压缩。JSON 会话与账本目前只有单进程互斥，多个 XDUDU 进程同时操作同一工作区时还没有数据库事务和跨进程锁；这是下一阶段必须补齐的可靠性边界。
