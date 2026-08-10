# M11 Provider 生态与思考路径设计

## 1. 范围

M11 在保持「公开输出不展示原始思维链」的前提下：

- **新增 OpenAI 兼容 Provider**（`openai-compatible`，可自定义 `base_url`），复用 DeepSeek 已验证的 OpenAI wire 协议；
- **打通思考（thinking/reasoning）闭环**：DeepSeek `reasoning_content` 进入会话持久化并在工具循环中回传，公开 TUI/JSON 始终不展示思维链；
- **配置化 `temperature` / `max_output_tokens` / `reasoning`**，替换 `agent.rs` 中硬编码的 `0.2` / `4096`。

## 2. Provider 命名与工厂

新增 Provider 名 `openai-compatible`（配置名 `provider.name = "openai-compatible"`）：

| 配置 `provider.name` | 协议 | 默认 base_url | 环境变量 |
| --- | --- | --- | --- |
| `deepseek` | OpenAI Chat Completions | `https://api.deepseek.com` | `DEEPSEEK_API_KEY` |
| `anthropic` | Anthropic Messages | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `openai-compatible` | OpenAI Chat Completions | 必填 `base_url` | `OPENAI_COMPATIBLE_API_KEY` |

- 工厂 `DefaultProviderFactory` 增加 `openai-compatible` 分支，创建 `OpenAiCompatibleProvider`（复用 `deepseek.rs` 的请求/SSE/工具聚合骨架，抽公共 `OpenAiWireProvider`）：
  - 请求体：`model/messages/tools/temperature/max_tokens`；
  - 响应：解释 `message.content`、`tool_calls`、`usage.prompt_tokens/completion_tokens`；
  - stream：`choices[].delta`（`content`、`reasoning_content`、`tool_calls`）+ `stream_options.include_usage`。
- `config.rs` 的 Provider 白名单（`validate`、`write_config_value`）从 `anthropic|deepseek` 扩展为 `anthropic|deepseek|openai-compatible`；
- `credentials.rs::env_name` 增加 `openai-compatible → OPENAI_COMPATIBLE_API_KEY`；`xdudu auth login openai-compatible` 与 `auth status` 自动生效；
- `ui.rs` 模型选择器 `model_options` 增加 `openai-compatible` 分支（返回当前模型或占位）与 `model_display_name` 映射。

## 2. 思考（thinking）闭环

### 2.1 现状与问题

- DeepSeek V4 因工具循环回传 `reasoning_content` 会破坏协议，当前 `deepseek.rs` 对 V4 显式发送 `thinking: disabled` 并丢弃 `reasoning_content`；
- `Message`（session.rs）、`provider_messages`、`ProviderResponse`、`ProviderStreamEvent` 均无 reasoning 载体。

### 2.2 领域模型扩展

新增 `content_block` 的推理载体（仅内部传递，公开输出由 AgentEvent 层剥离）：

```rust
// provider/mod.rs
pub enum ContentBlock {
    Text { text: String },
    Thinking { text: String },              // 新增，内部推理
    ToolUse { .. },
    ToolResult { .. },
}

pub struct ProviderResponse {
    pub message: ProviderMessage,
    pub reasoning: Option<String>,          // 新增：聚合的内部推理（用于 session 持久化）
    ..其他不变
}

pub enum ProviderStreamEvent {
    TextDelta { text: String },
    ReasoningDelta { text: String },        // 新增，stream sink 接收，不进入公开文本
}
```

`ProviderMessage::text_content()` 只拼接 `Text` 块（thinking 不进入文本通道）。

### 2.3 会话持久化

`Message` 增加字段：

```rust
pub struct Message {
   ..
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub reasoning: Option<String>,   // 内部推理，持久化
}
```

