//! Model-agnostic agent-loop primitives for embodied systems.

mod error;
mod event;
mod model;
mod orchestrator;
mod session;
mod stop;
mod tool;

pub use error::{Error, ModelError, Result, Retryability};
pub use event::{Event, EventSink, NoopEventSink};
pub use model::{
    DeltaSink, FinishReason, Model, ModelRequest, ModelResponse, NoopDeltaSink, Reasoning,
    Sampling, StreamAssembler, StreamDelta, Usage,
};
pub use orchestrator::{AgentLoop, LoopConfig};
pub use session::{Content, ContentPart, ImageSource, Message, Session};
pub use stop::StopToken;
pub use tool::{validate_tool_calls, Tool, ToolCall, ToolDefinition, ToolOutput, ToolRegistry};
