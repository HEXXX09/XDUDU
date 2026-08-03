//! 终端与 JSON Lines Renderer；核心事件不直接依赖 stdout。

use std::{
    io::{self, Write},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use serde_json::json;
use xdudu_core::{AgentEvent, AgentRunResult, EventSink, XduduError, redact_text, redact_value};

use crate::ui::TerminalTheme;

pub struct ConsoleRenderer {
    json: bool,
    stream: bool,
    color: bool,
    emitted_assistant: AtomicBool,
    output_lock: Mutex<()>,
}

impl ConsoleRenderer {
    pub fn new(json: bool, stream: bool, color: bool) -> Self {
        Self {
            json,
            stream,
            color,
            emitted_assistant: AtomicBool::new(false),
            output_lock: Mutex::new(()),
        }
    }

    pub fn begin_run(&self) {
        self.emitted_assistant.store(false, Ordering::Release);
    }

    pub fn finish_run(&self, result: &AgentRunResult) -> Result<(), XduduError> {
        let _guard = self.output_lock.lock().unwrap();
        if self.json {
            let value = redact_value(&json!({
                "type": "run_completed",
                "sessionId": result.session_id,
                "status": result.status,
                "turns": result.turns,
                "finalMessage": result.final_message,
                "exitCode": result.exit_code,
            }));
            println!(
                "{}",
                serde_json::to_string(&value).map_err(XduduError::from)?
            );
        } else if !self.stream || !self.emitted_assistant.load(Ordering::Acquire) {
            if !result.final_message.is_empty() {
                println!("\n{}", redact_text(&result.final_message));
            }
        } else {
            println!();
        }
        Ok(())
    }
}

#[async_trait]
impl EventSink for ConsoleRenderer {
    async fn emit(&self, event: AgentEvent) {
        let _guard = self.output_lock.lock().unwrap();
        if self.json {
            let event = serde_json::to_value(&event)
                .map(|value| redact_value(&value))
                .and_then(|value| serde_json::to_string(&value));
            if let Ok(line) = event {
                println!("{line}");
            }
            return;
        }
        let theme = TerminalTheme::new(self.color);
        match event {
            AgentEvent::AssistantDelta { text } if self.stream => {
                self.emitted_assistant.store(true, Ordering::Release);
                print!("{}", redact_text(&text));
                let _ = io::stdout().flush();
            }
            AgentEvent::ToolStarted { name, .. } => {
                eprintln!("\n  {} {}", theme.accent("◆"), theme.strong(&name));
            }
            AgentEvent::ToolProgress {
                name: _,
                phase,
                completed,
                total,
                unit,
                message,
                ..
            } => {
                let count = match (completed, total, unit.as_deref()) {
                    (Some(completed), Some(total), Some(unit)) => {
                        format!(" {completed}/{total} {unit}")
                    }
                    (Some(completed), None, Some(unit)) => format!(" {completed} {unit}"),
                    _ => String::new(),
                };
                let message = message
                    .as_deref()
                    .map(redact_text)
                    .map(|message| format!("：{message}"))
                    .unwrap_or_default();
                eprintln!("  {} {phase}{count}{message}", theme.muted("│"));
            }
            AgentEvent::ToolFinished { name, result, .. } => {
                let marker = if result.success {
                    theme.success("✓")
                } else {
                    theme.danger("✗")
                };
                eprintln!(
                    "  {} {marker} {name} {}",
                    theme.muted("└"),
                    theme.muted(&format!("{} ms", result.duration_ms))
                );
            }
            AgentEvent::Warning { message, .. } => {
                eprintln!("  {} {}", theme.warning("⚠ 警告"), redact_text(&message));
            }
            AgentEvent::PlanStarted { revision, .. } => {
                eprintln!("  {} 开始执行计划 revision {revision}", theme.accent("◆"));
            }
            AgentEvent::PlanStepStarted { title, attempt, .. } => {
                eprintln!(
                    "  {} {} {}",
                    theme.accent("→"),
                    redact_text(&title),
                    theme.muted(&format!("（第 {attempt} 次尝试）"))
                );
            }
            AgentEvent::PlanStepCompleted { summary, .. } => {
                eprintln!("  {} {}", theme.success("✓"), redact_text(&summary));
            }
            AgentEvent::PlanStepFailed { error, .. } => {
                eprintln!("  {} {}", theme.danger("✗"), redact_text(&error));
            }
            AgentEvent::PlanPaused { reason, .. } => {
                eprintln!(
                    "  {} {}",
                    theme.warning("Ⅱ 计划已暂停"),
                    redact_text(&reason)
                );
            }
            AgentEvent::PlanCompleted { .. } => {
                eprintln!("  {} 计划全部完成", theme.success("✓"));
            }
            _ => {}
        }
    }
}
