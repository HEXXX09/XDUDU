# XDUDU v0.6.0 工具与网络安全设计

> 状态：本地实现与自动化测试已完成，等待用户本地运行验收和三平台远端 CI。

## 1. 范围与原则

M6 增加原生文本搜索、结构化 Git 查询、多文件事务补丁、工具进度和受限网页抓取。Provider 扩展、Plan、MCP、插件、RAG、浏览器自动化和文件下载不属于本阶段。

所有能力遵守同一条执行链：

```text
ToolRegistry 注册
  → PermissionMode 权限矩阵
    → 严格输入校验
      → ApprovalGate 副作用审批
        → 工具专用安全策略
          → 超时、取消和有界输出
            → ToolResult、脱敏与会话审计
```

只读 Git 工具内部运行固定 Git 命令，属于工具实现细节，不触发进程执行审批。工作区写入和网络访问继续经过审批。

## 2. 工作区文本搜索

`search_text` 使用 `ignore` 遍历文件、`globset` 处理 include/exclude、`regex` 处理正则模式，不依赖用户安装 `rg`。

安全与资源边界：

- 搜索根路径必须位于工作区，绝对路径、`..` 和符号链接逃逸拒绝；
- 遵守 `.gitignore`，始终跳过 `.git`、`.xdudu`、`.xycli` 和 `target`；
- 允许 `.github` 等有价值隐藏目录；
- 跳过符号链接、含 NUL 的二进制文件、非 UTF-8 文件和超过 2 MiB 的文件；
- 最多扫描 20,000 个文件和 256 MiB 内容；
- 单行最多返回 4096 字节，总 JSON 结果最多 512 KiB；
- 行号和列号从 1 开始，列号按 Unicode 字符而不是 UTF-8 字节计算；
- 遍历期间定期检查取消令牌并发布扫描进度。

## 3. Git 专用只读工具

`git_status` 固定执行：

```text
git status --porcelain=v2 --branch -z
```

解析结果包含分支、upstream、ahead/behind、detached、clean 和文件状态。NUL 分隔避免特殊文件名破坏行解析，文件列表设置 10,000 项上限。

`git_diff` 仅允许 `worktree` 或 `staged`，固定使用 `--no-ext-diff`、`--no-textconv` 和 `--` 路径分隔。路径最多 50 项，全部必须是工作区相对路径；上下文和输出字节数有硬限制。

两个工具都会先解析 Git 仓库根。仓库根不在 XDUDU 工作区内时拒绝执行，不能使用自定义 `--git-dir`、`--work-tree` 或 `--no-index` 绕过边界。

## 4. v2 多文件事务账本

`JsonChangeLedger` 的新记录使用 `schemaVersion: 2`。一个工具调用只产生一个事务 ID，每个文件记录：

- 工作区相对路径；
- 创建、修改或删除操作；
- 前镜像与后镜像；
- 前后 SHA-256；
- 原文件 Unix 权限；
- 会话 ID 和模型工具调用 ID。

状态机：

```text
Prepared → Applying → Applied → Undone
                 ├──→ RolledBack
                 └──→ Conflict
```

写文件前先持久化 `Prepared`，开始提交时标记 `Applying`，全部文件成功后才标记 `Applied`。账本准备失败时零文件修改；最终账本写入失败时回滚全部文件。

启动恢复检查 `Prepared` 和 `Applying` 事务。每个当前文件必须匹配前哈希或后哈希，满足条件才整体恢复到前镜像并标记 `RolledBack`。任何文件既不匹配前哈希也不匹配后哈希时，不覆盖用户内容，事务标记 `Conflict`，启动返回明确错误。

`xdudu undo` 默认选择最近的完整事务。撤销前预检全部文件是否仍匹配后哈希；任一冲突都会使整批保持不变。v1 单文件 JSON 记录继续支持读取和撤销。

## 5. 严格多文件补丁

`apply_patch` 支持 UTF-8 unified diff 的创建、修改和删除，支持 `a/`、`b/`、`/dev/null`、多 hunk、LF/CRLF 和末尾换行语义。限制为 2 MiB、20 个文件、200 个 hunk 和 100,000 行变化。

