# XDUDU M1 稳定化修复设计（历史归档）

> 状态：已完成并被 Rust 实现取代。本文仅记录迁移前发现的问题和决策依据，不代表当前模块、命令或构建方式；当前设计见 `DESIGN.md`。

## 1. 修复目标

本轮不扩展 M2 功能，先把 M1 从“能运行的原型”收敛为“行为可预测、默认权限可信、可以继续迭代的基线”。修复范围来自 2026-07-20 的项目审查，覆盖以下问题：

1. `auto-safe` 可执行任意 shell 命令，且文件工具可以越过工作区。
2. 工具声明了 JSON Schema，但执行前没有校验模型输入。
3. 最大轮次和模型输出截断没有明确的未完成终态。
4. CLI 异常退出码、异步入口和 Provider 初始化错误处理不一致。
5. `AbortSignal` 没有传递到模型请求，Ctrl+C 不能及时终止网络等待。
6. REPL 每轮创建新会话，`/turns` 命令不生效，并可能并发处理输入。
7. DeepSeek 缺少测试和统一导出，流式工具参数累积不正确。
8. 当前 E2E 没有启动真实 CLI；npm 发布包包含测试和源码。
9. README、开发报告和目标架构文档与实际实现不同步。
10. 代码注释及 Markdown 文档语言不统一。

## 2. 设计原则

- **默认拒绝**：不确定的命令、路径或参数在 `auto-safe` 下拒绝执行。
- **工作区为边界**：`file_read`、`file_write` 以及安全命令的工作目录只能位于启动目录内，并防御 `..` 与符号链接逃逸。
- **不拼接 shell**：`terminal_exec` 使用“可执行文件 + 参数数组”，设置 `shell: false`，避免 shell 元字符注入。
- **权限分层**：`read-only` 只允许文件读取；`auto-safe` 只允许受审计的只读命令和工作区文件写入；`full-access` 才允许任意可执行文件。
- **边界校验一次完成**：ToolRegistry 在执行工具前统一运行 Zod 校验，工具实现只接收已经验证的数据。
- **终态不可含糊**：成功、未完成、中断和错误必须有不同状态，CLI 必须返回非零退出码表示非成功。
- **依赖可注入**：Provider 客户端允许测试注入，真实协议映射不依赖线上 API 才能测试。
- **文档区分现状与目标**：目标架构不再被表述成已实现能力。

## 3. 权限与路径设计

### 3.1 文件路径

新增统一的工作区路径策略：

1. 将启动目录解析为真实路径。
2. 对已存在目标使用 `realpath()`，阻止符号链接逃逸。
3. 对待创建目标逐级寻找最近的已存在父目录并解析真实路径。
4. 使用 `path.relative()` 判断目标是否仍在工作区内。
5. 越界时返回 `PATH_OUTSIDE_WORKSPACE`，不泄露文件内容。

`file_write` 的临时文件名加入随机后缀，避免同一路径并发写入时互相覆盖。

### 3.2 命令执行

`terminal_exec.command` 只接受单个可执行文件名，不接受空格或 shell 元字符；参数必须通过 `args` 数组传递。

`auto-safe` 首批白名单：

| 命令 | 允许范围 |
| --- | --- |
| `pwd` | 无参数 |
| `ls` | 常见展示参数及工作区内相对路径 |
| `git status` | 状态查看 |
| `git diff` | 工作区、暂存区差异查看 |
| `git log` | 历史查看，限制参数集合 |
| `git show` | 提交内容查看，拒绝输出到文件的参数 |

`npm`、`node`、`bash`、`sh`、`python`、`find -exec` 等具有间接代码执行能力的命令不属于安全白名单；需要显式使用 `--permission full-access`。白名单命令还必须解析到工作区之外的真实可执行文件，避免仓库通过修改 PATH 劫持 `ls` 或 `git`。

所有模式均校验命令 `cwd` 位于工作区内。`auto-safe` 禁止自定义环境变量；`full-access` 可以传入，但仍不使用 shell 拼接。

