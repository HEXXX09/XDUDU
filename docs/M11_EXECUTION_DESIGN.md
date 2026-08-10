# M11 Agent 编排设计：子代理、并行执行与停滞检测

## 1. 范围

M11 在既有 `run_agent` 主循环之上补齐主流 Agent 的编排能力，包含三项：

- **子代理（SubAgent）体系**：主代理可以通过统一的 `task` 工具把只读调研、外部文档或依赖库研究等任务委派给隔离的子上下文，并在同一批中并行执行多个子代理；
- **并行工具执行**：主循环批次内无副作用（只读）工具并发执行，副作用工具保持串行，不改变变更账本单写者与审批顺序约束；
- **停滞检测**：识别重复失败、无进展动作序列，注入恢复指令，并在不可恢复时以 `Incomplete` 暂停并保留现场。

设计目标是不改动 `xdudu-cli` 的领域边界：`xdudu-core` 不读取终端输入、不打印输出，全部新能力通过 `AgentEvent` 与 `EventSink` 暴露，CLI 负责渲染。

## 2. 子代理体系

### 2.1 领域模型

新增 `agent.rs` 中的统一 Agent 配置：

```text
AgentProfile {
  id: String                 // build / plan / explore / general / reviewer 等
  description: String        // 对模型可见，用于 task 工具选择
  mode: Primary | Subagent | All
  model: Option<String>      // 覆盖父会话模型
  permission: PermissionMode // 子代理权限模式，独立于父会话
  allowed_tools: Option<Vec<String>>   // None = 跟随 permission；Some = 显式白名单
  max_turns: u32
  system_extra: Option<String>        // 追加到系统提示词
}
```

内置档案：

| ID | 模式 | 定位 | 默认工具 |
| --- | --- | --- | --- |
| `build` | primary | 默认主代理，全量工具 | 全部内置工具 |
| `plan` | primary | 只读规划，不产生副作用文件 | 只读工具 + Plan 协议 |
| `explore` | subagent | 快速只读代码库探索 | read/search/git 只读 + web 只读 |
| `general` | subagent | 通用多步任务执行 | 全部内置工具 |
| `reviewer` | subagent | 代码审查，禁止修改 | read/search/git 只读 |

用户可以在用户级与项目级目录提供自定义 `toml` 档案，项目档案视为不可信输入，不能扩大权限或绕过审批。

### 2.2 协议化委派（`task` 工具）

`task` 是唯一对外暴露的委派工具，Schema 严格，输入：

```json
{
  "type": "object",
  "required": ["agent", "prompt"],
  "additionalProperties": false,
  "properties": {
    "agent": { "type": "string", "enum": ["explore", "general", "reviewer", "build"] },
    "prompt": { "type": "string", "minLength": 1, "maxLength": 8192 }
  }
}
```

执行流程（复用已验证的 `optional-or-error` 子循环模式，与 `execute_step` 同构）：

```text
1. 读取 AgentProfile，构建独立 system prompt（build_system_prompt + profile.system_override）
2. 构建受限工具集：内置工具 filtering 到 profile.allowed_tools
3. 建立隔离上下文窗口（不写入父会话 messages）
4. 循环不超过 profile.max_turns：
   - chat 请求（父会话同一 Provider/模型，或 profile.model）
   - 若返回 tool_calls：在隔离窗内执行工具（走同一 Permission/Approval/Redaction 链），结果落回隔离消息
   - 若 finish_reason = Stop 且无未完成工具：返回最终文本为结构化结果
5. 超出 max_turns 或发生不可恢复错误：返回失败 ToolResult（错误码 SUBAGENT_INCOMPLETE），不影响主循环其他调用
```

关键守则：

- 子代理上下文**不写入父会话历史**，避免污染主循环上下文压缩窗口；
- 子代理工具执行结果仍然进入父会话的 `ToolCallRecord`（审计可见），Markdown 渲染为子代理卡片；
- `task` 调用本身需要权限（默认 `AutoSafe` 允许 read-only 子代理，`general/build` 需要 `full-access` 或审批），不能通过子代理绕过审批；
- 是子代理的拒绝、失败在父循环中转换为普通 `ToolResult`，而不是异常中断。

### 2.3 并行子代理

同一批中多个独立的 `task` 调用（探针/explore）使用 `tokio::join_all` 并发执行。每个子代理独立创建隔离上下文。

## 3. 并行工具执行

### 3.1 并行度策略

| 工具类别 | 判定 | 执行方式 |
| --- | --- | --- |
| 无副作用 | `side_effect == SideEffectKind::None` | 同批并发 `join_all` |
| 有副作用 | 需要审批/账本 | 保持串行，沿用 `batch side-effect` 语义 |

- 只读工具（`file_read`、`search_text`、`git_status`、`git_diff`、`web_search`、`web_fetch` 只读部分不审批时）可并行；
- `file_write`、`apply_patch`、`terminal_exec` 等有副作用工具**绝不并行**，避免账本单写者竞争与审批顺序不可观测；
- 并行批次内任一 `ToolResult` 失败不影响其他只读调用继续完成，但最终状态由模型下一轮观察决定。

### 3.2 进度通道复用

当前 `agent.rs` 每轮创建一次 64 容量的 `mpsc::channel`。优化：在同一批次内创建 **单个** 进度通道，进度事件携带 `call_id` 分发；既有 `ToolProgress` 事件字段不变，渲染层无感知。

