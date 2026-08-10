# M11 上下文、记忆与 Web 阅读设计

## 1. 范围

两项上下文能力与一项 Web 能力：

- **LLM 上下文压缩**：将 `agent.rs` 现有「确定性截断摘要」升级为「LLM 结构化压缩 + 确定性回退」；
- **记忆/RAG 优化**：`search_memories` 注入从「固定取前 3 条」升级为「排序 + Token 预算 + 去重」；
- **`web_read` 工具**：分段拉取大型页面 + LLM 提炼子循环，解决 `web_fetch` 无法读取大页面的问题。

## 2. LLM 上下文压缩

### 2.1 现状

`compact_context` 在超预算时用 `summarize_messages` 做确定性截断（每条消息 content 截断到 600 字、tool call 400 字、汇总上限 12000 字）。优点是零额外成本、无失败路径；缺点是压缩丢失结构、长会话信息衰减明显。

### 2.2 设计：分级压缩

```text
预算超限？
  ├─ 低于 3× 预算：确定性截断（现有 compact_context，零成本）
  └─ ≥ 3× 预算：LLM 压缩
       ├─ 构建压缩输入（最近未压缩的完整消息，容量 ≤ 64KB）
       ├─ 发起结构化压缩请求（专用 dto 协议）
       ├─ 成功 → 写入 context_summary，记录压缩点
       └─ 失败/超时 → 回退确定性截断（不中断 Agent）
```

判断标准就是是否触发一次模型调用；压缩本身是**幂等、可观察、可回退**的操作。

### 2.3 结构化压缩协议

参照 `memory_suggestion` 的 `tools-only` 模式，但 LLM 压缩是「对话总结」而非「JSON 列表」，因此采用**两段式**：

```text
请求（独立 ProviderRequest）：
  system = 压缩指令（"对最近 N 条消息做结构化总结，保留计划、已确认结论、尚未完成事项"）
  messages = [User: 待压缩的消息列表]
  tools = [ submit_context_summary ]   // 单工具-only

响应（严格解析）：
  finish_reason 必须 ToolCalls
  工具 = submit_context_summary
  DTO deny_unknown_fields：
    summary: String (1..=8192)
    key_facts: Vec<String> (1..=32，每条 ≤512)
    open_items: Vec<String> (1..=32)
```

写入 `session.context_summary` 的文本为：

```text
## 会话压缩摘要（LLM）
{summary}
### 关键事实
- ...
### 待办
- ...
```

### 2.4 回退与容错

- LLM 压缩发生任何错误（协议不符、超时、finish_reason 非 ToolCalls）：**静默回退到确定性截断**，仅发 `Warning {code:"CONTEXT_COMPACT_LLM_FALLBACK"}`，绝不中断 Agent；
- 预算充足时不主动压缩；`compact` 命令（TUI `/compact`）可强制触发一次 LLM 压缩；
- 压缩输入保留 `session.context_summary` 旧摘要，避免压缩链丢失历史。

### 2.5 token 估算改进

现有 `estimated_tokens = chars/2+8` 对中文偏差大。改用「字符加权」估算并用 CLI flag 可调：

```
estimated_tokens(text)= ceil( (ascii_bytes + 2×cjk_chars) / 3.5 ) + 8
```

（保留旧函数名，行为不变仅为精度优化。）

## 3. 记忆/RAG 优化

### 3.1 现状

- `relevant_memories(c, 3)` 固定取 3 条、无排序、无预算、无去重；
- 注入是拼进系统提示词末尾的字符串。

### 3.2 注入管线

```
search_memories(query, topK=16)          // FTS5 召回
   → 相关性降序（查询词命中数量加权，含标题词加分）
   → token 预算裁减（memory.injection_token_budget，默认 1500 tokens）
   → 去重（内容归一化哈希）→ ≤ 8 条
   → 注入系统提示词「## 相关记忆」
```