## 4. 工具输入校验

为 `ITool` 增加 Zod `inputValidator`：

- ToolRegistry 在计算幂等键和执行前调用 `safeParse()`。
- 校验失败返回 `INVALID_TOOL_INPUT`，包含字段路径和原因，不进入工具实现。
- 对字符串长度、数字范围、数组大小和未知字段使用严格限制。
- Provider 侧继续使用 JSON Schema 描述，运行时以 Zod 为权威校验器。

## 5. Agent 状态与退出码

新增会话状态 `incomplete` 和循环状态 `INCOMPLETE`：

- 达到 `maxTurns`：`incomplete`，提示达到轮次上限。
- Provider `finishReason=length`：保存已有文本并标记 `incomplete`。
- Ctrl+C：`interrupted`。
- Provider 或持久化关键错误：`error`。
- 只有 Provider 正常结束才是 `completed`。

`AgentRunResult` 增加 `exitCode`：

| 结果 | 退出码 |
| --- | ---: |
| 完成 | 0 |
| 一般未完成或中断 | 1 |
| 参数校验失败 | 2 |
| 权限错误 | 3 |
| Provider 错误 | 4 |
| Tool 致命错误 | 5 |

CLI 使用 `parseAsync()`，顶层统一捕获 `XduduError`。单次模式设置 `process.exitCode`，不再无条件 `process.exit(0)`。

## 6. 中断与 REPL

- `ProviderRequest` 增加可选 `signal`，Anthropic 与 DeepSeek 请求均向 SDK 传递该信号。
- 工具注册中心使用组合 AbortController，并在结束后移除外部监听器。
- REPL 改为 `for await...of` 串行读取，避免多行输入并发执行。
- `runAgent` 支持通过 `sessionId` 继续已有会话；REPL 在同一个会话追加用户消息。
- `/turns` 修改真实运行变量；新增 `/new` 开始新会话。

## 7. Provider 与流式响应

- Provider 构造函数支持注入 SDK 客户端，用 mock 覆盖消息映射、错误映射和取消行为。
- Anthropic 流结束时使用 SDK 的 `finalMessage()` 获取完整文本及工具参数。
- DeepSeek 按 tool-call `index` 累积参数字符串，流结束后统一解析 JSON。
- 未知 finish reason 映射为 `error`，不再误报成功。
- DeepSeek 从 Provider 公共入口导出。

## 8. 测试与发布

### 8.1 测试顺序

1. 先增加会失败的单元测试：路径逃逸、命令注入、输入校验、最大轮次、截断、中断。
2. 实现后运行目标测试。
3. 增加真实进程 E2E：启动本地兼容 API 服务，再启动 CLI 完成 `list files`。
4. 最后运行测试、类型检查、构建和打包预检。

### 8.2 发布包

- 新增 `tsconfig.build.json`，只编译生产源码。
- `bin` 指向 `dist/cli.js`。
- `package.json.files` 只包含 `dist`。
- 声明 Node.js `>=18`。
- 打包预检必须确认不包含测试源码或编译后的测试。

## 9. 文档与中文化规则

- 所有自有 TypeScript 注释改为中文；第三方协议字段、类型名和命令保持原文。
- 所有 Markdown 叙述改为中文，代码块中的标识符不强制翻译。
- `ARCHITECTURE.md` 明确标注“目标架构”；新增当前实现章节。
- README、开发报告和任务统计以本轮最终验证数据为准。

## 10. 完成门槛

只有同时满足以下条件，本轮才可以请求用户确认推送：

- 全部自动化测试通过，且没有安全相关跳过项。
- 类型检查和生产构建通过。
- npm 打包预检不包含测试与 `src`。
- 默认配置与 README 一致。
- 工作区不存在意外生成或无关修改。
- 输出本地变更摘要；未收到用户明确确认前，不执行 commit、push 或创建 PR。
