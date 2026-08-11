# XDUDU 从零学习与完整面试手册

> 适用基线：`main@671d983`。本手册以当前源码为事实来源；源码映射使用文件和稳定符号，不绑定行号。

## 先看这里：这份文档里有三种完全不同的知识

如果你刚开始学习，不要直接从 150 道题往下背。XDUDU 同时使用了 Rust、Agent 原理和软件工程设计，名字经常出现在同一段代码里，但它们不是一回事。

| 标记 | 它是什么 | 典型内容 | 你要回答的问题 |
|---|---|---|---|
| **【Rust】** | 编程语言知识 | 所有权、借用、trait、Result、Arc、async | 这段 Rust 语法为什么能编译、数据由谁管理？ |
| **【Agent】** | AI Agent 原理 | Tool Calling、ReAct、上下文、Memory、Plan | 模型怎样决定行动，又怎样观察真实结果？ |
| **【XDUDU 工程】** | 本项目自己的实现 | ToolRegistry、SessionStore、ApprovalGate | XDUDU 怎样把 Agent 原理做成安全程序？ |
| **【通用工程】** | 不只属于 Rust/Agent | SQLite、事务、SSRF、CI、状态机 | 系统怎样在失败、并发和攻击下保持可靠？ |

例如：

```text
trait                  是 Rust 知识
Tool trait             是 XDUDU 使用 Rust 定义的工具接口
Tool Calling           是 Agent/模型协议
ToolRegistry           是 XDUDU 连接 Tool Calling 与真实工具的工程组件
Permission/Approval    是 XDUDU 的安全设计
```

所以不要把 `ToolRegistry` 当成 Rust 官方功能，也不要把 `trait` 当成 Agent 专用技术。正确关系是：

```text
Agent 原理提出需求：模型需要调用工具
             │
             ▼
XDUDU 工程设计：所有工具必须经过 ToolRegistry
             │
             ▼
Rust 提供实现手段：trait + HashMap + Arc + async
```

## 第零部分：先把项目里的关键词翻译成人话

这一部分不要求你看源码。目标是先知道每个词在系统里扮演什么角色。以后遇到陌生名词，先问四件事：

1. 它是一个人、数据、动作，还是规则？
2. 它位于模型侧、XDUDU 程序内部，还是操作系统侧？
3. 谁创建它，谁调用它？
4. 它失败以后，错误交给谁处理？

### 0.1 一张“公司岗位表”理解全部组件

可以把 XDUDU 想成一家接受自然语言委托的软件公司：

| XDUDU 名词 | 公司类比 | 实际职责 | 它不负责什么 |
|---|---|---|---|
| User | 客户 | 提出目标、确认审批 | 不需要提供每个实现步骤 |
| Model / LLM | 会分析问题的工程师 | 阅读上下文，生成回复或提出工具调用 | 不能直接读硬盘、运行命令 |
| Agent Runtime | 项目经理 | 反复协调模型和工具，判断任务是否结束 | 不实现具体模型 HTTP 协议 |
| Provider | 翻译员/供应商适配器 | 把 XDUDU 统一请求翻译成 DeepSeek 等厂商协议 | 不批准文件写入 |
| Tool | 具体执行人员 | 读取文件、搜索、运行 Git 或命令 | 不决定整个任务下一步 |
| ToolRegistry | 工具调度与安保中心 | 查找工具，并统一做权限、校验、审批、超时 | 不负责生成自然语言答案 |
| Session | 项目档案 | 保存消息、工具调用和运行状态 | 不是模型的上下文窗口本身 |
| SessionStore | 档案室 | 把 Session 存入 SQLite 并恢复 | 不决定模型该回答什么 |
| EventSink | 广播接口 | 接收状态、文本、工具进度等事件 | 不直接执行工具 |
| TUI | 用户看到的工作台 | 把事件渲染成终端界面并接收键盘输入 | 不应绕过 Registry 修改文件 |
| Plan | 经审批的施工方案 | 保存步骤、依赖、完成条件和状态 | 批准 Plan 不等于批准所有副作用 |
| MCP | 外部能力接入标准 | 让其他进程或服务向 XDUDU 暴露工具 | 不是另一个聊天模型 |

把它们连起来：

```text
用户
  │ 自然语言
  ▼
Agent Runtime ──统一请求──► Provider ──厂商请求──► DeepSeek API
  ▲                                      │
  │              文本或 ToolCall         │
  └──────────────────────────────────────┘
  │
  ├─ ToolCall ─► ToolRegistry ─► Tool ─► 文件/进程/网络
  │                  │
  │                  └─ 权限、审批、超时、审计
  │
  ├─ 状态写入 ─► SessionStore ─► SQLite
  │
  └─ 事件发送 ─► EventSink ─► TUI
```

### 0.2 Model、LLM、API 和 Provider 到底有什么区别

这四个词最容易混在一起。

#### Model / LLM

模型是负责“根据输入生成输出”的算法服务，例如 DeepSeek 某个具体模型。它本质上只看见请求中的文本、消息和工具定义。它看不见你的终端，也不能自己打开 `README.md`。

#### API

API 是调用模型服务时遵守的网络协议。它规定：

- 请求发到哪个 URL；
- HTTP Header 如何携带密钥；
- JSON 用哪些字段；
- 流式响应如何分段；
- Tool Call 如何编码；
- 错误和限流如何表达。

DeepSeek 和 Anthropic 的字段并不完全相同。即使两个模型都能回答问题，它们的 API 也可能不同。

#### Provider

Provider 是 XDUDU 内部的“厂商协议适配层”。它让 Agent 不必理解每一家 API。

假设 Agent 构造统一数据：

```rust
ProviderRequest {
    model: "deepseek-chat",
    messages: [...],
    tools: [...],
}
```

DeepSeek Provider 会把它翻译成 DeepSeek 接受的 HTTP JSON；Anthropic Provider 会翻译成 Anthropic Messages API。响应回来后，Provider 再把厂商字段翻译回统一的：

```rust
ProviderResponse {
    message,
    tool_calls,
    finish_reason,
    usage,
}
```

因此可以记住：

```text
模型       = 真正生成内容的能力
API        = 访问该能力的网络合同
Provider   = XDUDU 对不同 API 的翻译器
Agent      = 使用翻译器完成任务的控制循环
```

#### 为什么不在 Agent 里直接写 DeepSeek HTTP 请求

如果直接写，Agent 主循环会同时承担：

- 任务状态机；
- DeepSeek 鉴权；
- SSE 解码；
- Anthropic 字段差异；
- 重试和限流；
- 工具参数拼接。

这样更换模型时就必须修改 Agent 主循环。现在 Agent 只依赖 `Provider trait`，所以测试可以注入 `MockProvider`，未来增加兼容 Provider 也主要改适配层。

#### Provider 一次调用的实际数据流

```text
agent.rs
  │ 构造 ProviderRequest
  ▼
dyn Provider
  │ 根据运行时真实类型调用 DeepSeekProvider
  ▼
reqwest::Client
  │ HTTPS POST + Authorization + JSON
  ▼
DeepSeek
  │ SSE: data: {...}
  ▼
Provider 流解析器
  │ 拼接文本 delta / 工具参数 delta
  ▼
统一 ProviderResponse
  │
  └─ 返回 agent.rs
```

对应源码：

- `crates/xdudu-core/src/provider/mod.rs::Provider`：统一接口和请求/响应类型；
- `crates/xdudu-core/src/provider/deepseek.rs`：DeepSeek 协议转换；
- `crates/xdudu-core/src/provider/anthropic.rs`：Anthropic 协议转换；
- `crates/xdudu-core/src/provider/stream.rs`：流式事件接口；
- `crates/xdudu-core/src/provider/retry.rs`：可重试错误和退避；
- `crates/xdudu-core/src/provider/factory.rs`：根据配置创建具体 Provider。

### 0.3 Agent 和 Agent Runtime 是什么

普通聊天程序通常只做一次：

```text
用户问题 → 调用模型 → 展示答案
```

Agent Runtime 需要维持循环：

```text
用户目标
  ↓
调用模型
  ↓
模型是否要求工具？
  ├─ 是：执行工具 → 把真实结果放回上下文 → 再调用模型
  └─ 否：检查失败和完成条件 → 结束或标记未完成
```

“Runtime”强调它不是一段 Prompt，而是一套正在运行的控制程序。它负责：

- 保存当前轮次；
- 保证工具结果回到正确 call ID；
- 区分 Planning、Acting、Observing、Reflecting；
- 处理取消和最大轮次；
- 记录未解决的工具错误；
- 压缩过长上下文；
- 把过程持久化；
- 只在满足终态条件后报告完成。

XDUDU 的核心入口是 `crates/xdudu-core/src/agent.rs::run_agent`。

### 0.4 Prompt、Message、Context 和 Token

#### Prompt

Prompt 是发给模型的指令文本。System Prompt 定义身份和规则，User Prompt 表示用户本轮目标。Prompt 是软约束：模型可能理解错误，所以安全限制还必须由代码执行。

#### Message

Message 是有角色的会话单元：

```text
system     系统规则
user       用户要求
assistant  模型回复或工具调用
tool       工具真实结果
```

Tool Call 和 Tool Result 必须通过 call ID 对应，否则模型无法知道结果属于哪一次调用。

#### Context

Context 是“本次真正发送给模型的全部可见信息”，通常包含系统 Prompt、近期消息、摘要、记忆和工具定义。SQLite 中可以有上千条消息，但不代表每轮都全部发送。

#### Token

Token 是模型处理文本时的计量单位，不完全等于一个汉字或一个英文单词。上下文窗口有 Token 上限，因此 XDUDU 必须选择近期消息并压缩早期内容。

### 0.5 Tool Calling、Tool 和 ToolResult

Tool Calling 是模型表达“我希望程序执行某个能力”的结构化协议，而不是模型直接运行函数。

```json
{
  "id": "call_123",
  "name": "file_read",
  "arguments": {"path": "README.md"}
}
```

XDUDU 找到名为 `file_read` 的 Tool，执行后得到：

```json
{
  "callId": "call_123",
  "ok": true,
  "output": {"content": "# XDUDU ..."}
}
```

模型下一轮看到这个结果，才可能基于真实文件内容继续工作。这里的关键不是 JSON 长什么样，而是：

1. 模型只能提出意图；
2. 程序决定是否允许；
3. 结果必须反馈给模型；
4. 失败也必须反馈，不能伪装成功。

### 0.6 ToolRegistry、Schema、validate 和 preflight

ToolRegistry 可以拆成两个词：

- Registry：注册表，保存“工具名 → 工具对象”的映射；
- Tool executor：统一执行入口，所有工具必须经过相同策略链。

Schema 是工具输入的说明书。例如 `file_read` 要求 `path` 是字符串。Schema 会发给模型，帮助模型生成正确参数。

但模型输出不能被信任，所以程序还要 `validate`：

```text
Schema      告诉模型应该怎样写
validate    在执行端阻止错误输入
```

`preflight` 是正式产生副作用前的试运行。例如补丁工具先在内存解析所有 hunk、校验所有路径并计算结果；如果补丁本来就不成立，就不弹出审批。

完整链路：

```text
查找工具
 → 权限上限
 → 输入校验
 → 取消检查
 → 无副作用预检
 → 用户审批
 → 超时包裹
 → 真正执行
 → 脱敏、审计、返回结果
```

### 0.7 Permission、Approval 和 Side Effect

- Permission：本次运行允许达到的能力上限；
- Approval：用户是否授权这一次具体操作；
- Side Effect：操作会不会改变外部世界。

例如 `file_read` 通常只读，`file_write` 会改变工作区，`terminal_exec` 会启动进程，`web_fetch` 会访问网络。

```text
read-only 模式 + 用户点击允许写文件
              ↓
仍然拒绝，因为 Approval 不能突破 Permission 上限
```

这种分层避免一次误点击把整个程序提升为无限权限。

### 0.8 Session、SessionStore、SQLite 和 Checkpoint

Session 是一次对话/任务的领域数据；SessionStore 是保存和读取这些数据的接口；SQLite 是当前具体存储实现。

Checkpoint 是执行过程中的稳定保存点。例如工具调用前先保存 `Pending`：

```text
写入 Pending
  ↓
执行工具
  ↓
写入 Succeeded / Failed
```

如果程序在中间崩溃，重启后看见 Pending，只能判断“结果未知”。对文件写入、网络请求等副作用，不能自动重放，否则可能执行两次。

### 0.9 Event、EventSink 和 TUI

Agent 核心不直接打印终端，而是发语义事件：

```text
AssistantDelta
ToolStarted
ToolProgress
ToolFinished
PlanPaused
```

EventSink 是接收事件的接口。TUI、JSON Lines 和测试记录器可以用不同方式消费同一个事件。这就是为什么核心逻辑不依赖彩色终端。

### 0.10 async、Tokio、SSE 和“流式输出”

- `async/.await`：Rust 描述异步工作的语法；
- Tokio：负责调度异步任务的运行时；
- Reqwest：发送 HTTP 请求的客户端库；
- SSE：服务器持续推送文本事件的一种 HTTP 流格式。

模型可能这样返回：

```text
data: {"delta":"你"}
data: {"delta":"好"}
data: {"finish_reason":"stop"}
```

Provider 必须逐段解析并将文本 delta 发给 UI，同时在内部聚合完整回复。工具参数也可能被拆成多段，但参数没拼完整前绝不能执行。

### 0.11 Trait、依赖注入和“解耦”

Trait 是 Rust 定义能力接口的语言机制。依赖注入是一种组装方式：对象需要什么，由外部创建后传入，而不是对象内部写死。

```rust
pub struct AgentRunConfig<'a> {
    pub provider: &'a dyn Provider,
    pub tool_registry: &'a ToolRegistry,
    pub session_store: &'a dyn SessionStore,
}
```

这表示 Agent 只知道“有一个 Provider 可以调用”，不知道它是 DeepSeek 还是假实现。所谓解耦，就是修改其中一个组件时，其他组件尽量不需要修改。

依赖注入不一定要 Spring 或专门框架。XDUDU 由 CLI 的 Runtime 装配代码手动创建对象并传入。

### 0.12 Transaction、Hash、Atomic Write 和 Undo

- Transaction：多个修改整体成功或整体失败；
- Hash：文件内容的数字指纹，内容改变后 SHA-256 基本会改变；
- Atomic Write：先写临时文件，再用原子替换切换成新版本；
- Undo：依据账本恢复到修改前镜像。

这四个概念解决不同问题：

```text
Hash          防止覆盖用户刚刚修改的新内容
Transaction   防止多文件只改成功一半
Atomic Write  防止单文件只写入半截
Undo          允许成功修改之后安全恢复
```

### 0.13 Plan、Step、DAG、Revision 和 Evidence

Plan 是可持久化的结构化计划；Step 是其中一个步骤；DAG 是“有方向且无环”的依赖图。

```text
读取架构 ─┐
          ├─► 设计修改 ─► 编码 ─► 测试
读取测试 ─┘
```

- revision：计划内容修改了第几版；
- execution_version：执行状态更新了第几次；
- evidence：证明完成条件确实满足的证据；
- attempt：某一步第几次尝试。

Plan 审批只表示认可施工方案。执行时的文件写入和命令仍要走 ToolRegistry。

### 0.14 MCP、Skill 和 Plugin

- MCP 是外部工具通信协议；
- Skill 是给模型看的工作流程知识；
- Plugin 是 XDUDU 中声明一组 MCP Server 的配置载体；
- Tool 是最终可以被 Agent 结构化调用的能力。

例如 Python 编写一个静态扫描服务，通过 MCP 暴露 `semgrep_scan`。XDUDU 把它包装成普通 Tool，仍然执行权限、审批、超时和脱敏。Skill 可以告诉模型“做 code review 时先调用 semgrep_scan，再解释结果”，但 Skill 自己不执行扫描。

### 0.15 一页术语速查表

| 术语 | 一句话解释 | 所属知识 |
|---|---|---|
| LLM / Model | 根据上下文生成内容的模型 | AI |
| Provider | 模型厂商 API 适配器 | XDUDU 架构 |
| Agent Runtime | 驱动模型和工具循环的程序 | Agent 工程 |
| ReAct | 行动后观察结果再继续判断的循环 | Agent |
| Tool Calling | 模型输出结构化工具意图的协议 | Agent |
| ToolRegistry | 工具目录和统一安全执行入口 | XDUDU 工程 |
| Schema | 工具参数的数据结构说明 | 数据协议 |
| Session | 一次任务的消息与状态档案 | Agent 工程 |
| Context | 某一轮真正发送给模型的信息 | Agent |
| Memory | 跨会话保留的稳定信息 | Agent |
| SSE | HTTP 流式事件格式 | 网络 |
| Tokio | Rust 异步运行时 | Rust |
| Reqwest | Rust HTTP 客户端 | Rust |
| Crossterm | 跨平台终端控制库 | Rust/TUI |
| Trait | Rust 的能力接口 | Rust |
| 依赖注入 | 从外部把依赖传给组件 | 架构 |
| SQLite | 嵌入式关系数据库 | 存储 |
| Transaction | 一组操作整体提交或回滚 | 可靠性 |
| SHA-256 | 内容指纹算法 | 完整性 |
| DAG | 有方向、无环的依赖图 | 算法/编排 |
| MCP | 外部服务提供 Agent 工具的协议 | 扩展 |
| SSRF | 服务端被诱导访问危险地址的攻击 | 网络安全 |

## 第一部分：从零理解 Rust、Agent 和 XDUDU

这一部分是教学内容。读完以后，再进入后面的面试题。每个知识点都明确标注属于哪一层。

### 1. 先建立完整心智模型【Agent + XDUDU 工程】

用户在终端输入：

```text
分析项目并修改 README，最后运行测试
```

这句话不会直接变成 Shell 命令。完整过程是：

```text
用户输入
   │
   ▼
CLI 创建/恢复 Session
   │
   ▼
Agent 构建 ProviderRequest
   │  系统 Prompt、历史消息、工具定义、记忆
   ▼
模型返回文本或 ToolCall
   │
   ├─ 文本 ───────────────► 继续检查是否真正完成
   │
   └─ ToolCall
          │
          ▼
      ToolRegistry
          │  权限、校验、预检、审批、超时
          ▼
      真实工具执行
          │
          ▼
      ToolResult 写回 Session
          │
          └───────────────► 再次请求模型
```

这里至少包含四类角色：

- **模型**：根据上下文选择下一步，但不能直接操作电脑。
- **Agent Loop**：反复调用模型和工具，维护运行状态。
- **ToolRegistry**：决定某个工具调用是否允许、是否合法、如何执行。
- **SessionStore**：保存消息、工具调用和状态，让崩溃后仍能解释现场。

### 2. 什么是“值”和“变量”【Rust】

先看最简单的 Rust：

```rust
fn main() {
    let count = 3;
    let name = String::from("XDUDU");
    println!("{name}: {count}");
}
```

逐行理解：

- `fn main()`：程序入口函数。
- `let count = 3`：创建变量 `count`，保存整数值。
- `String::from(...)`：在堆上创建可以增长的字符串。
- `println!`：宏，编译器会展开它生成输出代码。

Rust 变量默认不可变：

```rust
let count = 3;
// count = 4; // 编译错误

let mut editable = 3;
editable = 4; // 合法
```

这首先是 **Rust 语言知识**，跟 Agent 没有直接关系。XDUDU 只是用这门语言编写。

### 3. 栈、堆与所有权【Rust】

整数大小固定，通常可以直接复制：

```rust
let a = 10;
let b = a;
println!("{a} {b}");
```

`String` 包含堆内存指针、长度和容量。如果赋值时只复制指针，两个变量离开作用域都会释放同一内存，产生 double free。因此 Rust 默认转移所有权：

```rust
let a = String::from("hello");
let b = a;

// println!("{a}"); // 编译错误：a 的所有权已移动给 b
println!("{b}");
```

可以把所有权理解为“谁负责最终清理这个值”。规则是：

1. 每个值都有一个所有者；
2. 同一时刻只有一个所有者；
3. 所有者离开作用域时，值被释放。

这能帮助 XDUDU 管理 HTTP Client、MCP 子进程和终端状态，但它不会自动撤销已经写入磁盘的文件。文件事务仍属于 **通用工程知识**。

### 4. 借用：使用数据，但不拿走所有权【Rust】

下面的函数拿走 `String`：

```rust
fn print_name(name: String) {
    println!("{name}");
}

let name = String::from("XDUDU");
print_name(name);
// println!("{name}"); // 所有权已经移动
```

如果函数只想临时读取，应传引用：

```rust
fn print_name(name: &String) {
    println!("{name}");
}

let name = String::from("XDUDU");
print_name(&name);
println!("{name}"); // 仍然可用
```

`&String` 是不可变借用。可变借用写成 `&mut String`：

```rust
fn append_agent(name: &mut String) {
    name.push_str(" Agent");
}

let mut name = String::from("XDUDU");
append_agent(&mut name);
```

核心规则：可以同时有多个不可变借用，或者只有一个可变借用；二者不能在同一有效范围重叠。它在编译期阻止“一个任务读取时另一个任务随意修改同一内存”。

### 5. 生命周期不是运行时间【Rust】

生命周期描述“引用至少要有效多久”，由编译器检查：

```rust
pub struct AgentRunConfig<'a> {
    pub provider: &'a dyn Provider,
    pub tools: &'a ToolRegistry,
    pub event_sink: &'a dyn EventSink,
}
```

逐项解释：

- `'a` 是生命周期参数，不是一个数值；
- `&'a dyn Provider` 表示借用一个 Provider；
- `AgentRunConfig` 不能比它借用的 Provider、Registry 和 Sink 活得更久；
- 因此 Agent 不会持有已经被释放的依赖。

这是 **Rust 实现手段**。Agent 原理只要求“运行时能访问模型和工具”，并不规定必须用生命周期。

### 6. struct：把相关数据组成一个对象【Rust】

```rust
struct ToolCall {
    id: String,
    name: String,
    arguments: serde_json::Value,
}
```

`struct` 把一组字段组成一个明确类型。XDUDU 中常见例子：

- `ProviderRequest`：一次模型请求所需信息；
- `ToolContext`：一次工具执行的 Session、工作区、取消令牌；
- `Session`：一次会话的持久化状态；
- `Plan`：可审批、可恢复的长期计划。

“struct 是什么”属于 Rust；“ToolCall 应包含哪些字段”属于 Agent 协议和 XDUDU 工程设计。

### 7. enum：有限状态的集合【Rust + 通用工程】

```rust
enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Error,
}
```