- `query` 用当前用户 prompt + 最近助手文本拼接（提高召回）；
- 每条记忆显示来源会话 ID；提供「不再注入此记忆」的负面信号（可选后续）；
- `memory.injection_token_budget` 默认 1500、`memory.top_k` 默认 8，均为配置可调。

## 4. `web_read` 工具

### 4.1 动机

`web_fetch` 有 `MAX_BYTES=1MiB` 硬上限，大文档单元格被截断直接失败；且一次请求无法“读完整页”。`web_read` 提供多段拉取与 **LLM 提炼**。

### 4.2 工具定义

```json
{
  "type": "object",
  "required": ["url", "goal"],
  "additionalProperties": false,
  "properties": {
    "url":  { "type": "string", "minLength": 1, "maxLength": 8192 },
    "goal": { "type": "string", "minLength": 1, "maxLength": 2048 },
    "maxChunks": { "type": "integer", "minimum": 1, "maximum": 8, "default": 8 },
    "startRef": { "type": "integer", "minimum": 0, "maximum": 10000, "default": 0 }
  }
}
```

side_effect：`NetworkAccess`（复用审批链）；name：`web_read`；timeout：默认 90s（大页面多段）。

### 4.3 执行流程

```
1. SSRF/DNS 校验（复用 web_fetch.pinned_client + validate_url）
2. 流式读取响应，单次网络响应最多保留 1 MiB；响应类型只接受 HTML、纯文本和 JSON
3. 将 HTML 转纯文本并按固定字符预算切分块段；超出提炼预算时均匀采样首尾与中间内容
4. 最多选择 8 个文本块送入 LLM 提炼器（每块 ≤ 8 KiB），增量累积：
   - 每块输出 submit_content_summary（附属 DTO）
5. 汇总最终 JSON：
   {
     "url", "title", "summary": "...",     // LLM 提炼
     "keyPoints": [...],                   // 供继续阅读
     "chunksRead": 3, "totalChunks": 6,    // 截断提示
     "truncated": true, "nextStartRef": 3  // 支持继续（下一轮传 startRef）
   }
```

- 提炼使用与主循环同一 Provider，但工具内持有独立 `ProviderRequest`（不可见思维链）；
- 提炼失败回退为「返回可读纯文本段」（不依赖 LLM，不阻塞工作）。

### 4.4 安全与限制

- 复用 `web_fetch` 的 SSRF/DNS/内容类型边界，不下载文件、不携带 Cookie/凭据、无代理；
- `maxChunks ≤ 8`，每次响应 ≤ 1 MiB，单次调用最多 8 次提炼请求，每次输出 ≤ 4 KiB；
- 只有服务器正确支持 Range 时才能通过 `startRef` 续读；服务器忽略 Range 时明确返回
  `RANGE_UNSUPPORTED`，不会重复下载同一首段；
- `web_read` 不可写文件，不启动进程；
- 提炼请求隐藏思考、不进入会话历史（区别于 agent 主循环消息）。

## 5. 事件与预算

- `AgentEvent::ToolProgress` 复用为每 chunk `phase: "reading" | "summarizing"`；
- LLM 提炼不写入会话消息块；提炼 Token 暂未合并进会话 `total_tokens`，这是发布前仍需
  评估的可观测性缺口，不影响网络与权限边界。

## 6. 测试与验收

- 压缩：`compact_context` 超 3× 预算触发 LLM 调用（mock provider），失败回退确定性；`/compact` 强制压缩；
- 记忆：`search`（相关度排序，>=预算被裁、去重）断言注入条数 ≤ top_k；
- web_read：切块、采样、Schema 上限与回环 SSRF 拒绝已覆盖；真实 Range 大 HTML、超时、
  审批和 Provider 提炼失败 E2E 留待三平台 CI 验收；
- 现有功能回归：`file_read`/`web_fetch`/`search_text` 行为不变。
