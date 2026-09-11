use crate::{Error, Event, EventSink, Message, Model, ModelRequest, Result, Session, ToolRegistry};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct LoopConfig {
    pub max_turns: usize,
    pub max_model_attempts: u32,
    pub retry_base_delay: Duration,
    pub temperature: f32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_model_attempts: 3,
            retry_base_delay: Duration::from_millis(250),
            temperature: 0.2,
        }
    }
}

#[derive(Clone, Default)]
pub struct StopToken(Arc<AtomicBool>);

impl StopToken {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct AgentLoop {
    model: Arc<dyn Model>,
    tools: ToolRegistry,
    system_prompt: String,
    config: LoopConfig,
}

impl AgentLoop {
    pub fn new(
        model: Arc<dyn Model>,
        tools: ToolRegistry,
        system_prompt: impl Into<String>,
    ) -> Self {
        Self {
            model,
            tools,
            system_prompt: system_prompt.into(),
            config: LoopConfig::default(),
        }
    }

    pub fn with_config(mut self, config: LoopConfig) -> Self {
        self.config = config;
        self
    }

    pub async fn run(
        &self,
        session: &mut Session,
        prompt: impl Into<String>,
        events: &dyn EventSink,
        stop: &StopToken,
    ) -> Result<String> {
        events.emit(Event::RunStarted {
            session_id: session.id.clone(),
        });

        if session.messages.is_empty() {
            session.messages.push(Message::System {
                content: self.system_prompt.clone(),
            });
        }
        session.messages.push(Message::User {
            content: prompt.into(),
        });

        let result = self.run_inner(session, events, stop).await;
        match &result {
            Ok(output) => events.emit(Event::RunCompleted {
                session_id: session.id.clone(),
                output: output.clone(),
            }),
            Err(error) => events.emit(Event::RunFailed {
                session_id: session.id.clone(),
                error: error.to_string(),
            }),
        }
        result
    }

    async fn run_inner(
        &self,
        session: &mut Session,
        events: &dyn EventSink,
        stop: &StopToken,
    ) -> Result<String> {
        for turn in 0..self.config.max_turns {
            if stop.is_stopped() {
                return Err(Error::Stopped);
            }
            events.emit(Event::TurnStarted { turn });

            let request = ModelRequest {
                messages: session.messages.clone(),
                tools: self.tools.definitions(),
                temperature: self.config.temperature,
            };
            let response = self
                .complete_with_retry(request, turn, events, stop)
                .await?;
            session.messages.push(Message::Assistant {
                content: response.content.clone(),
                tool_calls: response.tool_calls.clone(),
            });

            if response.tool_calls.is_empty() {
                return Ok(response.content);
            }

            // Robot commands are intentionally serialized. Parallel execution
            // can invalidate observations and safety checks between commands.
            for call in response.tool_calls {
                if stop.is_stopped() {
                    return Err(Error::Stopped);
                }
                events.emit(Event::ToolStarted {
                    turn,
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                });
                let result = match self.tools.get(&call.name) {
                    Some(tool) => tool.execute(call.arguments).await,
                    None => Err(Error::Tool(format!(
                        "requested tool `{}` is not registered",
                        call.name
                    ))),
                };
                let (content, metadata, is_error) = match result {
                    Ok(output) => (output.content, output.metadata, false),
                    Err(error) => (error.to_string(), json!({"error": error.to_string()}), true),
                };
                events.emit(Event::ToolCompleted {
                    turn,
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    is_error,
                    metadata,
                });
                session.messages.push(Message::Tool {
                    call_id: call.id,
                    name: call.name,
                    content,
                    is_error,
                });
            }
        }
        Err(Error::MaxTurns(self.config.max_turns))
    }

    async fn complete_with_retry(
        &self,
        request: ModelRequest,
        turn: usize,
        events: &dyn EventSink,
        stop: &StopToken,
    ) -> Result<crate::ModelResponse> {
        let attempts = self.config.max_model_attempts.max(1);
        let mut last_error = None;
        for attempt in 1..=attempts {
            if stop.is_stopped() {
                return Err(Error::Stopped);
            }
            events.emit(Event::ModelAttempt { turn, attempt });
            match self.model.complete(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => last_error = Some(error),
            }
            if attempt < attempts {
                let multiplier = 2u32.saturating_pow(attempt - 1);
                tokio::time::sleep(self.config.retry_base_delay.saturating_mul(multiplier)).await;
            }
        }
        Err(last_error.unwrap_or_else(|| Error::Model("model call failed".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelResponse, Tool, ToolCall, ToolDefinition, ToolOutput};
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::{collections::VecDeque, sync::Mutex};

    struct ScriptedModel(Mutex<VecDeque<ModelResponse>>);

    #[async_trait]
    impl Model for ScriptedModel {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Model("script exhausted".into()))
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "Echo a value".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
            Ok(ToolOutput::text(arguments.to_string()))
        }
    }

    #[tokio::test]
    async fn executes_tools_and_returns_the_final_response() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([
            ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: json!({"value": 42}),
                }],
            },
            ModelResponse {
                content: "done".into(),
                tool_calls: vec![],
            },
        ]))));
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool).unwrap();
        let agent = AgentLoop::new(model, tools, "test");
        let mut session = Session::new("test-session");

        let result = agent
            .run(
                &mut session,
                "go",
                &crate::NoopEventSink,
                &StopToken::default(),
            )
            .await
            .unwrap();

        assert_eq!(result, "done");
        assert!(matches!(session.messages[3], Message::Tool { .. }));
    }
}
