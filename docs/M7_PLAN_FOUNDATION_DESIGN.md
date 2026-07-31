# XDUDU M7 Plan 基础设计

> 范围：M7.0 会话交互、M7.1 Plan 领域基础与 M7.2 结构化计划生成。本文不包含执行审批和步骤调度。

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

`Plan` 使用 `schemaVersion: 1`，包含：

- `id`、`sessionId`、`goal`；
- `status`；
- `steps`；
- 创建、更新、审批和完成时间。

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
      → draft
      → approved
          → running
              → completed | failed | cancelled
```

步骤状态：

```text
pending → ready → running → completed | failed | blocked | cancelled
             └────────────→ skipped
blocked | failed → ready | cancelled
```

全部依赖处于 `completed` 或 `skipped` 时，待执行步骤才会进入可运行集合。仍有未完成步骤时，计划不能迁移为 `completed`。

## 5. 持久化

SQLite Schema v2 新增 `plans` 表：

```text
plans(
  id,
  session_id,
  status,
  schema_version,
  plan_json,
  created_at,
  updated_at,
  completed_at
)
```

外键关联 `sessions`，并建立 `(session_id, updated_at DESC)` 索引。`PlanStore` 当前提供：

- `create_plan`；
- `update_plan`；
- `get_plan`；
- `latest_plan_for_session`。

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

## 7. 后续阶段

M7.3 起继续实现：

1. 执行前审批、拒绝和计划修改；
2. 步骤调度与内部 `complete_step` 协议；
3. 中断、崩溃和恢复；
4. Plan CLI、TUI 进度和完整 E2E。
