# M8 MCP 与声明式插件设计

## 1. 范围

M8 将 XDUDU 作为 MCP Client 接入外部工具。首版支持 MCP Tools，不加载外部动态库，也不允许插件代码进入 XDUDU 进程。

支持两种标准传输：

- `stdio`：直接启动本地子进程，使用逐行 UTF-8 JSON-RPC 通信；
- `streamable-http`：通过 HTTP POST 接收 JSON 或 SSE 响应，远端强制 HTTPS，本机开发地址允许 HTTP。

旧版 HTTP+SSE 传输不在首版兼容范围内。Resources、Prompts、OAuth 自动授权和服务端通知订阅留到后续兼容阶段。

## 2. 生命周期

每个启用的 Server 都执行：

```text
读取并校验配置
  → 创建隔离连接
  → initialize
  → notifications/initialized
  → tools/list（有界分页）
  → 注册到 ToolRegistry
  → tools/call
```

协议版本优先使用 `2025-11-25`，同时接受已知旧版本。单个 Server 离线或协议错误时会被跳过并形成启动警告，不阻止其他内置工具和 Server 启动。

## 3. 安全边界

- stdio 使用参数数组启动程序，不经过 shell；清空继承环境，只保留系统执行所需路径和清单显式声明的非秘密变量；
- stdio stdout 只接受最大 1 MiB 的逐行 JSON-RPC，取消时终止子进程；
- Streamable HTTP 禁用系统代理和自动重定向；公网地址复用 DNS 固定及 SSRF 校验；
- HTTP Bearer Token 只从系统凭据 `mcp:<引用名>` 读取，不写入 TOML、日志或模型上下文；
- 消息、工具数量、分页、参数和超时都有硬上限；
- 外部工具统一命名为 `mcp__<server>__<tool>`；
- MCP 工具被视为不可信的 `full-access` 能力，必须经过现有权限、输入校验、审批、超时、取消、脱敏和审计链；
- Server 返回内容只作为工具数据，不具有覆盖系统提示词和权限规则的能力。

## 4. 配置与命令

用户配置位于 `~/.config/xdudu/mcp.toml`，通过命令管理：

```bash
xdudu mcp add-stdio filesystem npx -y @modelcontextprotocol/server-filesystem /workspace
xdudu mcp add-http team https://mcp.example.com/mcp --auth
xdudu mcp login team
xdudu mcp list
xdudu mcp doctor team
xdudu mcp disable team
```

`login` 使用隐藏输入并保存到操作系统凭据库。`show` 只显示环境变量名称和凭据占位符。

## 5. 声明式插件

插件是 `~/.config/xdudu/plugins/*.toml` 中的受限清单，只能声明 MCP Server：

```toml
schemaVersion = 1
id = "team-tools"
name = "Team Tools"
version = "1.0.0"
enabled = true

[[mcpServers]]
name = "team"
enabled = true
transport = "streamable-http"
url = "https://mcp.example.com/mcp"
credential = "team"
timeoutSeconds = 30
```

未知字段（包括动态库或脚本入口）会被拒绝。`sha256` 和 `signature` 当前属于可校验格式的来源元数据，不代表 XDUDU 已建立发行者信任链。插件与用户 MCP 配置中的 Server 名称不能重复。

管理命令：

```bash
xdudu plugin list
xdudu plugin show team-tools
xdudu plugin enable team-tools
xdudu plugin disable team-tools
xdudu plugin doctor team-tools
```

## 6. 当前限制

- 不实现任意 Rust/Python 插件进程内加载；Python 能力应作为隔离的 stdio/HTTP MCP Server 接入；
- 不自动安装插件依赖，不运行安装脚本；
- 不把配置文件中的值当作密钥；
- 不跟随 HTTP 重定向；
- 不实现 OAuth 浏览器登录和长期服务器事件流；
- 不支持 MCP Resources 与 Prompts。

这些限制确保首版扩展能力仍受 XDUDU 的统一安全边界控制。
