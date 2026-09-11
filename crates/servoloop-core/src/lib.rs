//! Model-agnostic agent-loop primitives for embodied systems.

mod error;
mod event;
mod model;
mod orchestrator;
mod session;
mod tool;

pub use error::{Error, Result};
pub use event::{Event, EventSink, NoopEventSink};
pub use model::{Model, ModelRequest, ModelResponse};
pub use orchestrator::{AgentLoop, LoopConfig, StopToken};
pub use session::{Message, Session};
pub use tool::{Tool, ToolCall, ToolDefinition, ToolOutput, ToolRegistry};
