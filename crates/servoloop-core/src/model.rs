use crate::{Message, Result, ToolDefinition};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub temperature: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelResponse {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<crate::ToolCall>,
}

#[async_trait]
pub trait Model: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse>;
}
