# M11 Skills、指令与命令白名单设计

## 1. 范围

M11 补齐三类开放扩展与体验能力：

- **Skills 技能系统**：支持 `SKILL.md`（YAML frontmatter）按需加载，与 Claude Code / opencode 生态目录兼容；
- **指令接通**：把已实现但未接入主循环的用户/项目级 `instructions`（`~/.config/xdudu/instructions/*.md`、`.xdudu/instructions/*.md`）真正注入系统提示词，并新增 `AGENTS.md` / `CLAUDE.md` 兼容读取；
- **命令白名单规则化**：`terminal_exec` 从「pwd/echo/ls/只读 git 五条」扩展为按前缀规则的三档策略（allow / ask / deny），对齐主流 Agent 体验。

## 2. Skills 技能系统

### 2.1 目录发现与优先级

按以下顺序发现 `SKILL.md`（第一个命中者优先），每项需验证 frontmatter：

```text
项目级 .xdudu/skills/<name>/SKILL.md
项目级 .claude/skills/<name>/SKILL.md      （Claude Code 兼容）
项目级 .opencode/skills/<name>/SKILL.md    （opencode 兼容）
用户级 ~/.config/xdudu/skills/<name>/SKILL.md
用户级 ~/.claude/skills/<name>/SKILL.md
用户级 ~/.config/opencode/skills/<name>/SKILL.md
```

`<name>` 必须匹配 `^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$`，与目录名一致。重复名称按上述优先级取第一个；不同目录同名的 skill 不算冲突。

### 2.2 Frontmatter 与格式

`SKILL.md` 必须以 YAML frontmatter 开头，只识别固定字段：

```yaml
---
name: git-release        # 必填，与目录名一致
description: 维护 release 分支与版本标签的工作流  # 必填，1..=1024 字符
license: MIT             # 可选
metadata:                # 可选，string-to-string
  requires: git
---
# 技能正文（Markdown，作为指令加载）
```

只读取前 512 字节做 frontmatter 解析（防止超大元数据注入）；正文限制 64 KiB。

### 2.3 skill 工具

模型通过 `skill` 工具按需加载：

```json
{
  "type": "object",
  "required": ["name"],
  "additionalProperties": false,
  "properties": { "name": { "type": "string", "minLength": 1, "maxLength": 64 } }
}
```

- 工具描述中列出全部可用 skill 的 `name + description`（仅索引，不包含正文）；
- 命中时工具结果只返回名称、描述、来源和加载状态；正文从下一次 Provider 请求开始注入
  system 提示词，不重复写入工具结果或公开会话输出，并发送 `SkillLoaded` 事件；
- 未命中返回 `TOOL_NOT_FOUND` 类错误；同名技能正文按首次成功加载顺序去重；
- 加载属于无副作用操作，但受 `skills` 权限模式约束（见下）。

### 2.4 权限

配置 `agent.skills`（默认 `ask`？不，默认 `allow` 但每个 skill 可覆盖）：

```toml
[agent]
skills = "allow"        # allow | ask | deny
```

- `deny`：技能索引不出现在工具描述中，加载被拒绝；
- `ask`：加载前进入审批门（复用 ApprovalGate）；
- `allow`：直接加载。

项目配置不能把 `skills` 从用户默认放宽（沿用项目不可信原则）。

## 3. 指令接通与 AGENTS.md/CLAUDE.md 兼容

### 3.1 现状与缺口

`instructions.rs` 已实现用户/项目级目录加载与 `render_instructions`，但 `agent.rs` 仍调用无指令版 `build_system_prompt`，指令从未进入主循环。M11 修复接通，并新增仓库约定文件读取。

### 3.2 加载顺序

系统提示词中「自定义指令」段的拼接顺序（后写覆盖先写，全部标记来源）：

```text
1. ~/.config/xdudu/instructions/*.md         （用户级，现有）
2. <repo>/.xdudu/instructions/*.md           （项目级，现有）
3. <repo>/AGENTS.md                          （仓库约定，新）
4. <repo>/CLAUDE.md                          （Claude Code 兼容，新）
5. <repo>/.claude/CLAUDE.md                  （Claude Code 嵌套兼容，新）
```

