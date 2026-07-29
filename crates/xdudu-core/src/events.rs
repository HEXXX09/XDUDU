//! Agent 领域事件；核心只发布事件，终端表现由调用方决定。

use async_trait::async_trait;
use serde::Serialize;

use crate::{provider::TokenUsage, session::AgentLoopState, tools::ToolResult};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    StateChanged {
        state: AgentLoopState,
    },
    AssistantDelta {
        text: String,
    },
    ToolStarted {
        call_id: String,
        name: String,
    },
    ToolProgress {
        call_id: String,
        name: String,
        phase: String,
        completed: Option<u64>,
        total: Option<u64>,
        unit: Option<String>,
        message: Option<String>,
    },
    ToolFinished {
        call_id: String,
        name: String,
        result: ToolResult,
    },
    UsageUpdated {
        usage: TokenUsage,
    },
    Warning {
        code: String,
        message: String,
    },
}

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: AgentEvent);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEventSink;

#[async_trait]
impl EventSink for NoopEventSink {
    async fn emit(&self, _event: AgentEvent) {}
}

pub(crate) async fn emit(sink: Option<&dyn EventSink>, event: AgentEvent) {
    if let Some(sink) = sink {
        sink.emit(event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 事件序列化为稳定的_json_类型() {
        let value = serde_json::to_value(AgentEvent::AssistantDelta {
            text: "你好".into(),
        })
        .unwrap();
        assert_eq!(value["type"], "assistant_delta");
        assert_eq!(value["text"], "你好");
    }

    #[test]
    fn 工具进度序列化为稳定事件() {
        let value = serde_json::to_value(AgentEvent::ToolProgress {
            call_id: "call-1".into(),
            name: "search_text".into(),
            phase: "scanning".into(),
            completed: Some(1000),
            total: None,
            unit: Some("files".into()),
            message: None,
        })
        .unwrap();
        assert_eq!(value["type"], "tool_progress");
        assert_eq!(value["call_id"], "call-1");
        assert_eq!(value["completed"], 1000);
        assert_eq!(value["unit"], "files");
    }
}
