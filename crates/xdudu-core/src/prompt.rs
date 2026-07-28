//! Agent 系统提示词。

use std::path::Path;

use crate::tools::ToolDefinition;

pub fn build_system_prompt(tools: &[ToolDefinition], cwd: &Path) -> String {
    let descriptions = tools
        .iter()
        .map(|tool| {
            let schema =
                serde_json::to_string_pretty(&tool.input_schema).unwrap_or_else(|_| "{}".into());
            format!(
                "### {}\n{}\n\n输入 Schema：\n```json\n{schema}\n```",
                tool.name, tool.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "你是 XDUDU，一个运行在终端中的 AI 编程助手。\n\n\
         你帮助开发者读取和修改代码、运行命令、调试并测试软件。你只能通过已提供工具操作当前工作区，必须遵守权限和安全限制。\n\n\
         ## 当前工作目录\n{}\n\n\
         ## 可用工具\n{}\n\n\
         ## 响应规则\n\
         - 先判断任务所需工具，再执行。\n\
         - 查看文件使用 file_read，修改文件使用 file_write，运行命令使用 terminal_exec。\n\
         - 工具被拒绝后不得尝试绕过权限。\n\
         - 任务完成后用简洁文本总结实际结果。\n\n\
         ## 安全规则\n\
         - 不执行未经明确授权的破坏性命令。\n\
         - 修改文件前先读取并尽量提供 expectedSha256。\n\
         - 修改后运行相关测试。\n\
         - 不输出密钥、API Key、密码或其他敏感信息。",
        cwd.display(),
        descriptions
    )
}