## 4. 停滞检测

### 4.1 检测信号

在 `run_agent` 循环内维护一个最近工具动作的滑动窗口（如最近 8 个 `ToolCallRecord`）：

- **重复失败**：同一 `tool_name` 连续失败 ≥ 3 次，且错误码相同；
- **无进展**：最近 4 轮模型输出极短（assistant_text < 16 字符）且没有成功工具调用，形成循环。

### 4.2 恢复策略

检测到停滞时发出：

```rust
AgentEvent::Stalled { reason, recovery: String }
```

`recovery` 是一条人类与模型可见的提示（例如：“仍多次尝试 file_write 失败，建议改用 apply_patch 或先读文件再重试”）。默认 `stalled.recovery = auto`：

- 附加恢复提示到 `system`（系统提示词）并继续，累计达到阈值（`stalled.max_recovery_attempts = 2`）仍停滞则 `Incomplete` 收尾；
- `stalled.mode = ask`：切换为暂停，向用户请求下一步（复用现有审批/暂停路径）；
- `stalled.mode = off`：完全关闭。

配置项：`agent.stalled_recovery`（`auto|ask|off`）、`agent.stalled_recovery_max`（默认 3）。

## 5. 事件与渲染

新增事件（`events.rs`）：

```text
SubagentStarted { agent_id, prompt }
SubagentProgress { agent_id, call_id, phase, completed, total, unit, message }
SubagentFinished { agent_id, result: ToolResult }
StalledRecovery { repeats, tool_names: Vec<String>, recovery: String }
```

TUI 侧：`tool activity` 条目与 `pending_tasks` 面板显示子代理；普通 Renderer 只降低一级缩进输出 `subagent: explore →` 前缀，`--json` 输出原始事件。

## 6. 配置

```toml
[agent]
max_turns = 25
stalled_recovery = "auto"      # auto | ask | off
stalled_max_recovery = 3

[agent.profiles]               # 可选自定义档案
explore.model = "deepseek-v4-pro"
reviewer.model = "claude-sonnet-4-5"
```

项目配置 `.xdudu/config.toml` 可以新增档案，但不能覆盖任何内置档案或用项目档案扩大权限/审批（与现有项目 `base_url`、`permission` 限制一致）。

## 7. 安全边界

- 子代理永远不获得父会话没有的权限；`task` 工具默认需要审批级别不低于 `PermissionLevel::ReadOnly`；
- 子代理工具的 `ChangeLedger` 与父会话共享同一实例，`hex` 事务预检仍然工作，`undo` 可跨子代理整批恢复；
- 并行只读工具不触碰变更账本，`ChangeLedger` 仍为单写者；
- 子代理产生的输出同样经过 `redact_text`。

## 8. 测试与验收

- 单元：`task` 工具 schema 校验、子代理并行 `join_all` 数量、停滞滑动窗口信号；
- Mock Provider：子代理工具调用返回 → 主循环只拿到终结结果且不回写父 history；
- 并行：三机只读工具并行后日志无条数回归，且副作用工具仍串行（断言事件顺序）；
- 停滞：Mock 连续失败 > 阈值后主循环 `Incomplete` 并带恢复提示；
- CLI E2E：`/agent`、`/skills` 命令、JSON lines 中出现 `subagent*` 事件。

## 7. 子代理任务图扩展

同一 Provider 回合返回多个 `task` 只能表达“彼此独立、立即并行”，无法表达依赖链。新增
内部协议工具 `task_graph`，一次提交完整 DAG：

```json
{
  "tasks": [
    {"id":"inspect","agent":"explore","prompt":"定位实现"},
    {"id":"review","agent":"reviewer","prompt":"审查风险"},
    {
      "id":"synthesize",
      "agent":"explore",
      "prompt":"汇总结论",
      "dependsOn":["inspect","review"]
    }
  ],
  "maxConcurrency": 2,
  "failurePolicy": "continue-independent"
}
```

运行时约束：

- 节点 1～24 个，ID 唯一且必须匹配安全字符集；总 prompt 不超过 64 KiB；
- 完整预检未知档案、不可委派档案、缺失依赖、重复依赖、自依赖和循环；失败时零节点执行；
- 按声明顺序扫描 Ready 节点，依赖全部 succeeded 后才能启动；
- `explore`、`reviewer` 等显式 ReadOnly 档案可并行，最大并发为 4；
- `general`、`build` 或其他非只读档案独占调度器，内部工具仍逐次审批；
- 成功前置结果以“不可信背景”注入后继 prompt，单结果 8,000 字符、总计 24,000 字符；
- failed/cancelled 节点的所有后继变为 blocked；独立分支默认继续，fail-fast 则取消未启动分支；
- 每个内部工具审计 ID 加 `graph.<graphId>.<nodeId>.` 前缀，Token 汇总回父会话；
- 整张图仍是父会话中的一个 Pending 工具调用。进程崩溃时按现有规则标记结果未知并取消，
  不自动重放任何节点或外部副作用。

任务图用于单次 ReAct 回合内的结构化委派；它不替代 M7 Plan。Plan 是跨步骤、持久化、
需用户整体审批和可恢复的任务执行协议，任务图则是一次工具调用内部的短生命周期调度。
