# XDUDU M7 Plan 基础设计

> 范围：M7.0～M7.6，包括会话交互、Plan 领域、结构化生成、整份审批/修订、串行 DAG 执行、恢复和完整 CLI。

## 1. 边界

XDUDU 同时存在两种不同层级的运行机制：

- ReAct：一次用户请求内部的隐藏运行循环，根据工具结果继续判断；
- Plan：跨步骤、可展示、可审批、可持久化和可恢复的显式任务。

Plan 不记录思维链，也不能绕过 ToolRegistry、PermissionMode、ApprovalGate、ChangeLedger 或现有脱敏边界。

## 2. 会话内恢复

全屏 TUI 支持：

```text
/resume
/resume <UUID>
```

不带 ID 时读取当前工作区最近 20 个 SQLite 会话，并显示标题、状态和更新时间。上下键循环选择，Enter 恢复，Esc 或 Ctrl+C 取消。恢复后：

1. 校验会话属于当前工作区；
2. 更新当前交互循环使用的会话 ID；
3. 重建用户与助手时间线；
4. 保留会话中的工具记录，但不把工具结果作为普通对话重复渲染；
5. 下一条用户消息继续写入同一会话。

## 3. 领域模型

`Plan` 使用 `schemaVersion: 3`，包含 revision 审阅字段以及 executionVersion、currentStepId、startedAt 和 pausedReason：

- `id`、`sessionId`、`goal`；
- `status`、从 1 开始的 `revision`；
- `steps`；
- 创建、更新、提交审批和完成时间；
- 最多 100 条 `reviewHistory`，每条记录绑定 decision、reason 和当时的 revision。

`PlanRevision` 是不可变的完整内容快照，保存计划 ID、revision、目标、步骤、修改要求和创建时间。当前 `Plan` 是唯一可执行版本；旧快照只用于查看与审计。

`PlanStep` 包含：

- UUID、标题和描述；
- 依赖步骤 UUID；
- 完成条件；
- 状态、结果和错误；
- 开始和完成时间。

计划最多 100 个步骤；单步骤最多 50 个依赖、20 条完成条件。目标、标题和完成条件有独立字节上限。

## 4. 依赖与状态

依赖图必须是 DAG。创建和更新计划时统一拒绝：

- 不存在的依赖；
- 自身依赖；
- 重复依赖；
- 循环依赖；
- 重复步骤 ID。

计划状态：

```text
draft
  → pending_approval
      → approved
          → running
              → completed | failed | cancelled
      → rejected
      → revision + 1 → pending_approval
```

步骤状态：

```text
pending → ready → running → completed | failed | blocked | cancelled
             └────────────→ skipped
blocked | failed → ready | cancelled
```

全部依赖处于 `completed` 或 `skipped` 时，待执行步骤才会进入可运行集合。仍有未完成步骤时，计划不能迁移为 `completed`。

## 5. 持久化

SQLite Schema v3 在 `plans` 表中增加当前 revision，并新增快照表：

```text
plans(
  id,
  session_id,
  status,
  schema_version,
  revision,
  plan_json,
  created_at,
  updated_at,
  completed_at
)

plan_revisions(
  plan_id,
  revision,
  revision_json,
  change_request,
  created_at,
  PRIMARY KEY(plan_id, revision)
)
```

外键关联 `sessions`，并建立 `(session_id, updated_at DESC)` 索引。`PlanStore` 当前提供：

- `create_plan`；
- `update_plan`；
- `get_plan`；
- `latest_plan_for_session`；
- `update_plan_if_current`；
- `append_revision_if_current`；
- `list_plan_revisions`。

审批和修订使用 `WHERE revision = ? AND status = ?` 进行乐观并发更新。陈旧请求返回稳定的 `PLAN_CONFLICT`，不会覆盖较新的决定。修订时当前快照更新与新 revision 快照插入处于同一个 SQLite 事务。

Schema v2 到 v3 的迁移也在单个事务中完成：兼容读取 Plan Schema v1、转换为 v2、回填 revision 1、重写当前 JSON 并写迁移标记。任何损坏记录都会使整次迁移回滚。

计划目标、步骤内容、完成条件、结果和错误在落盘前统一脱敏。M5 旧会话中的 `plan` JSON 字段继续保留；M7 不做破坏性迁移。

## 6. 结构化计划生成

M7.2 新增 `generate_plan` 服务边界。它接收会话 ID、原始目标、可选上下文、模型、工作区、Provider、PlanStore 和取消令牌，完成一次独立规划请求：

1. 校验目标、上下文和模型名称的大小边界；
2. 使用与普通 ReAct Prompt 隔离的规划 Prompt；
3. 只向 Provider 提供协议工具 `submit_plan`；
4. 严格解析响应并建立步骤 key 到 UUID 的映射；
5. 复用 Plan 领域层校验依赖 DAG；
6. 通过 `PlanStore` 保存 `draft`，同时返回模型 Token 用量。

`submit_plan` 是 Provider 结构化输出协议，不进入 `ToolRegistry`，因此不是一个可由 Agent 任意调用的运行时工具，也不产生文件、进程、网络或其他副作用。输入由步骤数组组成，每个步骤包含：

- 唯一 ASCII `key`；
- 标题与描述；
- 同一计划内的依赖 key；
- 至少一条可验证的完成条件。

协议采用 `additionalProperties: false`，Rust DTO 同时启用 `deny_unknown_fields`。运行时拒绝：

- `FinishReason` 不是 `tool_calls`；
- 普通文本或 Markdown 夹带；
- 零次、多次或错误名称的协议工具调用；
- 截断、内容过滤和 Provider 协议错误；
- 空步骤、重复或非法 key、未知依赖；
- 自身依赖、重复依赖和循环依赖；
- 超出 Plan 领域上限的文本、依赖和完成条件。