相比随意字符串，enum 让编译器检查是否处理了全部情况：

```rust
match reason {
    FinishReason::Stop => { /* 检查能否完成 */ }
    FinishReason::ToolCalls => { /* 执行工具 */ }
    FinishReason::Length => { /* 标记未完成 */ }
    FinishReason::Error => { /* 处理错误 */ }
}
```

`enum` 是 Rust；把 Agent 运行设计为 Planning、Acting、Observing、Reflecting 状态机是通用工程与 Agent 设计。

### 8. trait：定义“能做什么”【Rust】

`trait` 类似接口：

```rust
trait Speak {
    fn speak(&self) -> String;
}

struct Dog;

impl Speak for Dog {
    fn speak(&self) -> String {
        "汪".to_owned()
    }
}
```

XDUDU 的 Provider：

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn chat(&self, request: ProviderRequest)
        -> XduduResult<ProviderResponse>;
}
```

这里要分清：

- `trait`、`impl`、`&self` 是 Rust；
- `Provider` 是 XDUDU 定义的接口；
- DeepSeek/Anthropic/OpenAI-compatible 是不同实现；
- “把厂商协议统一起来”是架构设计。

### 9. `dyn Trait` 和动态分派【Rust】

运行前无法确定用户会选择哪个模型，所以 Agent 使用 trait object：

```rust
let provider: Box<dyn Provider> = factory.create(config, secret)?;
```

`dyn Provider` 表示“任何实现 Provider 的具体类型”。运行时通过虚函数表找到真实方法。网络请求的耗时远大于这次间接调用，因此这里优先获得可替换性和测试能力。

### 10. Result 和 `?`【Rust + 通用工程】

Rust 不用异常表示普通失败：

```rust
fn parse_port(text: &str) -> Result<u16, String> {
    text.parse::<u16>()
        .map_err(|_| "端口格式错误".to_owned())
}
```

`?` 的意思是：成功就取出值；失败就提前把错误返回给调用者。

```rust
fn load() -> Result<Config, XduduError> {
    let text = std::fs::read_to_string("config.toml")?;
    parse_config(&text)
}
```

XDUDU 再把错误分成：

- `XduduError`：core/CLI 级错误；
- `ToolResult.error.code`：模型和 JSON 能识别的工具错误；
- 退出码：Shell 脚本能识别的最终状态。

### 11. Arc、Send 和 Sync【Rust 并发】

`Arc<T>` 是线程安全的引用计数智能指针：

```rust
let tool: Arc<dyn Tool> = Arc::new(FileReadTool);
let another = Arc::clone(&tool);
```

它没有复制 `FileReadTool` 的真实资源，只增加共享所有者计数。

- `Send`：值可以安全移动到另一线程；
- `Sync`：`&T` 可以安全被多线程共享；
- `Arc`：解决共享所有权；
- `Mutex`：解决共享可变状态。

它们只能防内存数据竞争，不能证明两个文件写入在业务上不冲突。因此 XDUDU 仍规定只读工具可并行、副作用工具有序执行。

### 12. async、Future 和 Tokio【Rust 异步】

`async fn` 调用后不会立刻把所有工作做完，而是返回一个 `Future`：

```rust
async fn fetch() -> Result<String, XduduError> {
    // 等待网络期间，执行器可以运行其他任务
    Ok("result".to_owned())
}
```

`.await` 表示当前任务在这里等待结果，但线程可以去轮询其他 Future。

```rust
let response = provider.chat(request).await?;
```

异步不等于自动并行。只有同时保存并轮询多个 Future，或者 `spawn` 新任务，才形成并发。

### 13. `tokio::select!` 和取消【Rust 异步 + XDUDU 工程】

XDUDU 经常需要同时等待“操作完成”和“用户取消”：

```rust
tokio::select! {
    result = provider.chat(request) => result,
    _ = cancellation.cancelled() => {
        return Err(XduduError::interrupted("用户已取消"));
    }
}
```

`CancellationToken` 是协作式取消：调用 `cancel()` 只改变共享状态，执行中的代码必须在 `select!` 或循环里主动检查。这样能先保存 Interrupted/Paused 状态，再安全退出。

### 14. 什么是 LLM、Provider 和消息【Agent】

LLM 只接收输入并生成输出。Provider 是 XDUDU 对模型厂商协议的统一抽象：

```text
XDUDU ProviderRequest
          │
          ├─► DeepSeek JSON/SSE
          ├─► Anthropic Messages/SSE
          └─► OpenAI-compatible JSON/SSE
```

一次请求通常包含：

- system：系统规则；
- user：用户消息；
- assistant：模型之前的回复和 ToolCall；
- tool：真实工具结果；
- tools：当前可用工具及 JSON Schema。

Provider 只负责协议翻译，不负责决定文件能不能写。权限由 ToolRegistry 负责。

### 15. 什么是 Tool Calling【Agent】

模型不会直接调用 Rust 函数。它返回结构化意图：

```json
{
  "id": "call_123",
  "name": "file_read",
  "arguments": {
    "path": "README.md"
  }
}
```

Agent 收到后：

1. 保存这个调用；
2. 交给 ToolRegistry；
3. 执行真实工具；
4. 得到 `ToolResult`；
5. 以相同 call ID 交回模型。

Tool Calling 是 Agent/模型协议；`Tool` trait 是 Rust 接口；`ToolRegistry` 是 XDUDU 工程组件。

### 16. 什么是 ReAct【Agent】

ReAct 可理解为 Reason + Act + Observe 的循环，但 XDUDU 不向用户输出原始思维链：

```text
理解目标（Planning）
    ↓
调用工具（Acting）
    ↓
接收真实结果（Observing）
    ↓
根据结果再次判断（Reflecting）
    ├─► 继续调用工具
    └─► 检查完成条件
```

重要的是：模型每次都要根据真实结果继续判断。工具失败后不能假设成功，`FinishReason::Stop` 也不能单独证明任务完成。

### 17. ToolRegistry 到底是什么【Agent + XDUDU 工程 + Rust】

ToolRegistry 不是 Rust 标准库，也不是所有 Agent 必须使用的固定类。它是 XDUDU 的“工具目录 + 安全执行总管”。

核心结构：

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    approval_gate: Arc<dyn ApprovalGate>,
    change_ledger: Arc<dyn ChangeLedger>,
    command_rules: CommandRules,
}
```

逐字段解释：

- `HashMap<String, ...>`：用工具名找到工具；这是 Rust 集合。
- `Arc<dyn Tool>`：保存任意 Tool 实现并允许共享；这是 Rust。
- `ApprovalGate`：收集和判断审批；这是 XDUDU 安全设计。
- `ChangeLedger`：记录文件事务；这是 XDUDU 可靠性设计。
- `CommandRules`：控制 terminal_exec 前缀；这是 XDUDU 安全设计。

注册工具：

```rust
registry.register(FileReadTool)?;
registry.register(FileWriteTool)?;
registry.register(ApplyPatchTool)?;
```

执行工具时固定经过：

```text
查找 → Permission → validate → cancellation → preflight
    → ApprovalGate → timeout(execute) → ToolResult
```

如果没有 ToolRegistry，每个工具都可能自己忘记做审批、超时或路径检查，MCP 还可能形成第二套越权入口。

### 18. Tool trait 每个方法做什么【Rust + XDUDU 工程】

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn validate(&self, input: &Value) -> Result<(), Vec<String>>;
    async fn preflight(&self, input: &Value, context: &ToolContext)
        -> Option<ToolResult>;
    async fn needs_approval(&self, input: &Value, context: &ToolContext)
        -> bool;
    async fn execute(&self, input: Value, context: ToolContext)
        -> ToolResult;
}
```

- `definition`：告诉模型名称、描述、Schema，也告诉系统权限和副作用。
- `validate`：不相信模型 JSON，检查类型、范围和未知字段。
- `preflight`：执行无副作用预检，例如在内存应用补丁。
- `needs_approval`：根据本次输入决定是否审批。
- `execute`：真正访问文件、进程或网络。

`trait` 是 Rust；为什么要拆成这五步属于 XDUDU 工程设计。

### 19. Permission 和 Approval 不一样【XDUDU 工程】

```text
PermissionMode：这次运行最多允许什么
Approval：用户是否批准这一项具体副作用
```

例如当前模式为 read-only：

```text
file_write 需要 WriteFiles
        ↓
Permission 直接拒绝
        ↓
不会出现“是否批准”窗口
```

用户审批不能把 read-only 升级为 full-access。这样“程序能力上限”和“具体授权”分开，减少一次点击获得无限权限的风险。

### 20. 为什么 validate 之后才审批【XDUDU 工程】

模型返回的 JSON 不是可信数据。先 validate 可以拒绝：

- 缺少必填字段；
- 类型错误；
- 长度超限；
- 未知字段；
- 非法枚举值。

`apply_patch` 还会在 preflight 中完整解析补丁。如果预检失败，不应该让用户批准一个根本无法执行的操作。

### 21. 什么是 Session【Agent + 通用工程】

Session 不只是聊天记录。它还保存：

```text
用户/助手消息
工具调用 ID、输入、结果和状态
模型与工作区
Agent 终态
上下文摘要
Plan 关联
用量和时间
```

工具调用先写 `Pending` 再执行。如果进程崩溃，系统知道“这个调用可能已经发生，但结果未知”，因此不会自动重放副作用。

### 22. 为什么使用 SQLite【通用工程】

JSON 文件适合最小原型，但多条记录的原子更新、查询、迁移和并发很困难。SQLite 提供：

- transaction：一组更新一起成功或回滚；
- index：按时间、状态快速查询；
- WAL：改善读写并发；
- migration：从旧 Schema 升级；
- FTS5：全文检索记忆。

SQLite 不是 Agent 理论，也不是 Rust 特有；它是本地应用的存储方案。

### 23. 什么是文件事务【通用工程 + XDUDU 工程】

多文件补丁不能写一个算一个。XDUDU 的思路是：

```text
读取全部前镜像
    ↓
内存应用全部补丁
    ↓
再次检查哈希
    ↓
写 Prepared 账本
    ↓
逐项原子替换
    ↓
全部成功标 Applied
```

任何一步失败就根据前镜像回滚全部文件。Rust 所有权只负责内存对象，不会替你实现这个事务。

### 24. expectedSha256 为什么重要【通用工程】

Agent 读取文件到写回之间，用户可能刚好修改它：

```text
Agent 读取版本 A
用户改成版本 B
Agent 按版本 A 生成修改
```

写入前重新计算哈希，如果当前不是 A，就返回 `HASH_MISMATCH`，要求重新读取。它是文件级乐观并发控制，避免覆盖用户修改。

### 25. EventSink 与 TUI【XDUDU 工程】

Agent 不直接 `println!`，而是发事件：

```rust
AgentEvent::StateChanged { ... }
AgentEvent::AssistantDelta { ... }
AgentEvent::ToolStarted { ... }
AgentEvent::ToolProgress { ... }
AgentEvent::ToolFinished { ... }
```

不同消费者可以产生不同输出：

```text
TUI Renderer       → 彩色终端和活动区
Console Renderer   → 普通顺序文本
JSON Renderer      → 每行一个机器事件
Test EventSink     → 保存 Vec 供断言
```

`enum` 和 trait 是 Rust；事件驱动解耦是通用架构；具体 `AgentEvent` 是 XDUDU 设计。

### 26. Context、Summary 和 Memory 的区别【Agent】

| 概念 | 生命周期 | 内容 |
|---|---|---|
| 最近消息 | 当前会话短期 | 原始用户、助手和工具消息 |
| Context Summary | 当前会话中期 | 被压缩历史的结构化摘要 |
| Long-term Memory | 跨会话长期 | 稳定偏好和可复用项目事实 |

上下文窗口不是数据库容量。SQLite 可以保存全部消息，但每次发给模型只能选择预算内内容。

XDUDU 长期记忆分两阶段：

```text
会话结束自动提炼 → SQLite 原始记忆记录
                        ↓
                   合并、去重、压缩
                        ↓
                 MEMORY.md（用户可编辑）
```

### 27. Plan 和普通任务列表的区别【Agent + XDUDU 工程】

模型回复中的 Markdown 列表只是文本。XDUDU Plan 是数据库中的领域对象：

- 有稳定 Plan ID；
- 有 revision；
- 有依赖 DAG；
- 有完成条件；
- 有审批历史；
- 有 execution_version；
- 有 attempt、证据和失败原因；
- 可以 Paused、retry、cancel 和跨进程恢复。

Plan 审批表示“用户认可方案”，不表示“所有文件写入自动批准”。

### 28. 子代理和任务图【Agent】

子代理是一个隔离的小型 Agent：它有自己的 Profile、工具范围、权限上限和轮次限制，最后只返回有界报告。

任务图解决依赖和并发：

```text
探索配置 ─┐
          ├─► 汇总架构 ─► 审阅报告
