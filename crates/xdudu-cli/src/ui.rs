//! XDUDU 终端视觉组件。

/// 终端主题。所有 ANSI 样式都集中在此处，保证 `--no-color` 可彻底关闭颜色。
#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalTheme {
    color: bool,
}

impl TerminalTheme {
    pub(crate) const fn new(color: bool) -> Self {
        Self { color }
    }

    pub(crate) fn accent(&self, text: &str) -> String {
        self.style("1;38;2;252;244;163", "1;38;5;229", text)
    }

    pub(crate) fn brand(&self, text: &str) -> String {
        self.style("1;38;2;225;204;112", "1;38;5;221", text)
    }

    pub(crate) fn success(&self, text: &str) -> String {
        self.style("1;38;2;132;169;140", "1;38;5;108", text)
    }

    pub(crate) fn warning(&self, text: &str) -> String {
        self.style("1;38;2;200;158;87", "1;38;5;179", text)
    }

    pub(crate) fn danger(&self, text: &str) -> String {
        self.style("1;38;2;190;110;110", "1;38;5;167", text)
    }

    pub(crate) fn muted(&self, text: &str) -> String {
        self.style("38;2;145;139;120", "38;5;102", text)
    }

    pub(crate) fn strong(&self, text: &str) -> String {
        self.style("1;38;2;220;218;211", "1;38;5;253", text)
    }

    fn style(&self, rgb_code: &str, ansi256_code: &str, text: &str) -> String {
        if self.color {
            let code = if supports_true_color() {
                rgb_code
            } else {
                ansi256_code
            };
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

/// 当前终端是否明确声明支持 24 位真彩色。
pub(crate) fn supports_true_color() -> bool {
    std::env::var("COLORTERM").is_ok_and(|value| {
        value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
    })
}

pub(crate) fn compact_banner(
    theme: TerminalTheme,
    version: &str,
    provider: &str,
    model: &str,
) -> String {
    format!(
        "{} {}  {}",
        theme.brand("XDUDU"),
        theme.muted(&format!("v{version}")),
        theme.muted(&format!(
            "{} · {provider}",
            model_display_name(provider, model)
        ))
    )
}

/// 将 Provider 的 API 模型标识转换为面向用户的具体型号。
///
/// 原始标识仍用于请求，此函数只负责展示，避免把兼容别名误认为具体型号。
pub(crate) fn model_display_name(provider: &str, model: &str) -> String {
    if provider.eq_ignore_ascii_case("deepseek") {
        match model {
            "deepseek-chat" => "DeepSeek-V4-Flash（非思考）".into(),
            "deepseek-reasoner" => "DeepSeek-V4-Flash（思考）".into(),
            "deepseek-v4-flash" => "DeepSeek-V4-Flash".into(),
            "deepseek-v4-pro" => "DeepSeek-V4-Pro".into(),
            _ => model.to_owned(),
        }
    } else {
        model.to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelOption {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) description: String,
}

/// 返回当前 Provider 在 XDUDU 中经过兼容性验证的模型，而不是展示任意字符串。
pub(crate) fn model_options(provider: &str, current: &str) -> Vec<ModelOption> {
    if provider.eq_ignore_ascii_case("deepseek") {
        return vec![
            ModelOption {
                id: "deepseek-v4-flash".into(),
                label: "DeepSeek-V4-Flash".into(),
                description: "快速 · 日常编码与工具任务".into(),
            },
            ModelOption {
                id: "deepseek-v4-pro".into(),
                label: "DeepSeek-V4-Pro".into(),
                description: "能力优先 · 复杂编码与分析".into(),
            },
        ];
    }

    vec![ModelOption {
        id: current.to_owned(),
        label: model_display_name(provider, current),
        description: "当前 Provider 配置".into(),
    }]
}

pub(crate) fn model_matches(option: &str, current: &str) -> bool {
    option == current
        || (option == "deepseek-v4-flash"
            && matches!(current, "deepseek-chat" | "deepseek-reasoner"))
}

pub(crate) fn prompt(theme: TerminalTheme) -> String {
    format!("{} ", theme.accent("❯"))
}

pub(crate) fn tool_display_name(name: &str) -> &str {
    match name {
        "web_search" => "联网搜索",
        "web_fetch" => "读取网页",
        "search_text" => "搜索代码",
        "file_read" => "读取文件",
        "file_write" => "写入文件",
        "apply_patch" => "应用补丁",
        "git_status" => "Git 状态",
        "git_diff" => "Git 差异",
        "terminal_exec" => "运行命令",
        _ => name,
    }
}

pub(crate) fn tool_phase_display(phase: &str) -> &str {
    match phase {
        "resolving" => "解析地址",
        "connecting" => "建立连接",
        "downloading" | "reading" => "读取内容",
        "searching" => "搜索中",
        "parsing" => "整理结果",
        "scanning" => "扫描文件",
        "preflight" => "检查变更",
        "applying" | "committing" => "提交变更",
        _ => phase,
    }
}

pub(crate) fn help(theme: TerminalTheme) -> String {
    let title = theme.strong("交互命令");
    let command = |value: &str| theme.accent(value);
    format!(
        "\n  {title}\n\
         \x20 {}\n\
         \x20   {:<16} {}\n\
         \x20   {:<16} {}\n\
         \x20   {:<16} {}\n\
         \x20   {:<16} {}\n\
         \x20   {:<16} {}\n\
         \x20   {:<16} {}\n",
        theme.muted("────────────────────────────────────────"),
        command("/help"),
        "显示此帮助",
        command("/new"),
        "开始新会话",
        command("/resume [ID]"),
        "浏览或恢复历史会话",
        command("/model [NAME]"),
        "选择或切换当前模型",
        command("/turns N"),
        "设置最大 Agent 循环次数",
        command("/exit"),
        "退出 XDUDU",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 彩色提示符包含_ansi_且可关闭() {
        assert!(prompt(TerminalTheme::new(true)).contains("\x1b["));
        assert_eq!(prompt(TerminalTheme::new(false)), "❯ ");
    }

    #[test]
    fn deepseek_兼容别名展示为具体型号() {
        assert_eq!(
            model_display_name("deepseek", "deepseek-chat"),
            "DeepSeek-V4-Flash（非思考）"
        );
        assert_eq!(
            model_display_name("deepseek", "deepseek-v4-pro"),
            "DeepSeek-V4-Pro"
        );
    }

    #[test]
    fn 工具与阶段使用面向用户的名称() {
        assert_eq!(tool_display_name("web_search"), "联网搜索");
        assert_eq!(tool_phase_display("parsing"), "整理结果");
        assert_eq!(tool_display_name("custom_tool"), "custom_tool");
    }

    #[test]
    fn deepseek_只列出当前官方_v4_模型() {
        let options = model_options("DeepSeek", "deepseek-chat");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].id, "deepseek-v4-flash");
        assert_eq!(options[1].id, "deepseek-v4-pro");
        assert!(model_matches(&options[0].id, "deepseek-chat"));
    }
}
