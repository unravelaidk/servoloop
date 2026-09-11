use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    RunStarted {
        session_id: String,
    },
    TurnStarted {
        turn: usize,
    },
    ModelAttempt {
        turn: usize,
        attempt: u32,
    },
    ModelRetry {
        turn: usize,
        attempt: u32,
        error: String,
        retryable: bool,
    },
    ModelStreamStarted {
        turn: usize,
    },
    ModelStreamDelta {
        turn: usize,
        kind: String,
    },
    ToolStarted {
        turn: usize,
        call_id: String,
        tool: String,
    },
    ToolCompleted {
        turn: usize,
        call_id: String,
        tool: String,
        is_error: bool,
        metadata: Value,
    },
    Reconciled {
        unknown: usize,
    },
    RunCompleted {
        session_id: String,
        output: String,
    },
    RunFailed {
        session_id: String,
        error: String,
    },
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
}

impl<F> EventSink for F
where
    F: Fn(Event) + Send + Sync,
{
    fn emit(&self, event: Event) {
        self(event);
    }
}

#[derive(Debug, Default)]
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit(&self, _event: Event) {}
}