探索工具 ─┘
```

Kahn 算法负责检查环；`FuturesUnordered` 负责收集动态并发任务；这些是算法/Rust 实现。为什么只读节点并行、副作用节点独占属于 XDUDU 安全设计。

### 29. MCP、Skill 和 Tool 不一样【Agent + 扩展工程】

| 名称 | 本质 | 是否直接执行能力 |
|---|---|---|
| Tool | 模型可调用的结构化能力 | 是，但必须经过 Registry |
| Skill | 按需加载的工作流知识 | 否，最终仍调用 Tool |
| MCP | 外部进程/服务提供工具的协议 | 通过适配成 Tool 执行 |
| Plugin | 声明和组合 MCP Server | 不加载进程内动态代码 |

Python 能力更适合作为独立 MCP Server 接入，不需要重写 Rust 核心。

### 30. 一次真实任务如何穿过全部代码【综合】

以“修改 README 并运行测试”为例：

```text
1. main.rs 接收用户输入
2. SessionStore 保存用户消息
3. agent.rs 构造 ProviderRequest
4. Provider 返回 file_read ToolCall
5. Agent 先保存 Pending 调用
6. ToolRegistry 查找 FileReadTool
7. Permission/validate/preflight/approval
8. FileReadTool 返回文件内容
9. ToolResult 写入 Session 并回传模型
10. 模型生成 apply_patch ToolCall
11. preflight 在内存验证补丁
12. 用户批准 workspace-write
13. 账本 + 临时文件 + 原子替换完成写入
14. 模型调用 terminal_exec 运行测试
15. 测试失败则 unresolved_tool_failures 保留
16. 测试成功并无待处理失败，Agent 才能 Completed
17. EventSink 把全过程展示给 TUI
```

这条链路中：

- `trait`、`Arc`、async、Result 是 Rust；
- Tool Calling、ReAct、上下文是 Agent；
- Registry、审批、事务、Session 是 XDUDU 工程；
- 哈希、SQLite、状态机是通用工程。

### 31. 推荐源码阅读顺序【学习方法】

不要从 4000 行的 `main.rs` 开始逐行读。推荐顺序：

1. `provider/mod.rs`：先认识模型消息和 ToolCall；
2. `tools/mod.rs`：理解 Tool trait 和 Registry；
3. `tools/file_read.rs`：看最简单只读工具；
4. `agent.rs`：带着工具概念读 ReAct 循环；
5. `session.rs`：理解保存了什么；
6. `sqlite_session.rs`：再看持久化；
7. `apply_patch.rs` + `changes.rs`：学习文件事务；
8. `plan.rs` + `plan_executor.rs`：学习长期任务；
9. `subagent.rs` + `subagent_graph.rs`：学习短期编排；
10. `main.rs` + `tui.rs`：最后看装配和界面。

每次只回答四个问题：

```text
这个类型保存什么？
谁创建它？
谁调用它？
失败后状态落在哪里？
```

## 第二部分：把简历上的 XDUDU 真正讲明白

这一部分不是让你背简历，而是保证简历上的每句话都能用自己的话解释。面试时如果某个词说不清，就先删掉或降级描述，不要堆名词。

### 32. 项目标题逐词解释

简历标题：

> XDUDU - Rust 本地 AI 编程 Agent

逐词解释：

- **Rust**：主体代码使用 Rust，不是 Python/LangGraph 项目；Rust 负责内存安全、类型约束、异步并发和单二进制分发。
- **本地**：程序运行在用户电脑上，文件、命令、SQLite 会话和审批都发生在本机；模型推理由远程 API 提供。
- **AI 编程 Agent**：它不只是聊天窗口。模型可以提出文件读取、代码搜索、补丁、Git、测试和网络工具调用，程序执行后把真实结果反馈给模型，直到任务完成或被中断。

#### 不能怎样说

不要说“这是我训练的大模型”。XDUDU 没有训练基础模型，它构建的是模型之上的 Agent Runtime。

不要说“本地 Agent 所以模型也在本地”。当前主要调用 DeepSeek API，“本地”描述运行时、数据与工具所在位置。

#### 30 秒项目介绍

> XDUDU 是我用 Rust 从零实现的本地终端编程 Agent。它通过 Provider 适配 DeepSeek 等模型，通过 ReAct 循环让模型提出工具调用，再由 ToolRegistry 在权限、参数校验和用户审批之后执行文件、Git、命令或网络工具。会话和计划保存到 SQLite，长任务支持上下文压缩、Plan/DAG、暂停恢复和 MCP 外部工具接入。

### 33. 技术栈不是装饰：每个库解决什么

简历写了：

> Rust + Tokio + SQLite + Reqwest + Crossterm + SSE + MCP

面试时至少要讲清下面这张表：

| 技术 | XDUDU 为什么需要 | 在项目中做什么 | 它不是 |
|---|---|---|---|
| Rust | 构建本地 CLI 核心 | 类型、所有权、trait、错误处理、编译为二进制 | Agent 框架 |
| Tokio | 网络、进程、UI 事件都要异步等待 | 调度 Future、channel、timeout、select、取消 | HTTP Client |
| SQLite | 会话和计划必须跨进程保存 | 消息、工具调用、Plan、Memory、迁移和恢复 | 向量数据库 |
| Reqwest | 调用模型和受限网页 | HTTPS、Header、JSON、流式响应 | Provider 本身 |
| Crossterm | 控制跨平台终端 | 键盘、光标、颜色、屏幕尺寸、原始模式 | Agent Runtime |
| SSE | 接收模型逐段输出 | 文本 delta、结束原因、工具参数分片 | WebSocket |
| MCP | 接入外部工具 | stdio/HTTP 握手、发现工具、调用工具 | 模型 Provider |

源码入口：

- Tokio：`agent.rs`、`main.rs`、`mcp.rs` 中的异步任务和 `select!`；
- SQLite：`sqlite_session.rs`；
- Reqwest/SSE：`provider/*`、`web_fetch.rs`；
- Crossterm：`tui.rs`、`input_editor.rs`；
- MCP：`mcp.rs`。

### 34. 第一条简历描述逐句拆解

原文：

> Agent Runtime：从零实现 ReAct 模型-工具闭环，以 Trait 与依赖注入解耦 Provider、Agent、ToolRegistry、SessionStore 和终端 UI；支持流式响应、多轮观察、取消、超时、失败反馈与上下文压缩。

#### “从零实现 Agent Runtime”

含义：没有把任务交给 LangGraph 等图框架驱动，而是在 `run_agent` 中自己维护：

```text
创建/恢复 Session
 → 构造上下文
 → 请求 Provider
 → 解析文本或工具调用
 → 执行工具
 → 保存 ToolResult
 → 再请求 Provider
 → 判断 Completed / Incomplete / Interrupted / Error
```

“从零”不表示所有依赖都自己写。HTTP 使用 Reqwest、异步使用 Tokio、数据库使用 SQLite；自己实现的是 Agent 控制逻辑和工程边界。

#### “ReAct 模型-工具闭环”

“闭环”表示工具结果会返回模型：

```text
模型：我要读取 Cargo.toml
程序：执行 file_read
程序：把真实内容作为 tool message 返回
模型：根据内容决定继续搜索还是回答
```

如果工具执行后直接把结果只显示给用户、没有放回模型上下文，就不是完整闭环。

#### “Trait 与依赖注入解耦”

Trait 定义接口，依赖注入负责把具体实现传进去：

```text
Provider trait        ← DeepSeekProvider / MockProvider
SessionStore trait    ← SqliteSessionStore / 测试 Store
EventSink trait       ← TUI / JSON / 测试事件收集器
```

Agent 调用的是接口，所以：

- 换模型不必重写 Agent Loop；
- 测试不需要真实 API；
- 换 UI 不必把文件工具复制一遍；
- 存储实现可以独立演进。

#### “流式响应”

Provider 不等整个答案生成完再返回，而是解析 SSE delta，通过事件逐步交给 TUI。最终完整消息仍要聚合并写入 Session。

#### “多轮观察”

每次工具执行结果都会形成新观察。模型可能连续经历“搜索 → 读取 → 修改 → 测试 → 修复 → 再测试”，而不是只能调用一次工具。

#### “取消”

Ctrl+C 触发 `CancellationToken`。Provider、工具和 Plan 执行器需要协作检查取消状态，并将会话或步骤保存为 Interrupted/Paused，而不是粗暴退出且丢失现场。

#### “超时”

网络、MCP 和命令可能永远不返回。Registry 或调用层使用 `tokio::time::timeout` 设置上限，超时后返回稳定错误，而不是永久卡住 Agent。

#### “失败反馈”

工具失败不是只在终端打印。错误结果要写入 Session 并返回模型，模型才有机会修正参数或选择替代方案；仍未解决时不能声称完成。

#### “上下文压缩”

SQLite 保存完整历史，但模型上下文有 Token 上限。XDUDU 保留近期消息，把较早信息压缩成结构化摘要；压缩只改变“发给模型的视图”，不删除原始会话。

#### 这一条的面试口述版本

> 我把 Agent 主循环放在 core 中。Agent 每一轮通过统一 Provider 请求模型；如果模型返回 ToolCall，就先持久化调用，再通过 ToolRegistry 执行，把 ToolResult 写回会话并进入下一轮。Provider、存储和 UI 都通过 trait 注入，所以可以用 MockProvider 离线测试。运行中还处理 SSE 增量、取消令牌、超时、未解决失败和上下文预算，避免模型一次 Stop 就被误判为任务完成。

### 35. 第二条简历描述逐句拆解

原文：

> 工具安全与事务：设计权限检查、Schema 校验、预检、用户审批、执行、脱敏和审计策略链；实现多文件事务补丁、前后镜像哈希、原子回滚与整批 Undo，防止部分写入和并发覆盖。

#### 为什么需要“策略链”

模型生成的参数可能错误，仓库内容也可能诱导模型越权。不能让每个 Tool 自己随意决定安全步骤，否则新工具容易漏掉审批或超时。

```text
Permission  检查当前模式是否允许这种能力
Schema      说明并验证参数结构
Preflight   无副作用地确认操作可行
Approval    让用户授权具体副作用
Execute     真正访问文件/进程/网络
Redaction   输出和落盘前隐藏密钥
Audit       保存调用、结果、耗时和状态
```

顺序很重要。例如无效 JSON 应在审批前拒绝，避免用户批准一个根本不能执行的操作。

#### “多文件事务补丁”

假设一个补丁同时修改三个文件。天真的实现可能前两个写成功，第三个失败，仓库进入不一致状态。XDUDU 会先读取全部前镜像，在内存应用全部 hunk，通过预检后再提交。

#### “前后镜像”

- pre-image：修改前完整内容；
- post-image：预计修改后完整内容。

账本保存镜像或恢复所需信息，因此失败时能恢复，成功后也能 Undo。

#### “哈希”

读取文件后计算 SHA-256。正式写入前再次计算当前文件哈希：

```text
当前哈希 == 读取时哈希  → 可以继续
当前哈希 != 读取时哈希  → 用户或其他进程改过，拒绝覆盖
```

这叫乐观并发控制。

#### “原子回滚”

单文件先写临时文件再替换；多文件中任一提交失败，就用前镜像恢复已经修改的文件。账本状态区分 Prepared、Applying、Applied、RolledBack 和 Conflict。

严格来说，普通文件系统很难提供数据库那样真正的跨文件原子提交。XDUDU 实现的是“预检 + 逐文件原子替换 + 失败补偿回滚 + 崩溃恢复”的事务语义。面试时应主动说明这个边界。

#### “整批 Undo”

一个 ToolCall 产生一个事务 ID。默认 Undo 撤销整个事务，不把多文件变更拆散。撤销前再次校验所有当前文件是否仍等于 post-image；任何冲突都会停止整批撤销，避免覆盖用户后续修改。

#### 这一条的面试口述版本

> 我没有直接执行模型给出的工具参数，而是在 ToolRegistry 建立固定策略链。文件修改方面，一个 apply_patch 对应一个事务：先解析所有文件和 hunk，读取前镜像并计算哈希，在内存生成后镜像；审批后先写 Prepared 账本，再用临时文件和原子替换提交。失败时按前镜像补偿回滚，Undo 前校验后镜像哈希，因此既防止半成功，也防止覆盖用户并发修改。

### 36. 第三条简历描述逐句拆解

原文：

> 可靠执行与扩展：基于 SQLite 持久化会话、工具调用及检查点；实现可审批、可暂停恢复的 Plan/DAG 执行器，并将 MCP stdio/HTTP 动态工具纳入统一安全边界。

#### “SQLite 持久化会话”

用户消息、助手消息、工具调用、状态和摘要不能只放在内存。SQLite 让程序重启后仍能 `resume`，并支持事务、索引、Schema migration 和状态查询。

#### “工具调用检查点”

工具执行前先记 Pending，执行后再记成功或失败。崩溃发生在两者之间时，系统将结果视为未知，不能自动重放可能有副作用的操作。

#### “Plan”

Plan 不是 Markdown 待办列表，而是数据库对象，包含目标、步骤、依赖、完成条件、revision、审批历史和执行状态。

#### “DAG”

步骤之间可以有依赖，但不能形成环。只有所有依赖 Completed 的步骤才 Ready。XDUDU 当前按图和原始顺序受控执行，子代理任务图可以对满足条件的只读节点进行并发。

#### “可审批”

用户先审阅整个 Plan 是否符合目标。但这只是认可方案，步骤执行中的文件、命令和网络仍分别走 ToolRegistry 审批。

#### “可暂停恢复”

Provider 错误、用户拒绝工具、Ctrl+C、证据不完整或崩溃都会保存现场并 Paused。重试会创建新的 StepAttempt，已经完成的步骤不会重新执行。

#### “MCP stdio/HTTP”

- stdio：XDUDU 启动外部进程，通过标准输入输出发送 JSON-RPC；
- HTTP：连接远程 Streamable HTTP MCP Server；
- 动态工具：启动时从 Server 查询工具列表，不是编译时写死。

MCP 工具会适配成普通 `Tool`，因此仍经过 Permission、Approval、timeout、cancellation、redaction 和 audit。MCP 不是安全沙箱，安全来自 XDUDU 外层的统一策略。

#### 这一条的面试口述版本

> 我使用 SQLite 保存 Session、ToolCall 和 Plan 状态，并在副作用前后写检查点，崩溃恢复时对结果未知的调用只标记中断而不自动重放。长任务使用结构化 Plan 和 DAG，计划内容用 revision 做审批并发控制，执行进度用 execution_version 做检查点；步骤通过完成条件和 evidence 验收。外部 MCP 工具在发现后包装成普通 Tool，复用同一套权限和审计链。

### 37. 面试官最可能继续追问什么

#### 为什么不用 LangGraph

推荐回答：

> XDUDU 的目标之一是学习和控制 Agent Runtime 的底层语义，并且核心使用 Rust。当前 ReAct、Plan 和工具安全边界都需要与本地文件事务、SQLite 恢复和 TUI 深度集成，自研可以明确终态和副作用语义。代价是开发成本和维护成本更高。如果未来业务主要是快速组合大量 Python 节点、已有 LangGraph 生态工具，框架会更合适。

#### Rust 相比 Python 的收益是什么

推荐回答：

> Rust 便于生成单二进制，类型系统能约束状态和接口，所有权有利于管理终端、子进程和网络资源，Tokio 适合统一异步事件。代价是开发速度较慢、异步 trait 和生命周期学习成本高。模型效果本身不会因为 Rust 自动变好。

#### 最难的部分是什么

可以选择你真正理解的一项回答：

1. ToolRegistry 的权限与审批边界；
2. 多文件事务和崩溃恢复；
3. Agent 终态与工具失败闭环；
4. Plan revision/execution_version 并发控制；
5. TUI 中输入、流式输出、审批和取消的统一事件循环。

回答结构固定为：

```text
问题场景 → 天真实现为什么错 → 我的状态/数据设计
        → 失败时怎样恢复 → 如何测试 → 当前边界
```

### 38. 简历关键词自测清单

在保留简历当前写法前，应能不看文档回答：

- Provider 与 Model 有什么区别？
- Agent Runtime 为什么不是一段 Prompt？
- Tool Call 为什么不能直接执行？
- Trait 和依赖注入分别解决什么？
- SSE 文本 delta 与工具参数 delta 有什么区别？
- Stop 为什么不一定表示任务完成？
- Permission 和 Approval 为什么要分开？
- Schema 与 validate 为什么都需要？
- preflight 为什么放在审批之前？
- 文件哈希怎样防并发覆盖？
- 原子替换为什么仍不能直接等于跨文件事务？
- Prepared 账本如何帮助崩溃恢复？
- Plan 审批为什么不能放行后续所有工具？
- revision 和 execution_version 分别保护什么？
- MCP 工具为什么仍然不可信？

如果其中超过五个答不上来，先使用下面的基础题训练，不要直接背 150 道进阶题。

## 第三部分：零基础面试训练题

这一组题故意比正式题库更慢、更具体。每题先给一句话答案，再解释数据流、源码和误区。

### BASIC-001：XDUDU 到底是什么，和普通聊天程序有什么区别？

**一句话答案：** XDUDU 是一个在本机运行的控制程序，它让模型可以通过受控工具观察和修改代码，而普通聊天程序通常只展示一次模型回复。

**从输入到输出：**

1. CLI 接收用户目标并保存到 Session；
2. Agent 把消息和工具定义交给 Provider；
3. 模型可能返回文字，也可能返回 ToolCall；
4. ToolRegistry 校验并执行工具；
5. ToolResult 回到模型；
6. 模型根据真实结果继续行动或给出最终答案。

**为什么重要：** Agent 的能力来自“循环 + 工具 + 状态”，不是来自更长的 Prompt。

**源码入口：** `agent.rs::run_agent`、`tools/mod.rs::ToolRegistry`。

**常见错误：** “Agent 就是能聊天的大模型。”这忽略了程序侧的工具执行和状态控制。

**追问：** 模型能直接读取文件吗？不能；模型只能生成 `file_read` 意图，由本地程序执行。

### BASIC-002：Provider 是什么？为什么不是直接叫 DeepSeek？

**一句话答案：** Provider 是统一的模型 API 适配接口，DeepSeek 只是其中一个具体实现。

**详细解释：** Agent 只理解 `ProviderRequest` 和 `ProviderResponse`。具体 Provider 负责 URL、鉴权 Header、请求 JSON、SSE 事件和错误码转换。这样 Agent 不出现大量 `if provider == "deepseek"`。

**数据流：**

```text
Agent 的统一消息
 → DeepSeekProvider 转成 DeepSeek JSON
 → Reqwest 发 HTTPS
 → 解析 DeepSeek SSE
 → 转回统一 ToolCall/文本/FinishReason
```

**源码入口：** `provider/mod.rs::Provider`、`provider/deepseek.rs`、`provider/factory.rs`。

**常见错误：**

- Provider 是模型：错误，Provider 是访问模型的适配代码；
- Provider 决定文件权限：错误，权限属于 ToolRegistry；
- Reqwest 就是 Provider：错误，Reqwest 只是 Provider 使用的 HTTP 库。

**追问：** 如何测试 Agent 而不花 API 费用？注入返回固定响应的 `MockProvider`。

### BASIC-003：Trait 是什么？它在 Provider 中怎样工作？

**一句话答案：** Trait 是 Rust 的能力合同，规定实现者必须提供哪些方法。

```rust
pub trait Provider {
    async fn chat(&self, request: ProviderRequest)
        -> XduduResult<ProviderResponse>;
}
```

`DeepSeekProvider` 实现这个方法；`MockProvider` 也可以实现。Agent 持有 `&dyn Provider`，运行时才决定调用哪个实现。

**为什么需要：** 如果 Agent 直接持有 `DeepSeekProvider`，测试和切换协议都会与 Agent 耦合。

**Trait 与依赖注入的区别：**

- Trait 定义“必须能做什么”；
- 依赖注入决定“把哪个实现交给谁”；
- Factory 根据配置创建具体实现；
- CLI Runtime 负责最终组装。

**常见错误：** “使用 trait 就自动完成依赖注入。”Trait 只定义接口，仍需要创建对象并传进去。

### BASIC-004：一次 Tool Calling 为什么需要 call ID？

**一句话答案：** call ID 把模型提出的调用与程序返回的结果一一对应。

模型可能同一轮提出多个调用：

```text
call_1 → read Cargo.toml
call_2 → read README.md
```

结果顺序可能不同，因此返回结果必须携带原始 ID。Agent 还用 ID 持久化 Pending、Running 和 Succeeded/Failed 状态。

**没有 ID 的问题：** 模型无法判断哪段内容属于哪个工具，崩溃恢复也无法区分哪些调用已执行。

**源码入口：** `provider/mod.rs::ToolCall`、`session.rs` 中工具调用记录。

### BASIC-005：为什么 ToolRegistry 不是一个简单 HashMap？

**一句话答案：** HashMap 只负责找到工具，ToolRegistry 还负责所有工具都必须遵守的安全执行流程。

```text
HashMap.get(name)       只是找到 Arc<dyn Tool>
Permission              检查能力上限
validate                检查模型输入
preflight               提前发现不可执行操作
ApprovalGate            取得具体授权
timeout + cancellation  防止永久阻塞
execute                 真正执行
redaction + audit       安全记录结果
```

**为什么集中处理：** 如果审批散落在每个工具中，新 MCP Tool 可能忘记实现某个检查。统一入口使内置和外部工具共享硬边界。

**源码入口：** `tools/mod.rs::ToolRegistry::execute_with_progress`。

### BASIC-006：Schema 是什么？为什么模型已经看到 Schema 还要 validate？

**一句话答案：** Schema 是输入说明书，validate 是执行端的门卫。

模型可能因为能力限制、上下文污染或协议错误生成：

```json
{"path": 123, "unexpected": true}
```

即使 Schema 说 `path` 应是字符串，程序也不能假设模型必然遵守。因此：

- Schema 提升生成正确率；
- validate 阻止非法数据抵达文件系统；
- 路径规范化等运行时条件还要在 execute/preflight 再检查。

**常见错误：** “JSON 能反序列化就安全。”类型正确不代表路径位于工作区，也不代表长度合理。

### BASIC-007：ReAct 为什么叫闭环？

**一句话答案：** 每次行动的真实结果都会成为下一次判断的输入。

```text
模型猜测 Cargo 项目入口
 → search_text 返回真实位置
 → 模型读取相关文件
 → 模型生成补丁
 → 测试失败
 → 模型观察错误并修复
```

如果测试失败后程序仍结束并说“已经完成”，闭环就断了。XDUDU 跟踪未解决工具失败，并把 Stop 与“满足完成条件”分开。

**源码入口：** `agent.rs::run_agent` 的轮次循环和终态分支。

### BASIC-008：流式响应是怎样从网络显示到终端的？

**一句话答案：** Provider 解析 SSE 增量，转换成事件，经 EventSink 送到 TUI，同时聚合完整消息落盘。

```text
DeepSeek SSE bytes
 → 按事件边界解码
 → 提取 text delta
 → ProviderStreamSink
 → AgentEvent::AssistantDelta
 → TUI 活动区域
```

结束时，完整 AssistantMessage 写入 Session。只保存 delta 会难以恢复；每个 delta 都插数据库又会产生大量写入，所以展示流与最终持久化需要区分。

**工具参数的区别：** 工具 arguments delta 必须先按 call index/ID 拼成完整 JSON，校验通过后才执行，不能边收到边执行。

### BASIC-009：为什么需要 CancellationToken，直接杀进程不行吗？

**一句话答案：** 协作式取消允许各层停止工作并保存可恢复状态。

直接杀进程可能发生在：

- 文件已写一半；
- ToolCall 仍为 Running；
- Plan Step 没有结束时间；
- TUI 尚未恢复终端模式。

`CancellationToken` 让 Provider、工具、MCP、Plan 和 UI 观察同一取消信号。它不保证任何代码自动停止，每个长循环和等待点仍需主动检查。

**源码入口：** `agent.rs`、`tools/mod.rs`、`plan_executor.rs`。

### BASIC-010：上下文压缩和长期记忆有什么不同？

**一句话答案：** 上下文压缩服务当前会话 Token 预算，长期记忆保存跨会话仍有价值的稳定事实。

| 项目 | 上下文摘要 | 长期记忆 |
|---|---|---|
| 范围 | 当前 Session | 多个 Session |
| 内容 | 目标、进度、关键工具结果、未决问题 | 稳定偏好和项目事实 |
| 目的 | 让长会话继续 | 下次会话仍可复用 |
| 原始数据 | 消息仍在 SQLite | 原始提炼记录仍可审计 |

错误做法是把聊天中的每个话题都永久记忆，最终造成噪声和错误偏好。XDUDU 通过提炼、合并、去重形成用户可编辑的 `MEMORY.md`。

### BASIC-011：文件事务为什么比“写文件失败就报错”复杂？

**一句话答案：** 因为多文件修改可能部分成功，且失败、并发修改和进程崩溃发生在不同时间点。

需要分别处理：

1. 补丁内容本身不匹配：预检阶段零写入；
2. 用户在预检后修改文件：哈希冲突，拒绝覆盖；
3. 第二个文件写入失败：恢复第一个文件；
4. 写入中程序崩溃：启动时根据账本和哈希恢复；
5. 成功后用户要求 Undo：确认所有文件仍等于 post-image 后整批恢复；
6. 用户成功后又手工修改：标记 Conflict，不覆盖。

**源码入口：** `tools/apply_patch.rs`、`changes.rs`。

### BASIC-012：为什么 Plan 需要 DAG，而不是 Vec 步骤？

**一句话答案：** Vec 只能表示顺序，DAG 还能表示依赖关系和可以并发的分支。

```text
步骤 A：读 Provider ─┐
                     ├─► 步骤 C：总结架构
步骤 B：读 Tools ────┘
```

C 必须等 A、B 完成。若出现 A 依赖 B、B 又依赖 A，就形成环，永远没有 Ready 节点，因此生成时必须做环检测。

**XDUDU 的可靠性字段：**

- completion criteria：完成条件；
- attempt：一次执行尝试；
- evidence：每个条件的证据；
- revision：计划内容版本；
- execution_version：执行状态版本。

### BASIC-013：SQLite 为什么适合本地 Agent？

**一句话答案：** 它不需要单独服务器，却提供事务、查询、索引和迁移，比多个 JSON 文件更适合保存关联状态。

XDUDU 需要同时更新 Session、ToolCall、Plan Step 和恢复状态。SQLite transaction 能让这些更新一起提交。WAL 改善读写并发，Schema migration 让旧版本升级，FTS5 可用于文本检索。

**局限：** SQLite 适合单机本地应用，不等于分布式数据库；跨进程仍要考虑锁和 busy timeout。

### BASIC-014：MCP 为什么不能绕过 ToolRegistry？

**一句话答案：** MCP Server 是外部、不完全可信的工具来源，接入越方便，越需要复用统一安全边界。

MCP Server 返回工具名、描述和 Schema。XDUDU 将其包装成 `McpTool` 并注册到 Registry。真正调用仍要检查当前 Permission、SideEffect、Approval、timeout 和 cancellation。

如果 MCP 调用走另一条直接路径，用户会看到“内置 file_write 需要批准，但 MCP filesystem.write 不需要”，安全模型就失效。

### BASIC-015：这个项目最值得讲的工程能力是什么？

**一句话答案：** 不是接入了多少库，而是如何把不确定的模型输出约束成可验证、可审批、可恢复的系统行为。

可以从三个层次回答：

1. **可替换：** Provider、Store、EventSink 用 trait 和依赖注入解耦；
2. **可控制：** ToolRegistry 把模型意图变成受权限和审批控制的操作；
3. **可恢复：** Session、事务账本和 Plan checkpoint 保存失败现场。

进一步加分：主动说明当前边界，例如模型仍可能做出错误判断、补偿回滚不等于文件系统真正跨文件 ACID、MCP Server 本身不是沙箱。

## 第四部分：150 道分层面试题

### 题库使用方法

- 初级：先完成 `RUST`、`ARCH`、`LLM` 初级题，再进入 `AGENT`、`TOOL`。
- 中级：重点掌握 Agent、工具、安全、SQLite、Plan、MCP，并能画出数据流。
- 高级：必须说明不变量、失败模式、替代方案和工程取舍。
- 30 分钟模拟：RUST-005、ARCH-004、LLM-006、AGENT-005、TOOL-006、PLAN-009。
- 60 分钟模拟：再加 DB-006、PLAN-015、EXT-006、TUI-003、QA-006。
- 120 分钟模拟：每章抽基础、实现、高级题各一道，并完成一次事务或 DAG 白板推演。

### 统一评分

每题 4 分：0=不了解；1=会背名词；2=能讲通用机制；3=能准确映射 XDUDU 源码与数据流；4=还能分析失败边界、替代方案与取舍。各题“评分”给出 3～4 分关键点。

### 高频 30 题

RUST-005、RUST-010、ARCH-001、ARCH-004、ARCH-009、LLM-003、LLM-006、LLM-009、AGENT-003、AGENT-005、AGENT-010、TOOL-002、TOOL-006、TOOL-011、TOOL-016、DB-003、DB-006、DB-009、PLAN-003、PLAN-006、PLAN-009、PLAN-015、MEM-005、MEM-009、EXT-003、EXT-006、TUI-003、TUI-006、QA-003、QA-006。

### 知识标签

| 标签 | 题号 |
|---|---|
| Rust/异步 | RUST-001～015、PLAN-012、TUI-004 |
| 架构/接口 | ARCH-001～012、LLM-001、TUI-001 |
| LLM/ReAct | LLM-001～015、AGENT-001～015 |
| 安全/事务 | TOOL-001～020、EXT-005～012 |
| 持久化/恢复 | DB-001～012、PLAN-001～011 |
| 编排/并发 | PLAN-012～020、RUST-010～013 |
| 扩展/知识 | MEM-001～012、EXT-001～012 |
| 产品工程 | TUI-001～008、QA-001～009 |

---

## 第一章：Rust 与 Cargo 基础（15 题）

> **知识类型：Rust。** 先读第一部分第 2～13 节；本章主要考语言和异步基础，Agent 只是应用场景。

### 知识地图

从所有权、trait 和错误模型进入依赖注入，再延伸到 Tokio、取消、并发和 Workspace。重点是 Rust 如何约束 Agent 的资源生命周期与并发边界。

### RUST-001：所有权为什么适合本地 Agent？【初级｜所有权】
- **考察目标**：把所有权与文件、进程、网络资源联系起来。
- **完整答案**：值有唯一所有者，离开作用域执行 `Drop`；借用提供临时访问。XDUDU 的 HTTP Client、SQLite、MCP 子进程和终端守卫因此有清晰生命周期，异常返回也能释放句柄。所有权只保证内存与资源管理，不能替代文件事务和数据库事务。
- **源码映射**：`tui.rs::ScreenGuard::drop`；`mcp.rs::Connection`。
- **追问**：Drop 能替代补丁回滚吗？不能，前者释放资源，后者恢复持久状态。
- **常见误区**：Rust 会自动撤销外部副作用。
- **评分**：3 分须联系 RAII；4 分须区分资源安全与业务原子性。

### RUST-002：生命周期在运行配置中解决什么？【初级｜借用】
- **考察目标**：理解借用配置为何不复制服务对象。
- **完整答案**：`AgentRunConfig<'a>` 借用 Provider、ToolRegistry、Store 和 Sink，`'a` 保证配置不比依赖活得久，避免悬垂引用和全局单例。需要跨任务共同拥有时可改 `Arc`，代价是引用计数和共享状态复杂度。
- **源码映射**：`agent.rs::AgentRunConfig`；`plan_executor.rs` 的执行配置。
- **追问**：为何不都用 `'static`？会扭曲真实生命周期或迫使泄漏对象。
- **常见误区**：生命周期是运行时计时器。
- **评分**：4 分须比较借用与 `Arc`。

### RUST-003：trait 如何解耦 Provider、Store 和 UI？【初级｜多态】
- **考察目标**：理解接口与运行时替换。
- **完整答案**：Agent 依赖 `Provider`、`SessionStore`、`EventSink` 等 trait，不知道具体厂商、SQLite 或终端。`dyn Trait` 允许运行时按配置选择实现；一次虚调用成本远小于网络 I/O，换来 Mock 测试和前端复用。
- **源码映射**：`provider/mod.rs::Provider`、`session.rs::SessionStore`、`events.rs::EventSink`。
- **追问**：何时用泛型？类型编译期固定且性能敏感时。
- **常见误区**：trait 本身就完成依赖注入；真正装配在 CLI。
- **评分**：4 分须说明动态/静态分派取舍。

### RUST-004：领域状态为何大量使用 enum？【初级｜状态建模】
- **考察目标**：理解封闭状态集和穷尽匹配。
- **完整答案**：`AgentLoopState`、`FinishReason`、`PlanStatus`、`SideEffectKind` 有限互斥，enum 可在编译期检查匹配，避免字符串漂移。跨字段合法性仍靠迁移函数；新增变体还需考虑 SQLite JSON 和事件兼容。
- **源码映射**：`events.rs`、`provider/mod.rs`、`plan.rs`、`approval.rs`。
- **追问**：enum 能消除所有非法状态吗？不能，跨字段约束仍需校验。
- **常见误区**：主要收益只是比字符串快。
- **评分**：4 分须同时说明编译安全与持久化兼容。

### RUST-005：Result 与稳定错误模型如何配合？【初级｜错误处理】
- **考察目标**：理解错误传播、分类和退出码。
- **完整答案**：`Result<T,E>` 强迫调用者处理失败。`XduduError` 统一 kind、公开消息、retryable 和 details，CLI 再映射退出码；工具以稳定 `error_code` 返回可观察失败，让模型能修正。`?` 只是传播，边界仍须脱敏、分类并决定重试。
- **源码映射**：`error.rs`、`tools/mod.rs::ToolResult`、`main.rs`。
- **追问**：为何不全用 anyhow？不利于协议稳定、机器解析和重试分类。
- **常见误区**：用户输入路径可以 `unwrap`。
- **评分**：4 分须覆盖 Provider、Tool、CLI 三层语义。

### RUST-006：async trait 的成本是什么？【进阶｜Future】
- **考察目标**：理解异步 trait 对象。
- **完整答案**：async fn 编译为匿名 Future，不同实现类型不同；`async_trait` 通常装箱 Future，使 `dyn Provider`、`dyn Tool` 易用，但增加分配和动态分派。网络/磁盘主导时可接受，高频纯计算才考虑关联 Future 或泛型。
- **源码映射**：`provider/mod.rs::Provider`、`tools/mod.rs::Tool`。
- **追问**：async 会自动并行吗？不会，必须并发轮询或 spawn。
- **常见误区**：async 等于新线程。
- **评分**：4 分须说出装箱与适用条件。

### RUST-007：Send + Sync 证明了什么？【进阶｜线程安全】
- **考察目标**：区分类型安全与业务安全。
- **完整答案**：`Send` 允许跨线程移动，`Sync` 允许共享引用跨线程，Provider、Tool、Sink 因此可被 Tokio 任务共享。但它们不证明两个文件写入逻辑上可并行，XDUDU 仍把副作用工具串行化。
- **源码映射**：上述 trait 定义；`agent.rs` 批次调度。
- **追问**：Mutex 是否保证业务正确？只保证互斥，不保证不变量正确。
- **常见误区**：Sync 表示自动同步到磁盘。
- **评分**：4 分须解释外部副作用冲突。

### RUST-008：为什么使用 Arc？【进阶｜共享所有权】
- **考察目标**：解释并发任务共享依赖。
- **完整答案**：`Arc` 以原子引用计数让多个任务共同拥有 HTTP Client、Store、Sink，而不复制底层资源。内部可变性需配合 Mutex/RwLock；不能持大锁跨网络 await，否则阻塞或死锁。
- **源码映射**：CLI Runtime 与子代理执行上下文。
- **追问**：Rc 为什么不行？它不满足跨线程 Send/Sync。
- **常见误区**：Arc 自动让内部类型安全。
- **评分**：4 分须说明引用计数和锁边界。

### RUST-009：CancellationToken 如何取消？【进阶｜协作式取消】
- **考察目标**：理解取消检查点。
- **完整答案**：父流程克隆令牌给子 Future，Ctrl+C、超时或 fail-fast 调用 cancel；长操作在 `select!`、循环和提交前检查并安全退出。它不是强杀，CPU 忙循环或不可取消阻塞调用仍会延迟。
- **源码映射**：`agent.rs`、`plan_executor.rs`、`mcp.rs`、`web_fetch.rs`。
- **追问**：为何不直接 abort？会失去清理和审计，副作用结果可能未知。
- **常见误区**：token 取消后 Future 自动停止。
- **评分**：4 分须覆盖清理和未知结果。

### RUST-010：tokio::select! 在哪里使用？【进阶｜异步复用】
- **考察目标**：解释多事件竞争与取消安全。
- **完整答案**：它等待多个 Future 中最先就绪者。XDUDU 用于 TUI 输入/Agent 事件/运行结束，以及 Provider、MCP、Web 的响应/取消竞争。未选 Future 会被丢弃，所以帧解析和外部操作必须取消安全。
- **源码映射**：`main.rs::tui_interactive_loop`、`mcp.rs::stdio_request`、Provider 流解析。
- **追问**：select 是线程池吗？不是，只是 Future 复用。
- **常见误区**：忽略未选分支被取消。
- **评分**：4 分须指出取消安全。

### RUST-011：为何只读并行、副作用串行？【进阶｜并发】
- **考察目标**：区分数据竞争和业务顺序。
- **完整答案**：`SideEffectKind::None` 可同批并发降低总延迟，结果仍按调用顺序回填；写文件、进程和网络审批可能冲突或依赖顺序，必须串行。某副作用拒绝后，后续副作用也应跳过，避免批量绕权。
- **源码映射**：`agent.rs`、`approval.rs::SideEffectKind`。
- **追问**：并行读取是否同一快照？不是，外部文件仍可能变化。
- **常见误区**：Rust 无数据竞争等于外部操作可并行。
- **评分**：4 分须说明结果确定性与拒绝传播。

### RUST-012：FuturesUnordered 为何适合任务图？【高级｜动态并发】
- **考察目标**：理解完成驱动调度。
- **完整答案**：DAG 会随依赖完成动态解锁节点；`FuturesUnordered` 可随时加入 Future，并按完成顺序产出结果。调度器据此立即传播状态和解锁后继，同时用节点索引保存报告，不能把完成顺序当声明顺序。
- **源码映射**：`subagent_graph.rs`。
- **追问**：为何不是一次 join_all？后继尚未 Ready，且 fail-fast 需中途决策。
- **常见误区**：完成顺序直接决定输出顺序。
- **评分**：4 分须覆盖动态解锁和确定性存储。

### RUST-013：为何避免持锁跨 await？【高级｜锁】
- **考察目标**：分析异步死锁和长临界区。
- **完整答案**：await 会挂起当前任务，若仍持锁，其他任务长期等待；被等待操作间接取同锁会死锁。应在锁内复制最小状态，释放后 I/O，再用版本号/CAS 提交。Plan 的 execution_version 比大锁包全流程更可靠。
- **源码映射**：`plan.rs::PlanStore`、`sqlite_session.rs::checkpoint_plan_execution`。
- **追问**：Tokio Mutex 能否消除问题？只能允许跨 await，逻辑风险仍在。
- **常见误区**：编译通过就安全。
- **评分**：4 分须提出缩小临界区和乐观并发。

### RUST-014：Serde 严格 DTO 为何是安全边界？【高级｜序列化】
- **考察目标**：理解模型 JSON 不可信。
- **完整答案**：DTO 类型和 `deny_unknown_fields` 拒绝错字段与额外内容；反序列化后仍须做长度、DAG、路径、范围和跨字段校验。JSON Schema 只约束模型生成倾向，执行端验证才是硬边界。
- **源码映射**：`plan_generation.rs`、`plan_review.rs`、`memory_suggestion.rs`、`Tool::validate`。
- **追问**：为何仍用 Value？动态工具协议需要，但进入领域前必须验证。
- **常见误区**：tool calling 保证参数合法。
- **评分**：4 分须区分语法、结构、语义三层。

### RUST-015：Cargo Workspace 如何形成架构边界？【高级｜构建】
- **考察目标**：从构建结构解释依赖方向。
- **完整答案**：Workspace 统一锁文件、依赖和门禁，同时分出 core 与 cli crate。core 不读终端、不输出 ANSI，CLI 做装配与 UI，未来前端可复用核心。拆 crate 有公共 API 和编译成本，应按变化原因而不是文件数量划分。
- **源码映射**：根及两个 crate 的 `Cargo.toml`。
- **追问**：为何 Provider 未独立 crate？当前属于 core 外部适配，继续拆收益有限。
- **常见误区**：Workspace 只是多目录一起 build。
- **评分**：4 分须说明依赖方向与拆分成本。

---

## 第二章：分层架构与依赖设计（12 题）

> **知识类型：XDUDU 工程 + 通用架构。** 先理解 struct、trait、EventSink 和 Runtime 装配。

### 知识地图

考察模型、工具、持久化、安全与终端如何成为可替换边界，以及软提示和硬策略分别落在哪里。

### ARCH-001：XDUDU 的核心分层是什么？【初级｜架构】
- **考察目标**：建立完整系统地图。
- **完整答案**：CLI 负责参数、Runtime、输入和渲染；core 包含 Agent、Provider、ToolRegistry、安全、Session/Plan、子代理、MCP 和记忆。CLI 注入依赖，Agent 发领域事件返回 UI；core 不碰 stdin/ANSI，因而可支持 JSON、测试和未来 GUI。
- **源码映射**：`main.rs::create_runtime`、`xdudu-core/src/lib.rs`。
- **追问**：审批输入属于哪层？交互在 CLI，协议与规则在 core。
- **常见误区**：TUI 是 Agent 主循环。
- **评分**：4 分须画出依赖和事件方向。

### ARCH-002：Runtime 为何在 CLI 集中装配？【初级｜依赖注入】
- **考察目标**：理解 Composition Root。
- **完整答案**：具体 Provider、SecretStore、SQLite、ApprovalGate、Ledger 和工具依赖配置与环境，由入口统一创建。Agent 只收接口，不读取全局配置；测试可替换 Mock，配置错误也在任务前失败。代价是装配函数长，但不应把依赖查找藏回 core。
- **源码映射**：`main.rs::{Runtime,create_runtime}`。
- **追问**：全局单例问题？生命周期模糊、测试污染、并发配置困难。
- **常见误区**：依赖注入必须使用框架。
- **评分**：4 分须说明构造顺序和测试收益。

### ARCH-003：领域事件为何优于 UI 回调？【初级｜事件】
- **考察目标**：理解稳定输出边界。
- **完整答案**：Agent 发 `AgentEvent` 描述状态、工具、用量和 Plan 事实；TUI、JSON、测试 Sink 各自渲染。事件不含颜色或光标，瞬时进度也不进入模型上下文。新增事件须考虑序列化兼容与脱敏。
- **源码映射**：`events.rs`、`tui.rs::TuiRenderer`、`renderer.rs`。
- **追问**：EventSink 是持久队列吗？不是，持久事实另写 SessionStore。
- **常见误区**：所有事件都应落 SQLite。
- **评分**：4 分须区分事件、消息、UI 状态。

### ARCH-004：Prompt 与 ToolRegistry 规则有何区别？【进阶｜软硬边界】
- **考察目标**：识别安全核心。
- **完整答案**：Prompt 是模型可能忽略的软约束；ToolRegistry 在代码中强制权限、校验、审批、预检、超时和执行。网页或源码提示注入即使影响模型，也不能越过路径和授权。Schema 只有在执行端验证时才成为硬边界。
- **源码映射**：`prompt.rs`、`tools/mod.rs::execute_with_progress`。
- **追问**：更长 Prompt 能替代审批吗？不能。
- **常见误区**：模型“承诺安全”即可执行。
- **评分**：4 分须给出注入攻击下的阻断路径。

### ARCH-005：为何会话消息与 Provider 消息分离？【进阶｜领域模型】
- **考察目标**：理解持久化模型和外部协议模型。
- **完整答案**：Session 保存恢复、工具记录和状态；Provider 只接收本轮角色、内容、工具和参数。Agent 从 Session 构建请求，避免数据库绑定厂商 JSON，也便于压缩、迁移和切换协议。
- **源码映射**：`session.rs`、`provider/mod.rs`、`agent.rs`。
- **追问**：要不要保存原始 HTTP 响应？默认不，应只保留必要、脱敏的领域事实。
- **常见误区**：数据库是 Provider 缓存。
- **评分**：4 分须说明反腐层和演进收益。

### ARCH-006：MCP 工具为何仍实现普通 Tool？【进阶｜扩展】
- **考察目标**：理解统一策略链。
- **完整答案**：MCP 只改变发现和远程调用。`McpTool` 动态提供定义，再进入相同权限、校验、审批、超时、取消和审计链；外部 Server 不能声明自己安全而覆盖本地策略，也无需复制第二套 Agent 循环。
- **源码映射**：`mcp.rs::{McpTool,register_configured_mcp_tools}`、`tools/mod.rs`。
- **追问**：stdio/HTTP 副作用？分别是进程执行与网络访问。
- **常见误区**：MCP 是另一个 Agent Runtime。
- **评分**：4 分须沿发现到 Observation 讲完整。

### ARCH-007：为何内部协议工具不注册 ToolRegistry？【进阶｜控制协议】
- **考察目标**：区分环境能力和状态控制信号。
- **完整答案**：`submit_plan`、`complete_step`、摘要和 `task_graph` 只在特定阶段供模型提交结构化结果，由专用执行器限制次数、FinishReason 和状态迁移。注册成通用工具会允许错误阶段调用，并混淆环境授权与内部协议。
- **源码映射**：`plan_generation.rs`、`plan_executor.rs`、`memory_suggestion.rs`、`subagent_graph.rs`。
- **追问**：内部协议需审批吗？不产生环境副作用，但必须严格校验。
- **常见误区**：所有 tool call 都应注册。
- **评分**：4 分须说明生命周期和权限差异。

### ARCH-008：为何既有 ErrorKind 又有字符串错误码？【进阶｜兼容】
- **考察目标**：理解不同消费者。
- **完整答案**：`ErrorKind` 供 core/CLI 穷尽分类和退出码；工具 `error_code` 面向模型和 JSON，可表达 `HASH_MISMATCH` 等细粒度失败而不扩全局 enum。字符串易漂移，应集中构造并测试。
- **源码映射**：`error.rs`、`tools/mod.rs::ToolResult`。
- **追问**：自然语言为何不够？机器和模型难稳定判断恢复策略。
- **常见误区**：错误码越细越好。
- **评分**：4 分须说明消费者与版本稳定性。

### ARCH-009：项目关键不变量有哪些？【高级｜系统设计】
- **考察目标**：从系统而非文件列表理解架构。
- **完整答案**：统一工具策略链；路径不逃逸；秘密不进配置/日志/模型；调用先记 Pending；未知副作用不重放；Stop 不等于成功；Plan 批准不放行工具；只读并行、副作用有序；core 不依赖 UI；reasoning 不公开；迁移失败回滚。
- **源码映射**：`tools/mod.rs`、`agent.rs`、`plan_executor.rs`、`redaction.rs`、`sqlite_session.rs`。
- **追问**：最影响可信度的一条？不凭模型文本判成功。
- **常见误区**：把使用 Rust 当业务不变量。
- **评分**：4 分须列至少六条并指出落点。

### ARCH-010：增加 GUI 时哪些层不变？【高级｜演进】
- **考察目标**：验证分层复用。
- **完整答案**：Provider、Agent、ToolRegistry、Session/Plan、审批协议、记忆和 MCP 留在 core；GUI 实现 EventSink、输入、审批交互和会话浏览。剪贴板、编辑器、ANSI 是前端适配，GUI 不能直接写文件绕过 ToolRegistry。
- **源码映射**：`xdudu-core/src/lib.rs`、CLI `main.rs`/`tui.rs`。
- **追问**：审批 UI 能决定什么？收集选择，规则判定仍复用 core。
- **常见误区**：复制 Agent 循环。
- **评分**：4 分须提出适配层方案。

### ARCH-011：是否应改成微服务？【高级｜权衡】
- **考察目标**：避免架构形式主义。
- **完整答案**：本机 Agent 紧邻文件、进程、凭据和交互，拆微服务会增加部署、认证、延迟和秘密传输面，当前 trait/crate 已足够隔离。只有多租户远程执行、独立扩缩容或合规隔离出现时才重评，并补身份、租户和沙箱。
- **源码映射**：`docs/ARCHITECTURE.md`、Workspace。
- **追问**：MCP 算微服务吗？是协议边界，不等于中心化系统架构。
- **常见误区**：模块多就该拆服务。
- **评分**：4 分须从故障域与产品约束论证。

### ARCH-012：如何选择内置、Tool、Skill、MCP 或插件？【高级｜扩展决策】
- **考察目标**：建立决策树。
- **完整答案**：核心状态机/安全不变量放 core；模型可调用的 JSON 能力做 Tool；流程知识做 Skill；独立语言或外部系统做 MCP；插件只声明组合 MCP，不加载进程内代码。依据是信任、部署、副作用、升级、性能和崩溃隔离。
- **源码映射**：`tools/mod.rs`、`skills.rs`、`mcp.rs`、`plugin.rs`。
- **追问**：Python RAG 放哪里？若有真实需求，优先受限 MCP Server。
- **常见误区**：插件目录里的代码都可直接执行。
- **评分**：4 分须给出分类条件和安全理由。

---

## 第三章：LLM 与 Provider（15 题）

> **知识类型：Agent 协议 + XDUDU 工程。** Rust 只用于实现接口；重点是消息、流式响应和厂商适配。

### 知识地图

覆盖统一消息、厂商适配、SSE 聚合、工具调用、reasoning 边界和安全重试。必须区分“模型协议成功”和“Agent 任务完成”。

### LLM-001：Provider 抽象解决什么？【初级｜适配器】
- **考察目标**：理解领域协议与厂商 HTTP 差异。
- **完整答案**：`Provider` 接收统一 Request、返回统一 Response，Agent 不依赖鉴权头、JSON 字段和 SSE 事件。具体实现做协议翻译、FinishReason 和工具参数聚合；Factory 按配置创建并包装重试。独有能力应显式建模，不能让厂商分支污染 Agent。
- **源码映射**：`provider/mod.rs::Provider`、`provider/factory.rs`。
- **追问**：为何不用厂商 SDK 类型贯穿？会绑死会话、测试和切换。
- **常见误区**：统一 Base URL 就是完整抽象。
- **评分**：4 分须说明适配器、Factory 和能力差异。

### LLM-002：统一消息模型有哪些关键内容？【初级｜消息】
- **考察目标**：理解角色、内容和工具闭环。
- **完整答案**：消息包含角色和内容块；助手消息携带 `ToolCall`，工具消息以 call ID 回传结果。Request 还含系统 Prompt、工具 Schema 和模型参数。call ID 使多工具 Observation 不错配，不能简单把结果拼成 user 文本。
- **源码映射**：`provider/mod.rs::{ProviderMessage,ToolCall,ProviderRequest}`。
- **追问**：为何工具结果需独立角色？保留协议语义并降低注入混淆。
- **常见误区**：工具调用是独立聊天。
- **评分**：4 分须说明 call ID 和多轮关系。

### LLM-003：FinishReason 如何影响 Agent？【初级｜终止语义】
- **考察目标**：避免把停止生成当成功。
- **完整答案**：`ToolCalls` 进入执行并继续观察；`Length` 是截断；`Stop` 仅表示本轮停止，还要检查待调用和未解决失败；协议异常进入 Error。各 Provider 必须把厂商字段准确映射，否则会误判终态。
- **源码映射**：`provider/mod.rs::FinishReason`、`agent.rs`、各 Provider 解析器。
- **追问**：Stop+文本是否 Completed？有失败或未验证条件时不是。
- **常见误区**：HTTP 200 等于任务成功。
- **评分**：4 分须沿终态检查回答。

### LLM-004：为何系统 Prompt 不重复完整 Schema？【初级｜Token】
- **考察目标**：理解文本规则与结构化 tools 参数分工。
- **完整答案**：Prompt 只放原则和简短工具索引，完整 Schema 通过 tools 参数发送。重复会浪费上下文、形成两个可能漂移的真相源，并诱导模型输出文本 JSON。执行端仍以 ToolDefinition 和 validate 为准。
- **源码映射**：`prompt.rs::build_system_prompt`、`agent.rs`。
- **追问**：工具描述能否删除？不宜，语义描述帮助选择。
- **常见误区**：减少 Prompt 就可减少校验。
- **评分**：4 分须说明一致性与执行验证。

### LLM-005：三类 Provider 如何复用？【进阶｜协议适配】
- **考察目标**：识别共享 wire 层与独立协议。
- **完整答案**：DeepSeek 与 OpenAI-compatible 复用 OpenAI 风格 wire 解析，保留各自配置；Anthropic 使用独立消息和 SSE 结构。复用应发生在确实等价的协议层，不能因 JSON 相似强行合并语义。
- **源码映射**：`deepseek.rs`、`openai_compatible.rs`、`openai_wire.rs`、`anthropic.rs`。
- **追问**：compatible 是否完全兼容？不保证，需白名单和能力测试。
- **常见误区**：只靠模型名决定协议。
- **评分**：4 分须说明共享与分叉边界。

### LLM-006：SSE 工具调用为何必须聚合？【进阶｜流式】
- **考察目标**：理解 arguments 跨帧到达。
- **完整答案**：工具名、ID、JSON 参数可能拆成多帧，多调用还会按 index 交错。解析器按 index 累积，结束后才 parse 和执行；文本 delta 可即时显示。还要处理 CRLF、多个 data 行、DONE、未知事件和中断。
- **源码映射**：`provider/stream.rs`、`openai_wire.rs`、`anthropic.rs`。
- **追问**：每帧 parse 参数为何错？半截 JSON 本来合法但尚未完整。
- **常见误区**：一帧就是完整业务对象。
- **评分**：4 分须描述 index 聚合和完成边界。

### LLM-007：文本流与工具流有何不同？【进阶｜流式 UI】
- **考察目标**：区分可展示增量和可执行对象。
- **完整答案**：文本 delta 可发 UI，最终汇总落会话；工具 delta 只能内部拼接，完整验证后执行。已 emit 文本后透明重试会重复内容，因此 RetryingProvider 要记录是否输出；UI 也应区分活动区和已提交 transcript。
- **源码映射**：`provider/stream.rs`、`provider/retry.rs`、`tui.rs`。
- **追问**：非流式为何同一 Response？保持 Agent 语义一致。
- **常见误区**：流式只是动画。
- **评分**：4 分须覆盖重复输出风险。

### LLM-008：reasoning_content 的双边界是什么？【进阶｜隐藏推理】
- **考察目标**：区分闭环回传与公开输出。
- **完整答案**：工具循环可能要求回传上一轮 reasoning，XDUDU 将其保存于内部会话并用于下一请求；普通 TUI、session show、导出和事件不展示原始思维链。调试只提供脱敏结构化轨迹，持久化也要限长脱敏。
- **源码映射**：`provider/mod.rs`、`openai_wire.rs`、`agent.rs`、`session.rs`。
- **追问**：能否完全不存？若协议要求回传，则需保存运行所需最小内容。
- **常见误区**：“Thought for 6s”就是原始推理。
- **评分**：4 分须区分协议、存储、展示。

### LLM-009：何时可以安全重试 Provider？【进阶｜幂等】
- **考察目标**：判断重复输出与未知结果。
- **完整答案**：连接失败、限流和部分 5xx 可在未 emit 文本、未形成工具结果时有限指数退避。已有可见增量后重试会重复；工具调用到达状态未知也不能假设幂等。4xx 通常需修配置，重试应尊重取消。
- **源码映射**：`provider/retry.rs`、`provider/mod.rs::retryable_status`。
- **追问**：超时都重试吗？不是，要看是否已产生输出和错误类别。
- **常见误区**：无限重试提高可靠性。
- **评分**：4 分须提 emitted、退避和非确定性。

### LLM-010：TokenUsage 为何统一建模？【进阶｜预算】
- **考察目标**：理解用量除计费外的作用。
- **完整答案**：统一 Usage 供 Renderer 展示、Agent 累积和上下文预算决策。厂商不返回时只能估算，不能冒充精确计费；系统 Prompt、工具和结果也占窗口。用量是观测信息，不参与授权。
- **源码映射**：`provider/mod.rs::TokenUsage`、`events.rs::UsageUpdated`、`agent.rs`。
- **追问**：中文字符能直接等于 token？不能，不同 tokenizer 差异大。
- **常见误区**：只统计最终回答。
- **评分**：4 分须区分精确用量和估算。

### LLM-011：Provider 能力探测应如何设计？【高级｜能力协商】
- **考察目标**：设计安全演进。
- **完整答案**：定义 tools、streaming、reasoning、JSON Schema、上下文等显式 Capability，由实现静态声明或受控探测；Agent 据此降级或报错。缓存须绑定 Base URL、模型和版本，不能靠不断试错猜参数。
- **源码映射**：`provider/mod.rs`、`factory.rs`；`TASKS.md` M3 候选。
- **追问**：探测失败就 fallback？仅在显式策略和安全错误类别下。
- **常见误区**：一个 supports_tools 布尔值足够。
- **评分**：4 分须给出能力模型和失败策略。

### LLM-012：如何防自定义 Base URL 泄密？【高级｜配置安全】
- **考察目标**：理解 Provider 配置的信任边界。
- **完整答案**：默认要求 HTTPS，回环测试例外；不可信项目配置不能静默把用户 Key 发到任意域名，应追踪来源并验证。SecretString 仅在构造鉴权头时暴露，重定向和日志都需防止凭据跨主机或输出。
- **源码映射**：`config.rs`、`credentials.rs`、Provider Client。
- **追问**：HTTPS 就可信？只保护传输，不证明配置域名可信。
- **常见误区**：URL 可解析即可发 Key。
- **评分**：4 分须覆盖来源、TLS、重定向、日志。

### LLM-013：如何离线测试 Provider？【高级｜Mock】
- **考察目标**：设计确定性协议测试。
- **完整答案**：Agent 注入 MockProvider 返回预设工具、截断和错误；wire 层用本地 HTTP Server 喂真实 SSE 分片；少量 opt-in 网络冒烟只验真实兼容。普通测试不能依赖余额、网络和随机输出。
- **源码映射**：Provider 模块测试、`agent.rs` 测试、集成测试。
- **追问**：Mock 完整 Response 能测流解析吗？不能，需单独 wire 测试。
- **常见误区**：只断言 HTTP 200。
- **评分**：4 分须区分契约、协议、冒烟三层。

### LLM-014：怎样增加 Provider fallback？【高级｜容错】
- **考察目标**：分析未实现能力。
- **完整答案**：先定义可切换错误、能力等价、消息/Schema 转换、用量和告警。仅在未输出文本、工具或未知结果时切换较安全；工具循环中 reasoning 也可能不可迁移。fallback 必须显式配置，不能因内容过滤或 4xx 静默换模型。
- **源码映射**：`provider/retry.rs` 当前同 Provider 重试；`TASKS.md` M3。
- **追问**：更强模型一定能替代？能力、上下文和策略可能不同。
- **常见误区**：遍历 Provider 列表即可。
- **评分**：4 分须列切换前置条件。

### LLM-015：是否应引入 LangGraph？【高级｜框架取舍】
- **考察目标**：基于现状判断。
- **完整答案**：XDUDU 已有 Rust ReAct、Plan、任务图、检查点和安全链；直接引入 Python LangGraph 会增加双运行时、状态同步与部署边界。Python 工作流可先做 MCP；只有成熟生态带来可量化 Eval 收益且能接受跨进程边界时才评估。
- **源码映射**：`agent.rs`、`plan_executor.rs`、`subagent_graph.rs`、`mcp.rs`。
- **追问**：自研风险？协议、恢复和评测需持续维护。
- **常见误区**：不用框架就没有图调度。
- **评分**：4 分须比较集成成本与真实缺口。

---

## 第四章：ReAct 与上下文工程（15 题）

> **知识类型：Agent 原理。** 状态机、压缩和停滞检测属于 Agent 运行逻辑，不是 Rust 语法。

### 知识地图

解释 Planning—Acting—Observing—Reflecting 循环、上下文压缩、终态治理和停滞恢复。

### AGENT-001：聊天模型与 Agent 根本区别？【初级｜Agent】
- **考察目标**：理解行动闭环。
- **完整答案**：聊天模型生成文本；Agent 将模型置于请求—工具—真实执行—结果回传—再判断循环，并由代码控制权限、持久化和终态。可信度来自工具事实与验证，不来自像真的描述。
- **源码映射**：`agent.rs::run_agent`、`tools/mod.rs::ToolRegistry`。
- **追问**：一次工具调用就是 Agent？至少还要观察并继续决策。
- **常见误区**：长 Prompt 就是 Agent。
- **评分**：4 分须说明模型和执行环境职责。

### AGENT-002：ReAct 状态如何流转？【初级｜状态机】
- **考察目标**：准确说出阶段。
- **完整答案**：首次请求 Planning；执行工具 Acting；结果写回 Observing；携 Observation 再请求 Reflecting；新工具再 Acting，通过终态检查才 Completed。取消、轮次耗尽和协议错误分别进入 Interrupted、Incomplete、Error。
- **源码映射**：`events.rs::AgentLoopState`、`agent.rs`。
- **追问**：Reflecting 是公开思维链吗？不是，只是状态。
- **常见误区**：第一轮后全是 Acting。
- **评分**：4 分须说明事件顺序与终态。

### AGENT-003：一轮请求包含什么？【初级｜上下文】
- **考察目标**：理解模型实际输入。
- **完整答案**：系统 Prompt、工作区、Instructions/Skill、相关记忆、压缩摘要、预算内消息、工具 Schema 和模型参数。工具结果按 call ID 回传；所有动态内容先脱敏限长，不把整个仓库或无限输出塞入窗口。
- **源码映射**：`agent.rs`、`prompt.rs`、`instructions.rs`、`main.rs::relevant_memories`。
- **追问**：工具为何每轮可变化？运行模式和内部协议不同。
- **常见误区**：数据库全部消息原样发送。
- **评分**：4 分须覆盖摘要、工具、记忆与预算。

### AGENT-004：工具为何先写 Pending？【进阶｜恢复】
- **考察目标**：理解意图先行记录。
- **完整答案**：先持久化 Pending 再执行；崩溃后可识别“可能发生但结果未知”的调用，标记 Interrupted/Cancelled 而不是自动重放。写文件、命令、网络尤其需要该边界。
- **源码映射**：`agent.rs`、`sqlite_session.rs` 恢复。
- **追问**：Pending 证明未发生吗？不能，只证明意图已记录。
- **常见误区**：恢复时继续所有 Pending。
- **评分**：4 分须解释未知结果原则。

### AGENT-005：Stop 为何不等于完成？【进阶｜终态】
- **考察目标**：区分模型声明与系统事实。
- **完整答案**：Agent 仍检查待处理调用、`unresolved_tool_failures`、Length 和轮次上限。测试失败或审批拒绝未解决时，即使模型说完成也只能 Incomplete；安全重试成功后才移除对应失败。
- **源码映射**：`agent.rs` 终态分支。
- **追问**：最终文本非空即可吗？不可以。
- **常见误区**：模型自然语言覆盖工具错误。
- **评分**：4 分须解释失败集合生命周期。

### AGENT-006：同批副作用拒绝如何传播？【进阶｜批处理】
- **考察目标**：理解批量绕权防护。
- **完整答案**：某副作用被权限/审批拒绝后，后续副作用返回 `BATCH_SIDE_EFFECT_SKIPPED`，只读仍可执行。否则模型可把多个写操作放一批规避用户拒绝。全部结果回传，模型只能解释或请求新授权。
- **源码映射**：`agent.rs`、`tools/mod.rs`。
- **追问**：为何保留只读？不扩大副作用且可帮助诊断。
- **常见误区**：先并发启动全批再审批。
- **评分**：4 分须说明顺序与安全理由。

### AGENT-007：为何不能简单删除最旧消息？【进阶｜压缩】
- **考察目标**：理解工具对和关键约束。
- **完整答案**：按条删除会留下 Tool 结果却丢 call，或丢用户目标。XDUDU 按预算保留近期窗口、调整工具边界并生成结构化摘要；原始消息仍在 SQLite，压缩只是 Provider 视图。
- **源码映射**：`agent.rs::compact_context`、`session.rs`。
- **追问**：摘要可含 Key？不可，须脱敏。
- **常见误区**：压缩等于清库。
- **评分**：4 分须说明工具配对与审计历史。

### AGENT-008：分级压缩为何更合理？【进阶｜LLM 摘要】
- **考察目标**：比较确定性截断与模型摘要。
- **完整答案**：轻度超限用确定性截断，低延迟且无模型失败；远超预算才用严格 `submit_context_summary` 协议。摘要失败保留旧摘要并回退，压缩阶段不开放环境工具。
- **源码映射**：`agent.rs`、`M11_CONTEXT_WEBMEM_DESIGN.md`。
- **追问**：为何不每轮摘要？成本、延迟与漂移累积。
- **常见误区**：模型摘要必然更准确。
- **评分**：4 分须说明阈值、回退、无副作用。

### AGENT-009：如何防摘要污染？【高级｜可靠性】
- **考察目标**：判断二次生成内容可信度。
- **完整答案**：摘要只总结既有事实，使用严格 DTO、长度上限和脱敏；失败状态与用户约束不能被写成已完成。保存摘要边界避免重复/跳过，调试时回查原始消息而非把摘要当唯一真相。
- **源码映射**：`agent.rs`、`session.rs`、`redaction.rs`。
- **追问**：摘要能决定权限吗？不能。
- **常见误区**：系统摘要天然可信。
- **评分**：4 分须覆盖校验、事实边界和审计。

### AGENT-010：停滞检测如何判断无进展？【高级｜自愈】
- **考察目标**：理解失败窗口。
- **完整答案**：记录近期工具签名、错误码和结果摘要，识别连续重复失败或无进展窗口；达到阈值按 auto/ask/off 提醒改方案、询问或停止。签名须脱敏有界，时间戳等噪声不能算进展。
- **源码映射**：`stall.rs`、`agent.rs`、`events.rs::StalledRecovery`。
- **追问**：单次 HASH_MISMATCH 为何不算？可重读后安全恢复。
- **常见误区**：轮次多就等于停滞。
- **评分**：4 分须说明窗口、签名、恢复策略。

### AGENT-011：本地无结果为何进入 Web 闭环？【进阶｜检索】
- **考察目标**：区分项目事实和通用知识。
- **完整答案**：项目事实先查工作区；通用或时效问题可 web_search 找候选，再 web_fetch/read 来源。网络仍审批且网页是不可信数据；不能用网页覆盖本地源码，也不能本地零结果就结束。
- **源码映射**：`prompt.rs`、`web_search.rs`、`web_fetch.rs`。
- **追问**：任何权限模式都可直接联网？可提出，但访问仍受审批。
- **常见误区**：先 Web 搜内部符号。
- **评分**：4 分须说明来源优先级和授权。

### AGENT-012：为何不输出原始思维链？【进阶｜可解释性】
- **考察目标**：区分证据与内部推理。
- **完整答案**：原始推理可能含错误假设、敏感片段和噪声。默认展示计划、工具、进度、结果、证据；调试只给脱敏结构化事件。可解释性应建立在可验证动作而非逐 token 思维上。
- **源码映射**：`events.rs::DebugTrace`、`renderer.rs`、`tui.rs`。
- **追问**：如何调试？看状态、工具摘要、错误码、用量和终态原因。
- **常见误区**：隐藏思维链等于不可解释。
- **评分**：4 分须提出证据型观测替代。

### AGENT-013：Prompt Injection 如何防？【高级｜安全】
- **考察目标**：识别源码、网页、MCP 输出均不可信。
- **完整答案**：Prompt 声明外部内容仅为数据，但硬防线是路径限制、秘密隔离、网络审批、命令规则和脱敏。工具结果还需限长，避免恶意内容挤满上下文；任何文档都不能授予权限。
- **源码映射**：`prompt.rs`、`permission.rs`、`approval.rs`、`redaction.rs`。
- **追问**：模型拒绝就够吗？不够，需纵深硬边界。
- **常见误区**：过滤一句“ignore previous”。
- **评分**：4 分须给完整防御链。

### AGENT-014：如何设计 Agent Eval？【高级｜评测】
- **考察目标**：超越单元测试。
- **完整答案**：用固定任务、仓库夹具和完成判据统计成功率、工具正确率、绕权率、无效轮次、token 与延迟。输出可变，验收最终文件、测试和事件轨迹，不逐字比较；安全集含注入、路径逃逸和谎报完成。
- **源码映射**：`AGENT_LEARNING_GUIDE.md` Eval 章节、现有 Mock/集成测试。
- **追问**：如何比较 Prompt？同模型参数多次运行并分类失败。
- **常见误区**：手工聊几次评质量。
- **评分**：4 分须给可执行指标和事实验收器。

### AGENT-015：上下文窗口变大后还需压缩吗？【高级｜演进】
- **考察目标**：分析成本和信噪比。
- **完整答案**：大窗口仍增加成本、延迟、注意力稀释和注入面。应保留短期消息、结构摘要、相关记忆和按需文件/Web 检索；预算随模型配置，用 Eval 比较，不把仓库和历史常驻。
- **源码映射**：`agent.rs`、`memories.rs`、`search_text.rs`、`web_read.rs`。
- **追问**：何时缓存 Prompt？稳定大前缀且不跨信任边界时。
- **常见误区**：窗口越大答案越好。
- **评分**：4 分须讨论成本、召回和检索分层。

---

## 第五章：工具、安全与文件事务（20 题）

> **知识类型：XDUDU 工程 + 通用安全。** Tool Calling 是 Agent 概念，ToolRegistry 和事务链是项目实现。

### 知识地图

ToolRegistry 是模型与操作系统之间的硬边界。重点掌握“查找→权限→校验→取消→预检→审批→超时执行→审计”的固定顺序，以及文件、进程、网络各自的额外不变量。

### TOOL-001：ToolDefinition 为什么同时含 Schema、权限和副作用？【初级｜工具模型】
- **考察目标**：理解“能做什么”和“如何授权”必须同源。
- **完整答案**：名称/描述/Schema 供模型选择和输入生成；`PermissionLevel` 表示最低运行模式；`SideEffectKind` 决定是否审批及审计类别。定义随工具注册，避免 UI、Prompt 和执行器维护三份分类。动态 MCP 工具也必须映射这些字段。
- **源码映射**：`tools/mod.rs::{ToolDefinition,Tool}`、`mcp.rs::McpTool`。
- **追问**：Schema 能描述业务副作用吗？不能，必须单独建模。
- **常见误区**：只要参数合法就可执行。
- **评分**：4 分须解释模型选择、权限、审批三方消费者。

### TOOL-002：ToolRegistry 策略链为何顺序固定？【进阶｜策略链】
- **考察目标**：能完整复述执行管线。
- **完整答案**：先查工具，再检查 Permission，随后 validate、取消检查、preflight；只有输入确定可执行后才审批，最后 timeout 包 execute 并统一记录耗时与结果。审批太早会让用户批准无效操作，预检后直接执行则会绕过授权，执行后才审计又无法证明意图。
- **源码映射**：`tools/mod.rs::execute_with_progress`。
- **追问**：为何取消检查不只一次？长预检和执行内部也要观察取消。
- **常见误区**：审批通过后可跳过哈希复检。
- **评分**：4 分须给顺序及每次换序的后果。

### TOOL-003：PermissionMode 与 PermissionLevel 如何匹配？【初级｜权限】
- **考察目标**：理解能力上限。
- **完整答案**：运行模式 read-only、auto-safe、full-access 定义本次进程可触达的能力上限；工具声明最低级别，Registry 在审批前拒绝越界。用户审批只能在当前能力上限内授权，不能把 read-only 临时升级为写权限。
- **源码映射**：`permission.rs::{PermissionMode,PermissionLevel}`、`tools/mod.rs`。
- **追问**：网络工具为何可声明 ReadOnly 仍审批？它不改工作区，但属于独立 NetworkAccess 副作用。
- **常见误区**：批准按钮可覆盖 PermissionMode。
- **评分**：4 分须区分能力上限与具体授权。

### TOOL-004：SideEffectKind 为什么独立于 PermissionLevel？【进阶｜副作用】
- **考察目标**：理解二维安全模型。
- **完整答案**：权限级别回答“模式是否允许”，副作用回答“本次调用影响什么”。网络读取对文件是只读，却会泄露查询并访问外部；stdio MCP 启动进程；文件写入改变工作区。独立建模才能正确审批、持久规则和审计。
- **源码映射**：`approval.rs::SideEffectKind`、各 ToolDefinition。
- **追问**：git_status 的副作用？`None`，内部固定启动 git 是可信实现细节。
- **常见误区**：ReadOnly 等于无副作用。
- **评分**：4 分须给网络和进程反例。

### TOOL-005：输入校验为何由工具再次实现？【初级｜防御性编程】
- **考察目标**：区分 Schema 提示和执行证明。
- **完整答案**：Provider 可能忽略 Schema，MCP/测试也可直接构造 JSON。`Tool::validate` 收集类型、范围、长度和枚举问题，执行函数还验证路径与当前状态。Schema 改善生成质量；validate 才阻止非法输入抵达操作系统。
- **源码映射**：`tools/mod.rs::Tool::validate`、各工具实现。
- **追问**：为何收集多个问题？模型一次可修正全部，减少循环。
- **常见误区**：serde parse 成功即语义合法。
- **评分**：4 分须说明两层验证和错误反馈。

### TOOL-006：preflight 对 apply_patch 有什么价值？【进阶｜预检】
- **考察目标**：理解审批前的无副作用证明。
- **完整答案**：preflight 完整解析补丁、校验路径、读取前镜像并在内存精确应用 hunk，证明补丁当前可执行，但不写文件。随后只需一次 workspace-write 审批；真正提交前再比哈希防 TOCTOU。无效补丁不打扰用户，批准内容也更具体。
- **源码映射**：`tools/apply_patch.rs::preflight`、`tools/mod.rs`。
- **追问**：preflight 成功为何仍可能失败？用户或进程可能在批准期间修改文件。
- **常见误区**：预检后无需二次读取。
- **评分**：4 分须解释 TOCTOU 和审批体验。

### TOOL-007：工作区路径逃逸如何防？【进阶｜路径安全】
- **考察目标**：理解 canonicalize、符号链接和待创建路径。
- **完整答案**：已有路径规范化工作区根和目标后检查前缀；写入新文件时先解析最近存在父目录并逐层拒绝逃逸。拒绝 `..`、绝对路径和指向外部的 symlink，不能只做字符串 starts_with。删除/创建还要在提交时复检目标类型。
- **源码映射**：`tools/path_policy.rs::{resolve_existing,resolve_writable}`。
- **追问**：为什么字符串前缀不够？`/repo2` 以 `/repo` 开头，且符号链接可改变真实位置。
- **常见误区**：过滤 `..` 就安全。
- **评分**：4 分须说明现有与新路径两类解析。

### TOOL-008：file_read 如何处理大文件和二进制？【初级｜有界 I/O】
- **考察目标**：理解上下文和内存边界。
- **完整答案**：读取前校验工作区路径、普通文件和范围；限制字节/行数，检测 NUL 或非 UTF-8，避免把二进制和超大文件灌入模型。输出标记截断而非假装完整。分页参数让 Agent 按需继续读。
- **源码映射**：`tools/file_read.rs::FileReadTool`。
- **追问**：截断内容能否用于哈希写回？不能，修改需重新获取完整受控前镜像或使用补丁上下文。
- **常见误区**：一次读完整仓库更省轮次。
- **评分**：4 分须覆盖路径、类型、限长和截断语义。

### TOOL-009：file_write 的 expectedSha256 解决什么？【进阶｜乐观并发】
- **考察目标**：理解并发覆盖防护。
- **完整答案**：模型读取文件后到写入前，用户可能修改它。调用携带前镜像 SHA-256，写前重新计算；不匹配返回 `HASH_MISMATCH`，要求重读而不是覆盖。它是文件级乐观锁，不证明内容业务正确，仍需测试和 diff。
- **源码映射**：`tools/file_write.rs`。
- **追问**：新文件如何检查？确认目标不存在；存在即冲突。
- **常见误区**：哈希用于加密文件。
- **评分**：4 分须解释 TOCTOU 和安全恢复。

### TOOL-010：为什么写文件使用临时文件和原子替换？【进阶｜原子写】
- **考察目标**：理解半写和崩溃风险。
- **完整答案**：直接 truncate 后写入，崩溃可能留下空或半文件。先在同目录写临时文件、同步必要内容、保留权限，再 rename 替换，可利用同文件系统原子性缩短不一致窗口。删除和跨多文件仍需事务账本与回滚。
- **源码映射**：`file_write.rs`、`apply_patch.rs` 提交函数。
- **追问**：rename 跨文件系统原子吗？不保证，因此临时文件须在目标目录。
- **常见误区**：异步 write_all 就是原子写。
- **评分**：4 分须说明同目录、权限和多文件边界。

### TOOL-011：多文件 apply_patch 如何保证原子性？【高级｜事务】
- **考察目标**：白板说明完整事务。
- **完整答案**：先解析全部 patch、校验所有路径、读取前镜像和哈希、内存应用全部 hunk；审批后写 `Prepared` 账本，标 `Applying`，写临时文件并逐项替换/删除。任何失败按前镜像整体回滚；全部成功才 `Applied`。提交前复检哈希，账本写失败则不留未记录修改。
- **源码映射**：`apply_patch.rs`、`changes.rs::{ChangeSetDraft,ChangeSetStatus}`。
- **追问**：逐项 rename 不是整体原子，何以称事务？通过预写日志和补偿恢复提供应用级原子语义。
- **常见误区**：第二个文件失败只回滚第二个。
- **评分**：4 分须按阶段和崩溃点说明。

### TOOL-012：精确 hunk 匹配为何不做模糊匹配？【进阶｜补丁】
- **考察目标**：理解安全与便利的取舍。
- **完整答案**：模糊匹配可能把修改应用到相似但错误的代码段，编码 Agent 中静默错改风险高。XDUDU 要求上下文、行和换行语义精确匹配，失败返回稳定错误让模型重读生成新 patch。代价是并发编辑下重试更多，但行为可审计。
- **源码映射**：`apply_patch.rs::{parse_patch,apply_hunks}`。
- **追问**：如何改善成功率？补丁前搜索/读取最小相关范围并带稳定上下文。
- **常见误区**：patch 工具应像 git apply 一样尽量猜。
- **评分**：4 分须说明错误应用的危害。

### TOOL-013：CRLF 和末尾换行为何是补丁难点？【高级｜文本语义】
- **考察目标**：理解字节级正确性。
- **完整答案**：按 `lines()` 重组会丢 CRLF 与末尾是否有换行的信息，造成整文件噪声或哈希变化。解析需记录行结束风格与 EOF newline，应用 hunk 后按原风格重建；新文件从 patch 语义决定。UTF-8 字符列和字节索引也不能混用。
- **源码映射**：`apply_patch.rs::split_text`、`apply_hunks`。
- **追问**：为何测试中文和无末尾换行？能暴露字节/字符与 EOF 错误。
- **常见误区**：文本文件换行差异无关紧要。
- **评分**：4 分须说明 CRLF、EOF、Unicode 三点。

### TOOL-014：Undo 为什么要整批哈希检查？【高级｜撤销】
- **考察目标**：理解不覆盖用户后续修改。
- **完整答案**：一个 change set 是完整事务。撤销前检查所有目标当前哈希是否等于事务后镜像；任一冲突则整批不改，避免部分撤销和覆盖用户新内容。通过前镜像恢复创建、修改、删除，并标 `Undone`；旧 v1 单文件记录仍兼容读取。
- **源码映射**：`changes.rs`、CLI undo 处理。
- **追问**：能否强制撤销冲突文件？若未来提供也必须显式高风险操作，默认不能。
- **常见误区**：逐文件尽力撤销更友好。
- **评分**：4 分须解释事务边界和后镜像校验。

### TOOL-015：terminal_exec 如何降低 Shell 注入？【进阶｜进程安全】
- **考察目标**：理解命令执行边界。
- **完整答案**：输入是命令字符串但执行前有长度、cwd、权限、审批和命令规则；环境变量清理敏感值，超时/取消终止子进程并限制输出。专用 git/file/web 工具存在时 Prompt 禁止用 shell 绕过。更强方案是 argv 结构化执行，但会降低自然命令兼容性。
- **源码映射**：`tools/terminal_exec.rs`、`config.rs` command rules。
- **追问**：允许前缀规则为何不能只做 starts_with？会把 `cargo test-malicious` 误匹配，需按 argv/边界解析。
- **常见误区**：用户批准一次 shell 等于永久 full access。
- **评分**：4 分须覆盖规则、环境、超时和替代方案。

### TOOL-016：命令 allow/deny/ask 的优先级为何 deny 优先？【高级｜策略】
- **考察目标**：理解显式拒绝覆盖宽泛允许。
- **完整答案**：规则按规范化命令前缀匹配，`deny > allow > ask`。管理员或用户可用窄 deny 阻止危险子命令，即使存在较宽 allow；未匹配按当前审批策略。规则来源、匹配结果要可诊断，不能让项目文件静默扩大用户全局权限。
- **源码映射**：`config.rs` 命令规则、`terminal_exec.rs`。
- **追问**：永久 allow 存哪里？用户级受控配置，不能写仓库。
- **常见误区**：最长前缀 allow 必然获胜。
- **评分**：4 分须解释拒绝优先和信任来源。

### TOOL-017：git_status/diff 为何用固定命令而不审批？【进阶｜专用工具】
- **考察目标**：理解可信实现细节。
- **完整答案**：两工具只读，内部构造固定 argv，禁 ext-diff、textconv、no-index 和外部仓库，路径以 `--` 分隔；启动 git 是工具实现，不是模型任意进程执行，因此 `SideEffectKind::None`。输出有文件/字节上限并结构化解析。
- **源码映射**：`git_common.rs`、`git_status.rs`、`git_diff.rs`。
- **追问**：为何禁 textconv？Git 配置可触发外部程序和读取额外内容。
- **常见误区**：任何子进程都必须按 terminal_exec 审批。
- **评分**：4 分须说明固定 argv 与配置攻击面。

### TOOL-018：search_text 不依赖 rg 有什么价值？【进阶｜可移植搜索】
- **考察目标**：理解产品依赖和资源上限。
- **完整答案**：使用 `ignore`、`globset`、`regex` 在 Rust 内实现，三平台行为可控，不要求用户安装 rg。遵守 gitignore、跳过系统目录/符号链接/二进制/大文件，限制文件数、总字节和结果，并定期检查取消。
- **源码映射**：`tools/search_text.rs`。
- **追问**：Unicode 列号如何算？按字符而非 UTF-8 字节。
- **常见误区**：递归 read_to_string 所有文件。
- **评分**：4 分须覆盖 ignore、资源限制和取消。

### TOOL-019：统一脱敏为何要同时处理文本和 JSON？【进阶｜秘密保护】
- **考察目标**：理解敏感数据形态。
- **完整答案**：文本中可能有 `sk-`、Bearer、私钥块；JSON 中敏感值常藏在 apiKey/token/password 字段。`redact_text` 扫常见模式，`redact_value` 按键递归处理；事件、错误、日志、持久化、审批摘要和模型注入前都复用。脱敏是降低泄漏，不替代最小读取原则。
- **源码映射**：`redaction.rs`、`credentials.rs::SecretString`。
- **追问**：为何不记录后再离线清洗？日志和模型请求已经泄漏。
- **常见误区**：只遮 API Key 配置字段。
- **评分**：4 分须说明多边界复用与局限。

### TOOL-020：怎样新增一个安全 Tool？【高级｜实战】
- **考察目标**：综合扩展流程。
- **完整答案**：定义稳定名称/描述/严格 Schema、最小 Permission 与准确 SideEffect；实现 validate、可选无副作用 preflight、取消检查、有界 I/O、稳定错误和脱敏结果；注册后补权限拒绝、审批拒绝、恶意输入、超时/取消、路径逃逸与成功测试。若已有专用能力，Prompt 应指向它并禁止 shell 绕过。
- **源码映射**：`tools/mod.rs::{Tool,register_builtins}` 与任一内置工具。
- **追问**：工具能直接持久化审批吗？不应，统一由 ApprovalGate 管理。
- **常见误区**：只实现 execute 和 happy path。
- **评分**：4 分须给完整接入与测试清单。

---

## 第六章：SQLite、会话与恢复（12 题）

> **知识类型：通用后端工程 + XDUDU 持久化。** SQLite、WAL、事务和 CAS 不属于 Agent 专有知识。

### 知识地图

本章考察会话为什么既是上下文来源也是审计记录，以及 SQLite 迁移、锁、WAL、CAS 和崩溃恢复如何共同工作。

### DB-001：Session 需要保存哪些事实？【初级｜领域模型】
- **考察目标**：理解可恢复会话并非文本数组。
- **完整答案**：除用户/助手消息，还需工作区、Provider/模型、状态、工具调用 ID/输入/结果/时序、上下文摘要、用量和 Plan 关联。持久化前脱敏，原始 reasoning 不进入公开展示。字段支持 resume、审计和终态解释。
- **源码映射**：`session.rs::{Session,Message,ToolCallRecord}`。
- **追问**：瞬时 ToolProgress 要保存吗？不需要，它不影响恢复事实。
- **常见误区**：只存最终答案即可恢复。
- **评分**：4 分须覆盖工具和状态记录。

### DB-002：为何选择 SQLite 而非 JSON 文件？【初级｜存储选型】
- **考察目标**：比较事务、查询和迁移能力。
- **完整答案**：SQLite 提供事务、索引、并发锁、Schema 迁移和 FTS，适合本机单用户且无需服务。JSON 易开始但跨记录原子更新、列表查询和崩溃恢复困难。代价是迁移和连接管理，仍比部署数据库服务更符合 CLI。
- **源码映射**：`sqlite_session.rs::SqliteSessionStore`、旧 JSON 导入逻辑。
- **追问**：何时换远程 DB？多设备同步或多租户服务化时。
- **常见误区**：SQLite 不能并发。
- **评分**：4 分须结合本地产品约束。

### DB-003：为何同时保存结构列和完整 JSON？【进阶｜存储模型】
- **考察目标**：理解查询与兼容折中。
- **完整答案**：状态、时间、revision 等常用字段单列便于索引和 CAS；完整领域 JSON 保留嵌套消息、步骤、attempt，减少表爆炸并支持版本迁移。写入时二者必须在同一事务保持一致，读取还需验证列与 JSON 的关键字段。
- **源码映射**：`sqlite_session.rs` 的 sessions/plans 表与序列化。
- **追问**：只存 JSON 的问题？状态筛选和原子 WHERE 更新困难。
- **常见误区**：重复存储必然错误。
- **评分**：4 分须说明事务一致性和查询收益。

### DB-004：WAL 与 busy_timeout 解决什么？【进阶｜并发】
- **考察目标**：理解 SQLite 锁行为。
- **完整答案**：WAL 允许读者与写者更好并行，提交追加日志；busy_timeout 遇短暂锁竞争先等待而非立即 `database is locked`。它们不替代应用级进程锁或事务，也不能支持无限并发写。
- **源码映射**：`sqlite_session.rs` 连接初始化 pragma。
- **追问**：WAL 文件可随意删吗？运行中不可，会破坏一致性。
- **常见误区**：开启 WAL 后无需事务。
- **评分**：4 分须说明读取并发与局限。

### DB-005：跨进程 workspace lock 为什么需要？【进阶｜进程协调】
- **考察目标**：理解 SQLite 锁之外的业务互斥。
- **完整答案**：多个 XDUDU 进程在同一工作区可能同时恢复 Session、执行 Plan 或写 change ledger；数据库锁只保护单事务，不保护跨工具的业务流程。workspace lock 明确阻止不支持的并发运行，并在异常退出后可恢复。
- **源码映射**：`sqlite_session.rs` 初始化与锁文件。
- **追问**：为何不把整个 Agent 放数据库事务？网络/工具运行很长，会长期占锁。
- **常见误区**：SQLite 已锁所以无需进程锁。
- **评分**：4 分须区分数据库锁与业务租约。

### DB-006：启动崩溃恢复如何处理 Running/Pending？【高级｜恢复】
- **考察目标**：掌握“不重放未知副作用”。
- **完整答案**：启动扫描不完整 Session/Plan：Pending/Running 工具调用标 Cancelled，Running attempt 标 Interrupted，当前步骤 Blocked，Plan Paused，Session Interrupted，并保存原因。恢复只展示现场，用户明确 retry 才重新执行；绝不把未知调用当未发生。
- **源码映射**：`sqlite_session.rs::recover_incomplete_sessions`、Plan 恢复逻辑。
- **追问**：只读调用可自动重放吗？原则上可讨论，但统一不重放更简单确定；当前实现保守。
- **常见误区**：重启后从当前行继续。
- **评分**：4 分须完整列状态变化和理由。

### DB-007：Schema migration 如何保证失败回滚？【高级｜迁移】
- **考察目标**：理解版本化持久化。
- **完整答案**：读取 schema version，在单个 SQLite 事务中建表/加列、转换旧 JSON、回填新字段和写迁移标记；任一损坏记录或校验失败整体 rollback，旧数据保持。领域类型还提供 v1/v2 到当前 Schema 的兼容解析。
- **源码映射**：`sqlite_session.rs` migration、`plan.rs` migration helpers。
- **追问**：为何不能逐条 commit？中途失败会得到混合版本数据库。
- **常见误区**：`ALTER TABLE` 成功就完成迁移。
- **评分**：4 分须说明事务、数据转换和版本标记。

### DB-008：删除 Session 时为何需要级联？【进阶｜关系完整性】
- **考察目标**：理解关联数据生命周期。
- **完整答案**：Plan revision、工具记录、记忆来源等可能引用 Session；外键 `ON DELETE CASCADE` 或显式同事务删除避免孤儿记录。删除前仍需产品策略确认哪些长期记忆独立保留，不能因技术级联误删用户知识。
- **源码映射**：`sqlite_session.rs` 表定义、`PlanStore` 关联。
- **追问**：MEMORY.md 如何处理？它是整理后的用户文件，不应随单会话直接删除。
- **常见误区**：删 sessions 表一行就结束。
- **评分**：4 分须区分审计关联与长期记忆。

### DB-009：Plan 的乐观并发 SQL 如何工作？【高级｜CAS】
- **考察目标**：理解 WHERE 条件即并发证明。
- **完整答案**：审批内容更新用 `revision + status`，运行检查点再加 `execution_version`；SQL `UPDATE ... WHERE id=? AND revision=? AND execution_version=? AND status=?`，受影响行数为 0 即 `PLAN_CONFLICT`。这样两个陈旧操作只有一个成功，无需跨网络持锁。
- **源码映射**：`plan.rs::PlanStore`、`sqlite_session.rs::checkpoint_plan_execution`。
- **追问**：冲突后自动覆盖可否？不可，应停止工具并重新读取现场。
- **常见误区**：先 SELECT 再无条件 UPDATE。
- **评分**：4 分须写出比较并交换语义。

### DB-010：FTS5 在记忆中承担什么？【进阶｜全文检索】
- **考察目标**：理解关键词召回而非向量语义。
- **完整答案**：原始记忆记录写 SQLite，并由 FTS5 按查询词召回候选；应用层再精排、去重和 token 限额，最多注入有限条。FTS 低成本、可解释、离线，语义同义召回有限，因此是否引向量 RAG 要用真实评测决定。
- **源码映射**：`memories.rs`、`sqlite_session.rs` memory FTS、`main.rs::rank_memories`。
- **追问**：FTS 结果能直接全注入吗？不能，需预算和相关性治理。
- **常见误区**：全文检索等于语义检索。
- **评分**：4 分须说明召回—精排—预算链。

### DB-011：为什么持久化前统一脱敏？【进阶｜隐私】
- **考察目标**：理解“本地数据库”也有泄漏面。
- **完整答案**：本地文件可能被备份、诊断、导出或其他账户读取；会话内容也会再次进入模型。Session、Plan、review reason、工具输入结果和记忆落盘前脱敏，凭据只在 Keyring。脱敏不可逆会影响完整审计，因此更重要的是源头不读取无关秘密。
- **源码映射**：`redaction.rs`、Store 写入路径、`credentials.rs`。
- **追问**：数据库加密能替代脱敏？不能，解密后仍可能被输出或发给模型。
- **常见误区**：本地保存等于绝对安全。
- **评分**：4 分须覆盖备份、导出和再注入风险。

### DB-012：如何测试数据库迁移和恢复？【高级｜测试】
- **考察目标**：设计破坏性场景验证。
- **完整答案**：用临时目录构造每个旧 schema 和合法/损坏 JSON，升级后断言数据、revision、外键和 user_version；注入中途失败验证整体回滚；创建 Running/Pending 状态重开 Store，断言 Interrupted/Paused 且无工具重放；并发两个 CAS 只能一个成功。
- **源码映射**：`sqlite_session.rs` 测试、Plan/CLI 集成测试。
- **追问**：只测全新数据库够吗？不够，真实用户最危险的是升级路径。
- **常见误区**：迁移 SQL 能执行即通过。
- **评分**：4 分须包含旧版、损坏、并发和恢复。

---

## 第七章：Plan、DAG 与子代理（20 题）

> **知识类型：Agent 编排 + 算法 + XDUDU 工程。** Plan/task_graph 是项目领域模型，Kahn 是通用图算法。

### 知识地图

普通 ReAct 解决单次闭环，持久化 Plan 解决跨步骤审批和恢复，`task`/`task_graph` 解决一次运行内的隔离探索与短期并发。三者不能混为同一状态机。

### PLAN-001：ReAct、Plan、task 三者区别？【初级｜编排】
- **考察目标**：建立三层执行模型。
- **完整答案**：ReAct 是单次请求内部模型—工具循环；Plan 是用户审批、SQLite 持久化、可暂停恢复的长任务协议；task 是父 Agent 一轮内启动隔离子代理并返回有界报告。Plan 可跨进程，task 不承诺长期恢复，批准 Plan 也不等于批准具体工具。
- **源码映射**：`agent.rs`、`plan_executor.rs`、`subagent.rs`。
- **追问**：复杂任务都应自动 Plan 吗？当前只通过显式 `/plan`，避免误判和额外审批。
- **常见误区**：模型输出列表就是 Plan。
- **评分**：4 分须说明生命周期、持久化和授权差异。

### PLAN-002：Plan 为什么有严格状态机？【初级｜领域状态】
- **考察目标**：理解非法迁移防护。
- **完整答案**：Draft、PendingApproval、Approved、Running、Paused、Completed/Failed/Rejected/Cancelled 各有合法入口；例如只有 PendingApproval 可审批，Approved 才首次运行，Paused 才 retry。状态方法集中校验并设置时间/原因，避免 CLI、Store、Executor 各自随意改字符串。
- **源码映射**：`plan.rs::{PlanStatus,Plan::transition_to}`。
- **追问**：Ctrl+C 应 Failed 吗？不，应 Paused/Interrupted，允许用户检查现场。
- **常见误区**：Running 可直接编辑步骤。
- **评分**：4 分须列关键迁移和失败语义。

### PLAN-003：revision 与 execution_version 为何分开？【进阶｜版本控制】
- **考察目标**：理解内容与运行两类并发。
- **完整答案**：revision 表示审批前计划内容版本；每次自然语言修订生成新快照并重新审批。execution_version 表示同一已批准内容的运行检查点，每次步骤/attempt 更新递增。分开后内容审阅不会与运行进度混淆，SQL 可分别阻止陈旧审批和陈旧执行器覆盖。
- **源码映射**：`plan.rs::Plan`、`PlanRevision`、`sqlite_session.rs`。
- **追问**：运行后能修订 revision 吗？当前不允许，应取消并建新 Plan。
- **常见误区**：一个 updated_at 足够并发控制。
- **评分**：4 分须说明两个 CAS 维度。

### PLAN-004：submit_plan 协议为何要求 ToolCalls 结束？【进阶｜结构化协议】
- **考察目标**：理解严格生成边界。
- **完整答案**：规划 Provider 只可调用一次内部 `submit_plan`，必须 `FinishReason::ToolCalls`，不接受普通 Markdown、多个调用或其他工具。DTO 再校验步骤数、文字长度、完成条件、依赖存在和 DAG。这样数据库接收的是机器可验证对象，不靠解析自然语言列表。
- **源码映射**：`plan_generation.rs`。
- **追问**：模型同时解释计划为何拒绝？协议必须单一明确，展示由 CLI 根据 Plan 生成。
- **常见误区**：从代码块提取 JSON 即可。
- **评分**：4 分须覆盖 FinishReason、次数和语义校验。

### PLAN-005：整份 Plan 审批为何不复用 Tool Approval？【进阶｜授权语义】
- **考察目标**：区分认可方案与授予副作用。
- **完整答案**：Plan 审批表示用户认可目标和步骤，生成 review history；Tool Approval 表示允许一次具体文件/进程/网络副作用。前者不能创建 Allow always，也不放行后续工具；执行每一步仍走 ToolRegistry。合并两者会让抽象计划获得过宽授权。
- **源码映射**：`plan_review.rs`、`approval.rs`、`plan_executor.rs`。
- **追问**：为何整份而非逐步审批？产品锁定整份方案，具体副作用仍逐次授权。
- **常见误区**：批准计划即自动 full-access。
- **评分**：4 分须说明两个授权对象。

### PLAN-006：自然语言修订如何保持原计划安全？【高级｜事务修订】
- **考察目标**：理解失败不改变当前版本。
- **完整答案**：读取 PendingApproval 当前 revision，将目标/步骤和用户要求交给隔离 `revise_plan` 协议；仅在单次合法调用、完整 DAG 校验通过后，原子追加 revision 快照并更新当前 Plan。Provider 错误、截断或冲突时数据库完全不变；ID、Session、created_at 保持，新步骤可换 UUID。
- **源码映射**：`plan_review.rs::revise_plan`、`PlanStore::append_revision_if_current`。
- **追问**：为何修订后重新审批？用户批准的是具体 revision。
- **常见误区**：边生成边修改旧步骤。
- **评分**：4 分须说明隔离生成、原子 CAS 和不变字段。

### PLAN-007：PlanStep 的完成条件为何必须结构化？【初级｜验收】
- **考察目标**：理解可验证完成。
- **完整答案**：步骤标题描述动作，completion criteria 描述可判定结果。执行器要求 `complete_step` 为每个条件提供唯一索引和证据，阻止模型仅说“完成”。条件应面向测试、文件差异或明确观察，不能写“效果良好”这类无法验证表述。
- **源码映射**：`plan.rs::PlanStep`、`plan_executor.rs` 完成协议。
- **追问**：证据是自动证明吗？它是结构化声明，执行器还检查未处理工具失败；更强系统可加入外部 verifier。
- **常见误区**：summary 可替代逐项证据。
- **评分**：4 分须说明条件索引和事实来源。

### PLAN-008：PlanStepAttempt 为什么单独建模？【进阶｜审计】
- **考察目标**：理解重试历史不可覆盖。
- **完整答案**：每次执行生成独立 attempt ID、序号、状态、起止时间、summary、evidence、error 和 tool_call_ids。Paused retry 创建新 attempt，旧失败保留，已完成步骤不重复。这样能重建“做过什么”和判断未知副作用。
- **源码映射**：`plan.rs::{PlanStepAttempt,AttemptStatus}`。
- **追问**：为何不只在 Step 上放 last_error？会丢重试与工具审计链。
- **常见误区**：重试时清空旧 attempt。
- **评分**：4 分须解释历史、恢复和关联调用。

### PLAN-009：complete_step 如何阻止口头完成？【进阶｜协议】
- **考察目标**：掌握完成协议。
- **完整答案**：它只在步骤执行 Provider 请求中提供，必须单独调用，不能与真实工具同批；evidence 索引必须唯一、无越界且覆盖全部条件。存在未处理工具失败、普通 Stop、多个调用或缺证据时，步骤 Failed/Blocked，Plan Paused。
- **源码映射**：`plan_executor.rs` 的 complete_step DTO 和验证。
- **追问**：它授予工具权限吗？完全不授予。
- **常见误区**：最终文本含“完成”即可更新状态。
- **评分**：4 分须列至少四种拒绝情况。

### PLAN-010：Plan DAG 如何选择下一个步骤？【进阶｜DAG】
- **考察目标**：理解依赖解锁和确定性。
- **完整答案**：验证依赖存在且无环；完成节点后，所有依赖已 Completed 的 Pending 节点转 Ready；执行器按 Plan 原始顺序选第一个 Ready，因而当前实现是串行但支持依赖分支。确定顺序简化副作用和恢复。
- **源码映射**：`plan.rs` DAG 校验、`plan_executor.rs` Ready 选择。
- **追问**：为何不并行 Ready 步骤？长期 Plan 副作用复杂，当前锁定串行更可控。
- **常见误区**：DAG 必须并行才有意义。
- **评分**：4 分须说明解锁条件与稳定顺序。

### PLAN-011：Plan 检查点为何同时更新 Session？【高级｜原子性】
- **考察目标**：避免两个真相源分叉。
- **完整答案**：Plan 状态和 Session 状态对用户恢复必须一致，例如 Running/Paused/Completed。`checkpoint_plan_execution` 在单个 SQLite 事务中以 revision、execution_version、status 条件更新二者；CAS 失败立刻停止工具，不能让 Session 显示完成而 Plan 仍运行。
- **源码映射**：`PlanStore::checkpoint_plan_execution`、`sqlite_session.rs`。
- **追问**：先更 Plan 再更 Session 可否？崩溃间隙会产生矛盾状态。
- **常见误区**：UI 可临时推断并修复数据库。
- **评分**：4 分须说明事务与冲突停止。

### PLAN-012：子代理为什么需要 AgentProfile？【初级｜角色隔离】
- **考察目标**：理解能力和 Prompt 受控组合。
- **完整答案**：build/plan/explore/general/reviewer 档案定义模式、工具范围、权限上限和系统指令。子代理权限取父级与 Profile 更严格者，不能升级；报告长度和轮次有界。Profile 比自由文本“你是专家”更可验证、可审计。
- **源码映射**：`subagent.rs::{AgentProfile,builtin_profiles,more_restrictive}`。
- **追问**：自定义 Profile 能扩大父权限？不能。
- **常见误区**：Profile 只改变人设。
- **评分**：4 分须说明工具、权限和报告边界。

### PLAN-013：子代理上下文为何隔离？【进阶｜上下文管理】
- **考察目标**：理解污染与 token 成本。
- **完整答案**：子代理收到任务、Profile、工作区和必要依赖结果，不自动继承父会话全部消息；运行结果被压缩成有界报告返回父 Agent。隔离减少无关历史、提示注入扩散和 token 消耗，也避免子代理直接改父 Session 的对话顺序。
- **源码映射**：`subagent.rs::{SubagentContext,run_subagent,build_subagent_system}`。
- **追问**：何时传父摘要？任务确需上下文时传脱敏有界摘要。
- **常见误区**：复制全部父上下文最可靠。
- **评分**：4 分须解释输入最小化和结果汇总。

### PLAN-014：task_graph 的 Schema 应校验什么？【进阶｜图协议】
- **考察目标**：理解执行前完整预检。
- **完整答案**：节点 ID 唯一、数量与长度上限、Profile 存在、依赖 ID 存在且不自依赖、无环、并发上限、失败策略合法；全部通过才启动任一子代理。否则半图已执行会产生难审计副作用。
- **源码映射**：`subagent_graph.rs` 的 DTO 与 validate。
- **追问**：为何先全图校验？保证零节点执行的失败原子性。
- **常见误区**：运行到依赖不存在再报错。
- **评分**：4 分须列结构和 DAG 两类校验。

### PLAN-015：Kahn 算法如何发现环？【进阶｜算法】
- **考察目标**：能白板解释 DAG 校验。
- **完整答案**：计算每节点入度，把入度 0 入队；弹出节点并减少后继入度，新 0 入队；最终处理数等于节点数则无环，否则剩余节点属于环或受环阻塞。复杂度 O(V+E)，还能为调度生成拓扑关系。
- **源码映射**：`subagent_graph.rs` DAG validation。
- **追问**：DFS 可否？可以，颜色标记也为 O(V+E)，Kahn 更贴近 Ready 调度。
- **常见误区**：只检查直接互相依赖。
- **评分**：4 分须给入度过程与复杂度。

### PLAN-016：任务图如何传递依赖结果？【进阶｜数据流】
- **考察目标**：理解下游上下文构造。
- **完整答案**：每节点完成后保存有界、脱敏报告；后继启动时按声明依赖顺序拼接这些报告与自身任务，而非共享可变消息列表。失败依赖按策略将后继标 Blocked 或允许带失败摘要继续，不能伪装成成功输入。
- **源码映射**：`subagent_graph.rs` reports 与 prompt 构造。
- **追问**：为何不传原始工具日志？过大、敏感且会污染上下文。
- **常见误区**：依赖仅控制顺序，不传信息。
- **评分**：4 分须说明确定顺序、限长和失败标记。

### PLAN-017：任务图为何让副作用节点独占？【高级｜调度安全】
- **考察目标**：理解图并行中的外部冲突。
- **完整答案**：多个只读 explore 可受 max_concurrency 并行；含写/进程等副作用的 Profile 或节点不与其他节点并发，等待运行集清空后独占。这样避免两个子代理同时改相同文件或审批交错。更高并发需引入资源锁/写集声明，目前不值得复杂化。
- **源码映射**：`subagent_graph.rs` 调度选择、`subagent.rs::ProfileMode`。
- **追问**：两个写不同文件可否并行？理论可，但需可靠预声明写集和冲突处理，当前未知。
- **常见误区**：DAG 无依赖即都安全并行。
- **评分**：4 分须说明未知写集和独占策略。

### PLAN-018：fail-fast 与 continue 如何传播失败？【高级｜失败策略】
- **考察目标**：推演图失败。
- **完整答案**：fail-fast 在节点失败后取消运行节点并阻止新节点，待结果收集后输出图报告；continue 则将直接或传递依赖失败的节点标 Blocked，独立分支仍运行。无论哪种，父 Agent 都收到每节点状态，不能只返回成功报告。
- **源码映射**：`subagent_graph.rs` failure policy 与状态传播。
- **追问**：取消中的节点记 Failed 吗？应记 Cancelled/Interrupted，区分自身失败。
- **常见误区**：continue 会让依赖失败的后继照常运行。
- **评分**：4 分须区分独立分支、后继和运行中节点。

### PLAN-019：Plan DAG 与 task_graph 为何不合并？【高级｜领域边界】
- **考察目标**：比较持久图和短期图。
- **完整答案**：Plan 有用户审批、revision、execution_version、attempt、SQLite 检查点和跨进程恢复，且当前串行；task_graph 是父 Agent 一轮内的短期并发探索，完成后返回报告。合并会让短期调研承担昂贵审批/持久化，或让长期副作用失去恢复语义。可共享 DAG 校验思想，不应共享同一状态对象。
- **源码映射**：`plan.rs`/`plan_executor.rs` 与 `subagent_graph.rs`。
- **追问**：未来能否 Plan 步骤内部调用 task_graph？可以，但子图报告只是该步骤证据之一，权限不升级。
- **常见误区**：都是 DAG 就应一个调度器。
- **评分**：4 分须从生命周期和一致性解释。

### PLAN-020：如何测试任务图调度器？【高级｜测试】
- **考察目标**：设计确定性并发测试。
- **完整答案**：用 Mock 子代理记录开始/完成时间和输入，覆盖线性、多分支、多级依赖、环、未知依赖、并发上限、副作用独占、fail-fast/continue、取消和报告限长；使用 barrier 控制完成顺序，断言结果仍按节点索引稳定。测试不得依赖真实模型时序。
- **源码映射**：`subagent_graph.rs` tests、`subagent.rs` MockProvider。
- **追问**：只断言最终成功够吗？不够，错误并发顺序可能偶然成功。
- **常见误区**：用 sleep 猜并发顺序。
- **评分**：4 分须覆盖顺序、上限、失败和确定性。

---

## 第八章：Skills、Instructions 与 Memory（12 题）

> **知识类型：Agent 上下文工程。** FTS5 和原子文件写入属于通用工程，Skill/Memory 管线属于 Agent 产品设计。

### 知识地图

三者都进入模型上下文，但来源、生命周期和信任不同：Instructions 是约定，Skills 是按需工作流，Memory 是 Agent 提炼且用户可审查的长期知识。

### MEM-001：Instructions 的作用是什么？【初级｜仓库约定】
- **考察目标**：区分用户要求与持久项目规则。
- **完整答案**：加载用户级指令和项目 `AGENTS.md`、`CLAUDE.md` 等约定，渲染进系统上下文，让模型知道测试、风格和流程。它们仍是软规则，不能覆盖系统安全和 ToolRegistry；读取需有文件数/大小上限和来源标记。
- **源码映射**：`instructions.rs::{load_instructions,render_instructions}`。
- **追问**：仓库指令能授权网络吗？不能。
- **常见误区**：Instructions 是可执行脚本。
- **评分**：4 分须说明优先级与硬边界。

### MEM-002：指令优先级冲突如何处理？【进阶｜配置层级】
- **考察目标**：理解不同信任来源。
- **完整答案**：系统规则最高；当前用户请求高于一般项目偏好；用户级和项目级指令按明确层级加载，较具体规则可覆盖一般风格，但不能扩大权限。冲突应向模型标注来源，关键歧义由用户决定，不能靠文件遍历偶然顺序。
- **源码映射**：`instructions.rs::InstructionSource` 与加载顺序、`prompt.rs`。
- **追问**：多个嵌套目录规则？当前按实现支持的仓库约定读取，不应虚构无限层级。
- **常见误区**：最后读到的文件总是最高权威。
- **评分**：4 分须区分偏好覆盖与安全不可覆盖。

### MEM-003：Skill 与普通 Prompt 模板有何不同？【初级｜Skills】
- **考察目标**：理解按需能力说明。
- **完整答案**：Skill 有名称、描述、来源和正文，先只把轻量索引给模型，选中后通过内部 skill 工具加载完整工作流，降低常驻 token。它主要提供知识和步骤，不直接授予文件/网络能力，执行仍走现有 Tool。
- **源码映射**：`skills.rs::Skill`、`tools/skill.rs`、`agent.rs` SkillLoaded。
- **追问**：Skill 可包含脚本吗？即使引用脚本，运行也必须通过受控工具。
- **常见误区**：加载 Skill 等于安装插件。
- **评分**：4 分须说明渐进加载和权限不升级。

### MEM-004：六级 Skill 发现如何避免同名混乱？【进阶｜发现】
- **考察目标**：理解兼容目录和优先级。
- **完整答案**：XDUDU 扫描项目/用户下 `.xdudu`、`.claude`、`.opencode` 对应目录，解析受限 frontmatter、校验名称并按既定层级去重；警告损坏项但不让单个 Skill 阻断启动。最终来源保留用于展示和审计。
- **源码映射**：`skills.rs::{discover_skills,load_layer,parse_frontmatter}`。
- **追问**：为何兼容其他目录？降低迁移成本，但不承诺执行其私有扩展协议。
- **常见误区**：任意 Markdown 都自动成为 Skill。
- **评分**：4 分须说明层级、校验和错误隔离。

### MEM-005：XDUDU 长短期记忆如何区分？【进阶｜Memory】
- **考察目标**：理解三层上下文。
- **完整答案**：短期是当前 Session 最近消息；中期是超预算历史的 context summary；长期是跨会话可复用偏好/项目事实。原始长期提炼记录存 SQLite 供审计，后台合并去重为 `.xdudu/memories/MEMORY.md`，运行时优先注入有界汇总。
- **源码映射**：`agent.rs` context summary、`memories.rs`、`memory_suggestion.rs`、`main.rs::relevant_memories`。
- **追问**：长期记忆为何不是全部历史？历史含一次性细节和噪声，需提炼。
- **常见误区**：context summary 就是长期 Memory。
- **评分**：4 分须清楚区分生命周期和存储。

### MEM-006：为什么记忆提炼由 Agent 自动进行？【进阶｜自治】
- **考察目标**：理解不中断对话的记忆策略。
- **完整答案**：任务结束后后台使用隔离 `suggest_memories` 协议，从脱敏、有界会话上下文提取稳定偏好、长期项目事实和约束；不每条弹审批，避免破坏流畅性。用户通过 `/memory` 和 CLI 查看、编辑、删除，自动化负责建议，最终控制权仍在用户。
- **源码映射**：`main.rs::{spawn_auto_capture_memories,capture_memories}`、`memory_suggestion.rs::suggest_memories`。
- **追问**：何时不应记？临时问题、秘密、推测和低置信度信息。
- **常见误区**：模型说重要就永久保存。
- **评分**：4 分须说明后台、隔离、审查入口。

### MEM-007：为什么原始记录与 MEMORY.md 两阶段？【高级｜合并】
- **考察目标**：理解审计层与服务层。
- **完整答案**：SQLite 原始记录保留来源、时间和细粒度提炼，便于审计；直接逐条注入会重复和膨胀。`consolidate_memory_document` 用严格内部协议把现有文档与候选合并、去重、压缩为一个用户可编辑文件；写入原子、限 32KiB、拒绝 symlink 并设私有权限。
- **源码映射**：`memory_suggestion.rs::consolidate_memory_document`、`memories.rs` MEMORY.md I/O。
- **追问**：整理失败怎么办？保留旧 MEMORY.md 与原始记录，不阻断主任务。
- **常见误区**：生成新文档后删除所有原始记录。
- **评分**：4 分须说明失败回退和文件安全。

### MEM-008：记忆注入为何要 FTS→精排→去重→预算？【进阶｜召回】
- **考察目标**：理解检索管线。
- **完整答案**：FTS5 先低成本召回候选，应用层用当前用户问题加最近助手文本分词精排，规范化去重，再按 top_k、token budget 和最多条数截断。优先 MEMORY.md 时同样有长度上限。该管线防止记忆占满窗口并降低错误偏好影响。
- **源码映射**：`sqlite_session.rs` FTS、`main.rs::{query_tokens,rank_memories,relevant_memories}`。
- **追问**：为何拼最近助手文本？用户短追问可能省略主题。
- **常见误区**：按创建时间取最近 20 条。
- **评分**：4 分须完整复述召回和预算链。

### MEM-009：记忆污染有哪些来源，如何治理？【高级｜安全】
- **考察目标**：识别长期提示注入风险。
- **完整答案**：网页/MCP 内容、模型误推断、一次性用户表达和秘密都可能被错误记住。提炼 Prompt 限定类别，输入/输出脱敏，DTO 限长，合并去重；运行时将记忆标为背景信息而非权限，用户可 edit/path/list 审查。高风险决策不能只凭记忆。
- **源码映射**：`memory_suggestion.rs`、`redaction.rs`、CLI MemoryCommand。
- **追问**：用户编辑的 MEMORY.md 是否绝对可信？是用户控制内容，但仍不能覆盖系统安全。
- **常见误区**：本地记忆不受提示注入影响。
- **评分**：4 分须覆盖产生、存储、注入三阶段。

### MEM-010：为何当前不引入向量 RAG？【高级｜技术选型】
- **考察目标**：基于证据做架构决策。
- **完整答案**：当前语料主要是短偏好和项目事实，FTS+合并文档成本低、离线、可解释；没有真实评测集证明 embedding/向量库提升召回。向量 RAG 会增加模型、索引迁移、隐私和部署成本。先建立查询集和指标，只有同义语义召回明显不足才引入。
- **源码映射**：`TASKS.md` M9-T05/T06、`memories.rs`。
- **追问**：若引入如何隔离？可先做本地或 Python MCP，仍受权限和数据边界。
- **常见误区**：Agent 有 Memory 就必须向量库。
- **评分**：4 分须说明评测前置和成本。

### MEM-011：MEMORY.md 用户编辑如何保证安全？【进阶｜文件安全】
- **考察目标**：理解可审查入口的实现细节。
- **完整答案**：`memory path` 显示明确位置，`memory edit` 调用用户 `$VISUAL/$EDITOR`；程序读取/写入时拒绝 symlink、限制 UTF-8/大小、原子替换、脱敏并设置 0600。编辑器是用户主动进程，不把文件内容放命令参数。
- **源码映射**：`memories.rs`、`main.rs::handle_memory`。
- **追问**：为何拒绝 symlink？防项目文件把记忆写到工作区外敏感位置。
- **常见误区**：隐藏文件无需权限保护。
- **评分**：4 分须覆盖路径、原子性和编辑器调用。

### MEM-012：如何评测记忆质量？【高级｜Eval】
- **考察目标**：建立召回与污染指标。
- **完整答案**：构造带期望记忆的跨会话查询集，测 precision@k、recall@k、重复率、过期事实率、token 占比和答案增益；加入秘密、网页注入、否定偏好、修改/删除等安全集。对比无记忆、FTS、MEMORY.md、候选向量方案，并人工审查错误召回影响。
- **源码映射**：现有 memory 单元测试；`TASKS.md` RAG 评审门槛。
- **追问**：只测召回率够吗？不够，错误记忆的伤害可能大于漏召回。
- **常见误区**：能搜到一条就算有效。
- **评分**：4 分须包含准确率、污染、安全和答案增益。

---

## 第九章：MCP、插件与 Web（12 题）

> **知识类型：扩展协议 + 网络安全。** MCP 是开放协议，SSRF/DNS 防护是通用安全，McpTool 是 XDUDU 适配层。

### 知识地图

考察外部能力如何在不破坏本地安全链的前提下接入，以及网络工具如何防 SSRF、DNS 重绑定和无限响应。

### EXT-001：MCP 在 XDUDU 中解决什么？【初级｜MCP】
- **考察目标**：理解工具互操作协议。
- **完整答案**：MCP Server 通过 initialize、tools/list、tools/call 暴露动态工具，XDUDU 把定义适配成 `McpTool`，模型无需知道传输细节。它让 Python 或外部系统独立演进，但不是插件任意进程内执行，也不绕过本地安全策略。
- **源码映射**：`mcp.rs::{McpServerRuntime,McpTool}`。
- **追问**：MCP 会管理 Agent 状态吗？当前只作为工具能力来源。
- **常见误区**：接入 MCP 后直接信任服务返回。
- **评分**：4 分须说明协议、适配和安全链。

### EXT-002：stdio MCP 如何管理生命周期？【进阶｜子进程】
- **考察目标**：理解协议流和 stderr 隔离。
- **完整答案**：配置 command/args 直接 spawn，不经 shell；stdin/stdout 传逐行 JSON-RPC，stderr 作为日志独立处理，避免污染协议。请求 ID 关联响应，超时/取消后结束子进程，防孤儿；环境变量按白名单和凭据引用构造。
- **源码映射**：`mcp.rs::{spawn_stdio,write_stdio,stdio_request}`。
- **追问**：为何不能把 stderr 合并 stdout？日志会被误解析为 JSON-RPC。
- **常见误区**：用 `sh -c` 拼配置命令。
- **评分**：4 分须覆盖 argv、ID、stderr、取消。

### EXT-003：Streamable HTTP MCP 的会话如何工作？【进阶｜HTTP MCP】
- **考察目标**：理解请求、SSE 和 session ID。
- **完整答案**：Client 对受控 URL POST JSON-RPC，解析 JSON 或 SSE 响应，从响应头捕获 MCP session ID 并在后续请求携带；notification 与 request 分开处理。Body 有上限、支持取消/超时，鉴权 Secret 不进入普通配置。
- **源码映射**：`mcp.rs::{build_http,http_request,parse_sse_response,capture_session_id}`。
- **追问**：为何允许 localhost HTTP？仅开发测试例外，公网默认 HTTPS。
- **常见误区**：MCP HTTP 就是普通 REST GET。
- **评分**：4 分须说明 JSON-RPC、SSE、session ID。

### EXT-004：动态 MCP Tool 名称如何避免冲突？【进阶｜命名空间】
- **考察目标**：理解动态注册稳定性。
- **完整答案**：服务名和工具名先校验/规范化，再组合命名空间，防止覆盖内置工具或另一 Server；ToolDefinition 使用拥有所有权的 String 支持运行时发现。注册报告记录来源和失败，单个坏工具不应静默替代现有定义。
- **源码映射**：`mcp.rs::{sanitize_tool_component,register_configured_mcp_tools}`、`tools/mod.rs`。
- **追问**：为何不只用远端工具名？多 Server 很容易同名。
- **常见误区**：后注册覆盖前注册最方便。
- **评分**：4 分须说明命名空间和冲突失败。

### EXT-005：MCP 工具输入为何还要本地 Schema 校验？【进阶｜零信任】
- **考察目标**：理解远端定义不等于可信执行。
- **完整答案**：模型输入可能不符远端 Schema，本地 `validate_schema_input` 在发送前检查基本类型、required 等，避免无意义网络/进程调用；ToolRegistry 先做权限和审批。远端仍需自行验证，因为本地校验不是服务安全证明。
- **源码映射**：`mcp.rs::validate_schema_input`、`McpTool::validate`。
- **追问**：为何不完整实现所有 JSON Schema？当前支持受控子集，未知特性应明确拒绝或交给服务，不应假装验证。
- **常见误区**：Server 提供 Schema 即可信。
- **评分**：4 分须说明双端校验。

### EXT-006：MCP 如何进入统一审批链？【高级｜安全整合】
- **考察目标**：沿调用路径证明无法绕权。
- **完整答案**：发现的 `McpTool` 注册 ToolRegistry；stdio 定义 FullAccess+ProcessExecution，HTTP 定义 FullAccess+NetworkAccess。执行顺序仍是 Permission、validate、Approval、timeout/cancel、MCP call、脱敏结果、Agent Observation。审批拒绝时请求绝不发送到 Server。
- **源码映射**：`mcp.rs::impl Tool for McpTool`、`tools/mod.rs`。
- **追问**：远端说 readOnlyHint 是否降权？不能自动覆盖本地保守分类。
- **常见误区**：MCP 自带权限系统所以无需本地审批。
- **评分**：4 分须完整复述链路。

### EXT-007：声明式插件为何不加载动态代码？【高级｜插件隔离】
- **考察目标**：理解供应链和 ABI 风险。
- **完整答案**：插件清单只声明 MCP Server、元数据和启用状态；实际代码在独立进程/服务中运行。这样崩溃、语言运行时和依赖与 XDUDU 进程隔离，权限仍由 MCP Tool 管理；代价是 IPC 延迟和协议限制，但避免 Rust ABI、任意 dylib 与进程内秘密访问。
- **源码映射**：`plugin.rs`、`docs/M8_MCP_PLUGIN_DESIGN.md`。
- **追问**：签名元数据是否等于已验证签名？必须看当前实现，不能把声明字段夸大成完整信任链。
- **常见误区**：插件启用即授予 full access。
- **评分**：4 分须说明进程隔离和权限不升级。

### EXT-008：web_fetch 的 SSRF 防御链是什么？【高级｜SSRF】
- **考察目标**：掌握网络安全核心。
- **完整答案**：只允许 HTTPS；解析 DNS 后要求所有地址为公网，拒绝回环、私网、链路本地、保留、多播和映射地址；每次重定向重新校验；用已验证地址固定连接，SNI/证书仍用原域名，防 DNS 重绑定。禁代理、Cookie、自定义头和认证。
- **源码映射**：`tools/web_fetch.rs::{validate_url,public_ip,pinned_client}`。
- **追问**：公网与私网混合 DNS 可选公网吗？应整体拒绝，避免负载选择绕过。
- **常见误区**：只过滤 localhost 字符串。
- **评分**：4 分须覆盖 DNS、重定向、固定解析和 TLS。

### EXT-009：为什么代理设置会影响 DNS，项目如何权衡？【高级｜网络环境】
- **考察目标**：理解安全与可用性冲突。
- **完整答案**：透明代理可能把公网域名解析到 198.18/保留或内部地址；严格生产策略会拒绝，造成正常网站不可用。XDUDU 的目标是禁环境代理避免凭据/流量被静默转发，并通过受控解析处理公网；不能为可用性加入“允许所有私网”开关。若支持显式代理，必须作为新的信任配置和审批面设计。
- **源码映射**：`web_fetch.rs` Client 构建和公网 IP 判定。
- **追问**：198.18/15 是公网吗？是基准测试保留段，不应当普通公网。
- **常见误区**：DNS 能返回就安全。
- **评分**：4 分须阐明保留地址和显式信任方案。

### EXT-010：Web 响应为何必须有界？【进阶｜资源治理】
- **考察目标**：理解流式读取的 DoS 风险。
- **完整答案**：Content-Length 可缺失或伪造，必须逐 chunk 累计并在上限停止，检查取消和时间。HTML/text 可返回截断标记；JSON 若截断会无效，应报 `RESPONSE_TOO_LARGE` 而非解析半 JSON。只接受允许 MIME，HTML 去 script/style 等再抽正文。
- **源码映射**：`web_fetch.rs` body loop 与内容处理。
- **追问**：为何不先 response.bytes()？可能把无限响应读进内存。
- **常见误区**：timeout 能替代字节上限。
- **评分**：4 分须说明 streaming、MIME 和 JSON 特例。

### EXT-011：web_search、web_fetch、web_read 如何分工？【进阶｜检索链】
- **考察目标**：理解候选发现、精读和长文提炼。
- **完整答案**：web_search 返回有界标题/HTTPS 链接/摘要；web_fetch 读取单页并抽正文/JSON；web_read 对大型页面分块采样，用隔离 LLM 协议提炼与问题相关内容。三者都 NetworkAccess 审批，不能把搜索摘要当最终来源。
- **源码映射**：`web_search.rs`、`web_fetch.rs`、`web_read.rs`。
- **追问**：web_read 为何不直接扩大 maxBytes？会增加内存和上下文，分块提炼更有界。
- **常见误区**：一次 search 就完成事实核验。
- **评分**：4 分须说明逐层用途与共同边界。

### EXT-012：如何测试恶意 MCP/Web 输入？【高级｜安全测试】
- **考察目标**：设计边界攻击集。
- **完整答案**：本地模拟 stdio/HTTP Server 覆盖错 ID、畸形 JSON/SSE、超大 body、超时、取消、stderr 噪声、恶意 Schema、审批拒绝零请求；Web 覆盖 IPv4/IPv6 私网、混合 DNS、重定向私网、重绑定、危险 MIME、截断 JSON。生产构造器不得提供私网放行参数。
- **源码映射**：`mcp.rs` tests、`web_fetch.rs` tests、集成安全测试。
- **追问**：测试 localhost 与生产拒绝矛盾吗？测试通过注入模拟解析器/策略，生产 API 保持封闭。
- **常见误区**：只测 example.com 成功。
- **评分**：4 分须覆盖协议、资源、网络和审批。

---

## 第十章：TUI 与事件系统（8 题）

> **知识类型：Rust 异步 + 终端产品工程。** EventSink 是架构边界，ANSI、输入和布局是 CLI/TUI 实现。

### 知识地图

TUI 不参与业务判断，但必须正确处理增量输出、终端 resize、多行输入、队列、取消和审批。核心原则是已完成历史不可变，活动区可重绘。

### TUI-001：为什么 Agent 不能直接 println？【初级｜解耦】
- **考察目标**：理解输出架构。
- **完整答案**：直接打印会污染 JSON Lines、破坏光标、难测试且不能复用 GUI。Agent 只发 `AgentEvent`；ConsoleRenderer、TuiRenderer 和测试 Sink 分别消费。事件内容在 core 脱敏，颜色与布局留 CLI。
- **源码映射**：`events.rs::EventSink`、`renderer.rs`、`tui.rs::TuiRenderer`。
- **追问**：错误是否例外直接 eprintln？运行期应走事件/统一边界，启动致命错误由 CLI 处理。
- **常见误区**：TUI 只是给 println 加颜色。
- **评分**：4 分须说明 JSON、测试和复用。

### TUI-002：Transcript 与活动区为何分离？【进阶｜渲染】
- **考察目标**：理解长输出和复制问题。
- **完整答案**：用户/助手完成消息和工具摘要一旦提交就写入 transcript，不再每帧重画；当前 token、工具进度、输入框属于活动区，可局部清除更新。若全屏每 token 重绘，终端原生滚动、选择复制和长历史都会丢失或闪烁。
- **源码映射**：`tui.rs::{TranscriptBlock,ToolActivity,push_block}`。
- **追问**：完成工具参数默认全展示吗？摘要入历史，详情按需查看并脱敏。
- **常见误区**：保留 200 个 UI 块等于完整会话历史。
- **评分**：4 分须说明不可变与瞬态边界。

### TUI-003：单一异步 TUI 循环解决什么？【进阶｜事件循环】
- **考察目标**：理解运行时仍可输入和取消。
- **完整答案**：`tokio::select!` 同时处理终端事件、AgentEvent、运行完成、tick/resize；Agent 运行时用户仍可编辑并把消息入队，Ctrl+C 取消当前令牌而不丢队列。普通消息顺序发送，不能并发写同一 Session；UI 命令可在安全范围即时处理。
- **源码映射**：`main.rs::tui_interactive_loop`、`handle_tui_running_event`、`input_queue.rs`。
- **追问**：为何不用 Agent 期间停读 stdin？会造成不可中断和长任务体验差。
- **常见误区**：并发接收意味着并发执行所有消息。
- **评分**：4 分须说明输入、队列、取消和 Session 串行。

### TUI-004：终端 Resize 为什么难？【进阶｜布局】
- **考察目标**：理解状态与视图分离。
- **完整答案**：列数变化会改变中文宽字符换行、输入光标、候选和活动区高度；resize 后应从语义状态重新计算布局，而不是移动旧坐标。启动图在无对话时重新居中，一旦 transcript 已提交则不回到欢迎页。小终端隐藏非必要元素。
- **源码映射**：`tui.rs::{draw_intro,centered_column,layout_height,append_wrapped}`。
- **追问**：为何不能用字符串 len？UTF-8 字节长度不等终端列宽。
- **常见误区**：只更新窗口宽度变量。
- **评分**：4 分须覆盖 Unicode、光标和语义重绘。

### TUI-005：多行 Composer 如何处理 Enter 与粘贴？【进阶｜输入】
- **考察目标**：理解终端输入语义。
- **完整答案**：普通 Enter 发送；Ctrl+J/约定快捷键插入换行；Bracketed Paste 事件一次接收整段，长粘贴折叠为占位但内部保留，不应把粘贴中的换行当多次 Enter 自动发送。编辑状态以字符索引维护，光标位置按显示宽度计算。
- **源码映射**：`tui.rs::{handle_input_key_regular,insert_paste_text,wrap_input}`、`input_editor.rs`。
- **追问**：为何启用 bracketed paste？区分用户按键和批量粘贴。
- **常见误区**：逐字符模拟粘贴。
- **评分**：4 分须说明发送边界和内部完整内容。

### TUI-006：上下文内审批如何保持安全默认？【进阶｜审批 UX】
- **考察目标**：把安全策略与交互结合。
- **完整答案**：审批卡显示脱敏工具/影响摘要，默认选“拒绝”，上下/jk 选择，Enter 确认，Esc/Ctrl+C 拒绝；Once、Session、Always 作用域明确。Always 写用户级 approval rules，后续同工具+副作用匹配不再询问；不得写项目仓库或把 Plan 批准混入。
- **源码映射**：`approval_prompt.rs`、`main.rs::ConsoleApprovalGate`、`approval.rs`。
- **追问**：为何规则键包含副作用？同名工具不同影响不能共享授权。
- **常见误区**：Session 规则重启后仍生效。
- **评分**：4 分须解释默认拒绝与三种作用域。

### TUI-007：Markdown 流式渲染为何要缓冲未闭合结构？【高级｜Markdown】
- **考察目标**：理解增量解析难点。
- **完整答案**：半个代码围栏、表格或强调在下一 delta 到来前语法不完整，立即渲染会闪现 `#`、`**` 或错误表格。TUI 只提交闭合行/节点，把尾部保留活动缓冲；最终消息再完整渲染。Plain 与 TUI 应共享语义解析，仅样式降级。
- **源码映射**：`markdown.rs`、`tui.rs::{take_ready_segments,is_fence_line,flush_table}`。
- **追问**：为何不用每 token 重新解析全文？成本高且会重复打印已提交内容。
- **常见误区**：Markdown 是简单正则替换。
- **评分**：4 分须说明流式边界和一致输出。

### TUI-008：如何测试 TUI 而不依赖人工截图？【高级｜PTY 测试】
- **考察目标**：设计终端集成验证。
- **完整答案**：纯布局/输入状态做单元测试；Renderer 用内存 writer 快照 ANSI/纯文本；PTY 测试启动真实进程，发送按键、paste、resize、Ctrl+C，断言历史、队列和退出状态；覆盖 40×12、80×24、160×50、TERM=dumb、NO_COLOR 和 Windows Terminal 差异。快照应规范化耗时/UUID。
- **源码映射**：`tui.rs` tests、CLI integration tests。
- **追问**：为何只看最终屏幕不够？无法发现中途重复输出、闪烁和取消失效。
- **常见误区**：TUI 无法自动化测试。
- **评分**：4 分须提出状态、快照、PTY 三层。

---

## 第十一章：测试、CI、发布与演进（9 题）

> **知识类型：通用软件工程。** 这些方法适用于多数 Rust/后端项目，不是 Agent 独有知识。

### 知识地图

本章考察如何证明 Agent 在确定性逻辑、模型协议、安全边界、三平台终端和发布供应链上都可靠。

### QA-001：XDUDU 的测试金字塔是什么？【初级｜测试策略】
- **考察目标**：区分不同测试职责。
- **完整答案**：纯解析、状态迁移、路径与算法做单元测试；MockProvider/MemoryStore 验 Agent 和 Plan 协议；本地 HTTP/stdio Server 验 wire 与恶意输入；CLI/PTY 验命令和交互；少量真实网络冒烟可选。越靠上越少、越慢，不能用 E2E 替代核心单元测试。
- **源码映射**：各模块 `#[cfg(test)]`、workspace integration/security tests。
- **追问**：安全边界放哪层？单元与集成都要，前者穷举，后者证明接线。
- **常见误区**：cargo test 通过即产品所有场景正确。
- **评分**：4 分须说明各层输入和断言对象。

### QA-002：MockProvider 应具备哪些能力？【进阶｜测试替身】
- **考察目标**：验证多轮 Agent 行为。
- **完整答案**：按队列返回文本、单/多工具调用、reasoning、usage、Length、错误和协议畸形；记录收到的 Request 以断言工具结果回传、摘要和消息顺序；支持取消/延迟模拟。它不应复刻真实 HTTP parser，否则两者可能共同犯错。
- **源码映射**：`agent.rs`、`plan_*`、`subagent.rs` 测试 Mock。
- **追问**：何时用 fake server？验证 SSE/Headers/JSON wire 时。
- **常见误区**：Mock 永远返回 Stop。
- **评分**：4 分须覆盖脚本化响应与请求捕获。

### QA-003：安全测试最重要的负向断言是什么？【进阶｜安全】
- **考察目标**：证明危险动作没有发生。
- **完整答案**：不仅断言返回 PERMISSION_DENIED，还要断言目标文件未变、子进程未启动、HTTP Server 零请求、账本无错误 Applied、日志无 Key。覆盖审批拒绝、路径逃逸、哈希冲突、恶意 MCP、私网重定向和批次副作用跳过。
- **源码映射**：security integration tests、工具模块测试。
- **追问**：为何只看错误码不够？实现可能先产生副作用再报错。
- **常见误区**：失败结果天然表示安全。
- **评分**：4 分须提出副作用零发生证明。

### QA-004：Clippy `-D warnings` 的价值与风险？【初级｜静态检查】
- **考察目标**：理解质量门禁。
- **完整答案**：把可疑所有权、无效分支、API 误用等 warning 升为失败，保持 main 无债务；`--workspace --all-targets --locked` 覆盖测试和二进制并锁依赖。升级 Rust 可能新增 lint 导致 CI 失败，因此工具链需管理，必要 allow 要带理由且局部。
- **源码映射**：CI workflow、README 质量命令。
- **追问**：Clippy 能证明无逻辑 bug？不能，只是静态启发式。
- **常见误区**：为了通过全局 allow warnings。
- **评分**：4 分须说明工具链漂移和局部豁免。

### QA-005：为什么要 `--locked`？【初级｜可复现构建】
- **考察目标**：理解 Lockfile 与供应链。
- **完整答案**：`--locked` 要求 Cargo.lock 不变，CI、发布和本地使用同一解析依赖，避免索引更新静默选择新版本。它不锁操作系统库或 Rust 工具链，发布还需固定 runner/toolchain 并审计依赖。
- **源码映射**：`Cargo.lock`、CI/release workflow。
- **追问**：库项目也提交 lock 吗？可讨论；本项目发布应用二进制，应提交。
- **常见误区**：semver 范围自动等于可复现。
- **评分**：4 分须说明 lock 的边界。

### QA-006：三平台 CI 最可能暴露什么？【进阶｜跨平台】
- **考察目标**：理解 macOS/Linux/Windows 差异。
- **完整答案**：路径分隔/权限位、symlink 能力、原子 rename、终端事件、Git/Keyring、进程终止、换行和 shell 命令差异。CI 运行 fmt、clippy、全测试和 release build；平台专属测试用 cfg 明确，不应用跳过掩盖核心语义。
- **源码映射**：`.github/workflows`、`path_policy.rs`、`terminal_exec.rs`、TUI tests。
- **追问**：在 macOS 通过为何 Windows 仍失败？文件权限和进程/终端 API 完全不同。
- **常见误区**：Rust 跨平台意味着应用无需平台测试。
- **评分**：4 分须列至少四类差异。

### QA-007：发布供应链如何保证用户拿到正确二进制？【高级｜Release】
- **考察目标**：理解构建、校验和来源证明。
- **完整答案**：tag 触发三平台 release build，生成命名稳定的产物、SHA-256 校验和与 attestation，再创建 Release；安装脚本验证平台、下载和 checksum。版本号、tag、帮助和文档一致；回滚发布新修复版本或撤下有问题资产，不覆盖旧 tag。
- **源码映射**：release workflow、`scripts/install-e2e.sh`、`COMPATIBILITY.md`。
- **追问**：checksum 能防 GitHub 账户被攻陷吗？只能防传输/文件变化，attestation 和仓库权限提供更强来源链。
- **常见误区**：cargo build --release 后手工上传即可。
- **评分**：4 分须覆盖自动化、校验、证明和回滚。

### QA-008：1.0 兼容性应冻结哪些接口？【高级｜版本治理】
- **考察目标**：理解 CLI 产品的公共面。
- **完整答案**：命令/参数/退出码、配置键和默认值、环境变量、凭据服务名、数据库迁移、事件 JSON、Tool/Plan 错误码、文件目录与安装方式都可能被脚本或用户依赖。冻结不等于永不新增，而是新增向后兼容、删除需弃用周期和迁移说明。
- **源码映射**：`COMPATIBILITY.md`、`error.rs`、`config.rs`、CLI clap 定义。
- **追问**：内部 Rust pub 是否全是稳定 API？若 crate 未承诺库生态，可限定；外部行为更关键。
- **常见误区**：只有函数签名算 API。
- **评分**：4 分须覆盖命令、数据、事件和错误。

### QA-009：项目当前成熟度与下一步是什么？【高级｜项目判断】
- **考察目标**：基于事实而非宣传评价。
- **完整答案**：M1～M10 功能链基本完成，M11 本地门禁通过并已含 Provider、停滞、子代理图、Skills、压缩、记忆和 web_read；仍需确认 M11 三平台 CI、完成 M9 FTS 是否足够的评审、发布 v1.0.0，并持续做真实 Agent Eval。Provider fallback、向量 RAG、后台多 Agent 等不是当前完成能力。
- **源码映射**：`docs/TASKS.md`、`ARCHITECTURE.md`、当前 CI。
- **追问**：能否作为学习项目？可以，因链路完整且安全/恢复实现丰富；但不能当未经审计的生产沙箱。
- **常见误区**：功能多就等于生产成熟。
- **评分**：4 分须准确区分已实现、待验收和未来候选。

---

## 核心白板题参考图

### 1. ReAct 模型—工具循环

```text
Session + Prompt + Tools
          │
          ▼
      Provider Request
          │
      ┌───┴──────────────┐
      │ ToolCalls        │ Stop/Length/Error
      ▼                  ▼
持久化 Pending       终态约束检查
      │
ToolRegistry 策略链
      │
Observation 写回 Session
      └──────────────► Reflecting ─► 下一轮
```

### 2. ToolRegistry 策略链

```text
查找工具 → Permission → validate → cancellation → preflight
        → ApprovalGate → timeout(execute) → 脱敏结果/审计
```

换序风险：审批早于 validate 会批准无效输入；execute 早于审批会产生越权副作用；preflight 后不复检哈希会发生 TOCTOU 覆盖。

### 3. SSE 工具参数聚合

```text
frame(index=0,name="apply_patch")
frame(index=0,args="{\"pat")
frame(index=1,name="file_read")
frame(index=0,args="ch\":\"...")
frame(index=1,args="{...}")
                 │
                 ▼
BTreeMap<index, PartialToolCall> → 结束帧 → 完整 JSON parse → ToolCall
```

### 4. 多文件事务与恢复

```text
解析全部 → 内存应用 → 哈希复检 → Prepared 落盘 → Applying
   → 临时文件 → 逐项替换/删除 → Applied
                         │失败/崩溃
                         ▼
                 比较前/后哈希
          ┌──────────────┴─────────────┐
       可判定                         用户已改
       回滚前镜像                     Conflict，不覆盖
```

### 5. Plan 双版本乐观锁

| 维度 | 变化时机 | 防止的问题 |
|---|---|---|
| `revision` | 内容修订 | 陈旧审批覆盖新方案 |
| `execution_version` | 每个运行检查点 | 两个执行器覆盖进度 |

```sql
UPDATE plans
SET plan_json = ?, execution_version = execution_version + 1
WHERE id = ? AND revision = ? AND execution_version = ? AND status = ?;
```

### 6. 子代理 DAG 调度

```text
        A(探索配置) ─┐
                     ├─► C(汇总) ─► D(审阅)
        B(探索工具) ─┘

全图预检 → Kahn 无环校验 → Ready 选择 → 只读并发
→ 有副作用节点独占 → 报告按依赖顺序注入 → fail-fast/continue
```

### 7. 上下文与长期记忆

```text
当前会话最近消息 ─────────────┐
超预算历史 → context summary ├─► Token Budget → Provider
跨会话原始提炼 → SQLite FTS ─┤
                   │          │
                   └→ 合并/去重 → MEMORY.md（用户可编辑）
```

### 8. MCP 接入安全链

```text
Server initialize → tools/list → 名称空间化 McpTool
→ ToolRegistry(Permission/validate/Approval)
→ stdio 或 HTTP call → 有界解析/脱敏 → Agent Observation
```

## 面试官使用建议

1. 不要求候选人背文件名；源码映射用于核验答案是否能落到实现。
2. 初级题看概念是否准确，中级题看数据流，高级题看失败模式、替代方案和边界。
3. 系统设计题允许不同答案，但必须保持：权限不由模型授予、未知副作用不自动重放、完成以证据而非文本判定。
4. 实战面试可让候选人任选一题，在源码中找到入口、画调用链，再设计一个负向测试。
5. 自测总分 600：450 分以上且高频题均达 3 分，可认为具备独立续写该项目的基础；520 分以上并能完成白板推演，可进入高级架构讨论。
