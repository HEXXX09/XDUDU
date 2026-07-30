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

pub(crate) fn prompt(theme: TerminalTheme) -> String {
    format!("{} ", theme.accent("❯"))
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
         \x20   {:<16} {}\n",
        theme.muted("────────────────────────────────────────"),
        command("/help"),
        "显示此帮助",
        command("/new"),
        "开始新会话",
        command("/model NAME"),
        "切换当前模型",
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
}