`AGENTS.md` / `CLAUDE.md` 从 `cwd` 向上沿仓库根（首个 `.git` 目录或文件系统根）查找，只取首个命中的同名文件。项目 `.xdudu`、`.claude`、`.opencode` 与 `AGENTS.md`/`CLAUDE.md` 都视为不可信输入：只影响工作方式，不改变权限、审批、Base URL 或网络边界（沿用现有项目指令安全声明）。

### 3.3 注入实现

`build_system_prompt_with_instructions` 已存在；`run_agent` 改为：

```rust
let instructions = render_instructions(&load_instructions(cwd).0);
let system = build_system_prompt_with_instructions(&definitions, cwd, &instructions);
```

限制保持：单文件 64 KiB、目录最多 32 个文件、跳过项目 symlink；为 `AGENTS.md`/`CLAUDE.md` 单独设 128 KiB 上限并跳过 symlink。`/instructions` 与 `doctor` 增加「指令加载摘要」输出（来源、数量、告警），方便排查。

## 4. 命令白名单规则化

### 4.1 现状

`terminal_exec` 在 `auto-safe` 下仅允许 `pwd`、`echo`、工作区内 `ls`、只读 git 子命令，其余一律 `UNSAFE_COMMAND` 拒绝。这导致构建、测试、安装等日常命令每一步都要人工放行。

### 4.2 三档策略

`auto-safe` 的 `terminal_exec` 改为三档匹配（按命令可执行名 + 前缀规则）：

| 档位 | 行为 | 默认规则 |
| --- | --- | --- |
| `allow` | 直接执行，不弹审批 | `pwd`、`echo`、工作区内 `ls`、只读 git（status/log/diff/show/branch -l）、`cargo check`、`cargo build`、`cargo test`、`npm run build`、`npm test`、`python -m pytest` 等（白名单见下） |
| `ask` | 进入审批门（交互 TUI 单次/会话/永久） | 其他未匹配命令 |
| `deny` | 立即拒绝 | 明确危险命令：`rm -rf`、`sudo`、`curl | sh`、写 shell 历史等（前缀匹配） |

配置（用户级）：

```toml
[agent.commands]
allow = ["cargo check", "cargo build", "cargo test", "npm run build", "npm test", "git status", "git log"]
ask = ["git push", "git commit", "npm install", "cargo install"]
deny = ["rm -rf", "sudo", "mkfs"]
```

规则匹配顺序：`deny` > `allow` > `ask`（deny 优先，命中即拒）。前缀匹配以「完整可执行名 + 首个空格前参数」归一化；项目配置只能追加 `deny` 与 `ask`，不能追加 `allow`（项目不可信原则）。

### 4.3 默认 allow 白名单（auto-safe）

```text
pwd | echo | ls（工作区内）
git status | git log | git diff | git show | git branch -l | git stash list
cargo check | cargo build | cargo test | cargo fmt --check | cargo clippy
npm run build | npm test | npm run lint
python3 -m pytest | python3 -m unittest
make -n | gofmt -l | go test
```

仍不允许：任何以 `&&`、`;`、`|` 连接、重定向（`>`、`<`、`>>`）、通配符注入或 shell 特性；全部按「可执行文件 + 参数数组」执行，不经过 shell。

## 5. 事件与命令

- `AgentEvent::SkillLoaded { name }`：技能加载事件（TUI 显示一行，JSON 输出原始事件）；
- `/skills` 交互命令：列出可用技能（来源、启用状态），`/skills <name>` 显示详情；
- `/instructions` 交互命令：显示指令来源与数量。

## 6. 测试与验收

- 单元：frontmatter 解析（非法、超长、名称不匹配）、目录优先级、`skill` 工具 schema 校验；
- 权限：`deny` 技能不出现在描述、`ask` 进入审批、项目配置不能加宽；
- 指令：`AGENTS.md`/`CLAUDE.md` 注入顺序与不可信声明；symlink 与超大文件跳过；
- 命令：三档前缀匹配表驱动测试，`cargo build` 放行而 `sudo` 拒绝，`&&`/重定向拒绝；
- CLI E2E：`/skills`、`/instructions` 输出正常。
