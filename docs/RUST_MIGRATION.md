# XDUDU Rust 迁移与收尾记录

> 迁移版本：v0.2.0
> 开始日期：2026-07-20
> 收尾日期：2026-07-21
> 状态：完成

## 1. 迁移范围

本次以 Rust 替换原 TypeScript 核心运行时，覆盖 CLI、Agent 循环、权限、工具注册、文件工具、终端工具、会话存储、Anthropic 和 DeepSeek Provider。

迁移不是逐行翻译，而是保持可观察行为和安全边界，并通过 Rust 测试重新建立可信基线。

## 2. 最终结构

```text
Cargo workspace
├── crates/xdudu-cli
│   ├── src/main.rs
│   └── tests/cli.rs
└── crates/xdudu-core
    ├── src/
    └── tests/
```

旧 `src/`、`test/`、npm 清单、TypeScript 配置、Vitest 配置和 Node 构建脚本已删除。迁移前实现仍可通过 Git 历史审计。

## 3. 关键设计决定

### 3.1 核心库与 CLI 分离

`xdudu-core` 不依赖具体终端，`xdudu` 是薄 CLI。这样可以独立测试核心，也为桌面端或服务端复用留出边界。

### 3.2 统一异步运行时

Tokio 管理 HTTP、文件、子进程、超时、信号和取消。`CancellationToken` 从 CLI 传到 Agent、Provider 与工具。

### 3.3 严格工具边界

Provider 工具参数保留为 `serde_json::Value`，每个工具在执行前严格校验必填字段、类型、长度、数值范围和未知字段。

### 3.4 无 shell 命令执行

终端工具只接受程序名和参数数组。即使在 `full-access` 下，分号、管道和重定向也不会被 shell 解释。

### 3.5 会话数据

会话 JSON 使用 `camelCase`，写入采用同目录临时文件和原子重命名。格式兼容迁移期已有会话，但 Rust 实现现在是唯一写入方。

## 4. 验收流程

```bash
rustup component add rustfmt clippy
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace --release
./target/release/xdudu --help
```

测试覆盖单元、Agent、多轮工具、路径安全、命令安全、Provider HTTP 和真实 CLI 子进程。默认测试不访问线上模型 API。

## 5. 退役准则

删除旧实现前确认：

- Rust 已覆盖原主链路；
- Rust 格式、Clippy、测试和 Release 构建全部通过；
- CLI 可以在本地真实运行；
- 旧实现不再是默认入口；
- 文档和路线图已切换为 Rust-only；
- 迁移前代码可通过 Git 历史恢复。

以上条件均已满足。

## 6. 后续边界

迁移完成不等于产品功能完成。配置与系统凭据、流式输出、Provider 重试、交互审批、SQLite、上下文压缩、MCP 和正式跨平台发布仍属于后续里程碑。
