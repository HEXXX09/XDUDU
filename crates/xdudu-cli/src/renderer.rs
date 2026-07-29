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

    fn style(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
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
        match event {
            AgentEvent::AssistantDelta { text } if self.stream => {
                self.emitted_assistant.store(true, Ordering::Release);
                print!("{}", redact_text(&text));
                let _ = io::stdout().flush();
            }
            AgentEvent::ToolStarted { name, .. } => {
                eprintln!("\n  {} {name}", self.style("36", "→"));
            }
            AgentEvent::ToolProgress {
                name,
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
                eprintln!("  {} {name}/{phase}{count}{message}", self.style("36", "·"));
            }
            AgentEvent::ToolFinished { name, result, .. } => {
                let marker = if result.success {
                    self.style("32", "✓")
                } else {
                    self.style("31", "✗")
                };
                eprintln!("  {marker} {name}（{} ms）", result.duration_ms);
            }
            AgentEvent::Warning { message, .. } => {
                eprintln!("  {} {}", self.style("33", "警告："), redact_text(&message));
            }
            _ => {}
        }
    }
}