所有步骤校验完成前不会调用 `PlanStore`。成功记录始终处于 `draft`，M7.2 不把普通模型文本当作完成信号，也不迁移到审批或运行状态。

## 7. M7.3 整份计划审批与修订

用户通过 `/plan <目标>` 显式进入规划流程。新会话会先落盘，已有会话只注入脱敏摘要和最近用户/助手文本，不携带无限工具输出。生成 Draft 后立即提交为 `pending_approval`，同一会话不能并存第二个 Draft、待审批、已批准或运行中的计划。

全屏 TUI 展示目标、revision、步骤、依赖编号和完成条件。菜单支持：

- 批准计划：写入 Approved 审阅记录，并把 Session 置为 `plan_ready`；
- 请求修改：读取自然语言要求，由独立 `revise_plan` Provider 协议生成完整新版本；
- 拒绝计划：写入 Rejected 记录，计划和 Session 进入终态；
- Esc/Ctrl+C：仅关闭界面，保持 `pending_approval`。

默认选择“拒绝计划”。小终端可使用 PageUp/PageDown 滚动。非 TTY 模式可以生成并保存待审批计划，但不会进行伪交互审批。

自然语言修订只能发生在 `pending_approval`：Provider 必须且只能调用一次 `revise_plan`，不得夹带普通文本，结构继续使用严格 DTO、步骤边界和 DAG 校验。Provider 错误、截断、取消或校验失败时数据库完全不变。修订成功后保留 Plan ID、Session ID 和 created_at，全部步骤使用新 UUID，revision 加一并立即重新等待整份审批。

Plan 审批不复用工具 ApprovalGate。批准方案只表示用户认可任务结构，未来执行文件、进程或网络工具时仍须经过原有 PermissionMode、ApprovalGate 和 ToolRegistry。

## 8. M7.4 串行 DAG 执行器

`PlanExecutor` 只接受 Approved 计划首次运行，或 Paused 计划显式重试。它根据依赖状态计算 Ready 步骤，并始终按计划原始顺序选择第一个 Ready 节点，当前版本不并行运行 DAG 分支。

每个步骤开始前创建 `PlanStepAttempt`，记录连续 attempt 编号、开始时间和后续真实工具调用 ID。步骤 Provider 可以使用 ToolRegistry 中的真实工具，但完成步骤只能调用不会注册进 ToolRegistry 的内部协议 `complete_step`：

```json
{
  "summary": "步骤结果",
  "evidence": [
    { "criterionIndex": 1, "evidence": "支持第一条完成条件的真实结果" }
  ]
}
```

`complete_step` 必须单独出现、不能夹带普通文本，证据索引从 1 开始、不得重复或越界，并且必须覆盖所有完成条件。存在未处理的工具失败、Provider 正常停止但未调用协议、响应截断或达到轮次上限时，步骤均不能标记 Completed。

真实文件、进程和网络调用继续经过 ToolRegistry、PermissionMode、ApprovalGate 和 ChangeLedger。Plan 审批不会成为工具审批的通配授权。

## 9. SQLite v4 与执行检查点

SQLite Schema v4 为 plans 增加 `execution_version`。执行更新使用 `id + revision + execution_version + status` 的比较并交换条件，Plan 与对应 Session 在同一个事务中更新。每次开始 attempt、记录工具调用、写入工具结果、完成步骤、暂停或完成计划都会推进 executionVersion。条件不匹配时返回 `PLAN_CONFLICT` 并立即停止。

Plan Schema v2 和历史 PlanRevision 在启动迁移中升级到 v3；数据库迁移在一个事务内完成，损坏记录会使迁移整体回滚。执行摘要、证据和错误与其他 Plan 内容一样在落盘前统一脱敏。

## 10. 暂停、重试、取消与崩溃恢复

工具审批拒绝、Provider/协议错误、轮次上限、Ctrl+C 和执行冲突都会停止自动推进并持久化 Paused。逻辑失败的 attempt 标记 Failed；用户中断、审批拒绝或结果未知标记 Interrupted，步骤标记 Blocked。已经完成的步骤不会回滚，已发生的外部副作用也不会自动撤销。

启动时发现 Running Plan 会取消 Session 中 Pending/Running 的工具记录，把 Running attempt 改为 Interrupted、步骤改为 Blocked、Plan 改为 Paused，并写明结果未知。恢复过程不调用 Provider、不运行工具；只有用户明确选择后才创建新 attempt。

显式取消把未完成步骤标记 Cancelled。取消不等于撤销副作用；文件修改如需恢复仍使用独立事务账本和 `xdudu undo`。

## 11. M7.5 CLI 与 TUI

交互模式支持 `/plan new/status/run/retry/cancel/revisions`。`/resume` 遇到 PendingApproval 时打开审阅界面，遇到 Paused 时展示暂停原因、步骤状态、当前 attempt、完成条件和证据，并提供继续、重试、查看详情与取消选项。

非交互模式支持 `xdudu plan create/list/show/revisions/approve/reject/revise/run/retry/cancel`。`run` 仅接受 Approved，`retry` 仅接受 Paused；副作用工具继续遵守审批策略。终端与 JSON Lines 都输出稳定的 Plan 生命周期事件。

## 12. M7 完成边界

M7 已完成显式计划从生成、修订、审批到执行、暂停和恢复的闭环。当前刻意不包含并行 DAG、运行中修改计划、自动回滚外部副作用、Provider 扩展、MCP、插件、Skills 和 RAG。