- 写入时统一 `redact_text`（推理可能含敏感中间态）；
- **SQLite schema v6 迁移**：无需新增表，`sessions.session_json` 内嵌 `Message` 的序列化增加 `reasoning` 字段即可；`#[serde(default, skip_serializing_if)]` 保证旧 JSON 与旧会话兼容（迁移仅更新 `schema_migrations` 版本号，不动表结构）；`session apply_patch / 纠错` 后自动生效。
- `provider_messages`（OpenAI wire）把 `Message.reasoning` 注入到返回的 assistant message 中对应的 `reasoning_content`：
  - OpenAI wire：构建 assistant message 时 `content = { "reasoning_content": reasoning, ..}`（DeepSeek 接受的字段）；
  - Anthropic：扩展思考块按 Anthropic 格式 `thinking` 回传（保留 signature block），仅当该 provider 支持且配置开启。

### 2.4 请求开关

`ProviderRequest` 新增：

```rust
pub struct ProviderRequest {
   ..
   pub reasoning: ReasoningMode,           // Disabled | Enabled
}
pub enum ReasoningMode { Disabled, Enabled }
```

- `deepseek.rs`：`reasoning == Enabled` 时不发送 `thinking: disabled`，模型返回 `reasoning_content` → 解析到 `ProviderResponse.reasoning` / `ReasoningDelta`；`Disabled` 保持现状（V4 显式关思考）；
- 默认值：由配置 `provider.reasoning` 决定（见 §3），未配置时不发送 thinking 字段（兼容旧 Provider）。

### 2.5 公开输出边界（不变式）

- `AgentEvent::AssistantDelta` 永不包含 thinking；`ReasoningDelta` 只在流式 sink 内部流转，不发给公开 `TuiRenderer`；
- 公开路径断言：`--debug-trace` 只输出结构化元数据（不包含思维链正文）；
- 会话中持久化的 `reasoning` 在 `session list/show` 与 `--json transcript` 输出中**默认遮蔽**，仅 `--debug-trace` 打开时才显示状态位（不含正文）。

## 3. 配置化 temperature / max_output_tokens / reasoning

### 3.1 配置键

`ProviderConfig` 新增三个字段（均有默认值，旧配置兼容）：

```toml
[provider]
temperature = 0.2            # 0.0..=2.0，默认 0.2
max_output_tokens = 4096     # 1..=32768，默认 4096（Anthropic 需 >1）
reasoning = false            # 是否启用内部思考闭环
```

- `config.rs::validate` 校验范围；
- `write_config_value` whitelist 增加上述三键；
- CLI 覆盖（`ConfigOverrides`）增加 `--temperature`、`--max-output-tokens`、`--reasoning/--no-reasoning`；
- `agent.rs`、`plan_generation.rs`、`plan_review.rs`、`plan_executor.rs`、`memory_suggestion.rs` 的请求构造全部改用配置值，删除硬编码 `0.2` / `4096` / `2048`。

### 3.2 生效范围

- 主循环 `run_agent`：使用配置值 + 覆盖项；
- Plan 协议（`submit_plan` / `revise_plan`）：planning 比对话略保守，可在配置上 `plan.max_output_tokens` 覆盖（默认取 `provider.max_output_tokens`）；
- `memory_suggestion`：默认 `max_output_tokens=2048`（比对话小），仍可用全局配置覆盖。

## 4. 兼容与回退

- 旧 `deepseek-chat`/`deepseek-reasoner` 行为：`reasoning=false` 配置下完全等价现状，不做任何请求体变化；
- 思考闭环失败（模型不支持 `reasoning_content`、流中断）→ 把该次请求标记为 `thinking_failed`，自动降级为 `Disabled` 并继续下一次调用，不中断 Agent；
- schema 升级采用**追加字段**策略，任何 v5 之前旧会话可无缝读取。

## 5. 测试与验收

- 配置：`temperature/max_output_tokens/reasoning` 白名单、范围校验、默认值、CLI 覆盖优先级；
- wire：openai-compatible 请求/响应/SSE 解析（本地 mock HTTP），工具调用聚合；
- reasoning：DeepSeek mock 返回 `reasoning_content`，`ProviderResponse.reasoning` 有值、session 持久化、`provider_messages` 回传、公开 AssistantDelta 不包含思考文本；
- 兼容：`reasoning=false` 时 V4 请求仍发送 `thinking.disabled`；
- 迁移：v5 会话加载后 schema 版本为 6，无数据损坏。