补丁拒绝二进制、rename/copy 元数据、模式变更、符号链接、submodule、模糊 hunk 和工作区外路径。

执行过程：

1. 完整解析所有文件和 hunk；
2. 校验路径并读取全部前镜像；
3. 在内存中精确应用 hunk；
4. 计算前后哈希并再次读取当前文件，防止并发覆盖；
5. 保存 `Prepared` 事务并标记 `Applying`；
6. 使用同目录临时文件逐项原子替换或删除；
7. 任一失败时逆序恢复已经提交的文件；
8. 全部成功后标记 `Applied`。

回滚动作本身失败时记录 `Conflict`，错误会列出无法恢复的文件，不会伪装为成功回滚。

## 6. 工具进度事件

领域事件新增 `AgentEvent::ToolProgress`，包含工具调用 ID、工具名、阶段、完成量、总量、单位和可选消息。

Agent 为每次工具调用创建容量为 64 的 Tokio 通道。工具使用 `try_send` 非阻塞发送；通道满时允许丢弃中间进度，最终工具结果不受影响。进度不写入 SQLite、不参与模型上下文，只用于实时展示。

- `search_text` 每 1000 个文件或 250 ms 报告；
- `apply_patch` 报告解析、预检和逐文件提交；
- `web_fetch` 每 64 KiB 或 250 ms 报告；
- Git 工具由统一生命周期事件展示开始和完成；
- `--json` 输出稳定的 `tool_progress` JSON Lines；
- `--no-stream` 只关闭助手文本增量，工具阶段仍可显示；
- 所有终端和 JSON 内容展示前统一脱敏。

## 7. Web 权限与 SSRF 防御

`web_fetch` 使用 `PermissionLevel::ReadOnly` 和 `SideEffectKind::NetworkAccess`。三种权限模式都能提出网页读取，但网络副作用始终按 `ask`、`never`、`always` 独立审批。`ask` 可选择仅本次、当前会话同类工具或永久允许同类工具；细粒度规则只匹配 `web_fetch + network-access`，不会放行其他工具。这样 `read-only` 仍然可以在用户批准后查阅公开资料，同时不能修改本地文件或执行进程。

网络约束：

- 仅 GET 和 HTTPS；
- URL 不允许用户名或密码；
- 最多 5 次重定向，每一跳重新校验；
- 默认 512 KiB、最大 1 MiB，超时 1～30 秒；
- 固定 User-Agent `XDUDU/0.6`；
- 禁用系统代理、环境代理、Cookie、认证、自定义 Header、请求体和自动重试；
- 不读取浏览器登录状态，不写文件，不执行网页脚本。

每一跳先解析 DNS。只要结果中存在回环、私网、链路本地、共享地址、未指定、多播、保留地址或 IPv4 映射的非公网 IPv6，就整体拒绝。若系统解析结果全部位于 `198.18.0.0/15`，说明本机代理可能启用了 Fake-IP；此时通过固定到 Google Public DNS 公网地址的 HTTPS DoH 查询一次真实 A/AAAA 记录，查询结果仍执行完全相同的公网校验，不放行 Fake-IP 本身。通过 reqwest DNS override 把最终连接固定到已经验证的地址，TLS SNI 与证书验证仍使用原始域名，避免 DNS 检查后的重绑定。

响应只接受 `text/html`、`text/plain`、`application/json` 和 `*+json`。HTML 删除 `script`、`style`、`noscript`、`svg` 后提取标题和可读文本；文本达到上限时返回截断标记；JSON 超限时拒绝解析残缺内容。生产构造函数不存在私网放行参数。

## 8. 测试与发布门禁

本地自动化覆盖搜索忽略规则和 Unicode、Git 状态与暂存差异、多文件补丁与整批撤销、v1 账本兼容、崩溃恢复与用户冲突、进度 JSON、网络权限、HTML 清理以及 IPv4/IPv6 SSRF 地址分类。

发布前执行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --release --locked
cargo install --path crates/xdudu-cli --locked --force
xdudu doctor --json
```

用户本地运行验收后再提交和推送。GitHub Actions 需要在 macOS、Linux、Windows 全部通过，才能完成 v0.6.0 发布验收并进入 M7。
