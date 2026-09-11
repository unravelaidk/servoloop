//! Cancellation-aware agent loop with bounded retries, deadlines, and
//! conservative tool dispatch.

use crate::{
    validate_tool_calls, DeltaSink, Error, Event, EventSink, Message, Model, ModelRequest,
    ModelResponse, Result, Retryability, Sampling, Session, StopToken, StreamDelta, ToolCall,
    ToolRegistry,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Configuration for the agent loop.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// Maximum number of model turns before the loop aborts with
    /// [`Error::MaxTurns`].
    pub max_turns: usize,
    /// Maximum retry attempts per model call. Only transient (retryable)
    /// model errors are retried.
    pub max_model_attempts: u32,
    /// Base delay for exponential backoff between retries.
    pub retry_base_delay: Duration,
    /// Maximum delay cap for backoff.
    pub retry_max_delay: Duration,
    /// Default per-attempt deadline for model calls when the request does
    /// not set one. `None` disables the per-attempt timeout.
    pub model_deadline: Option<Duration>,
    /// Per-tool execution deadline. `None` disables the tool timeout.
    pub tool_deadline: Option<Duration>,
    /// Maximum number of tool calls the model may make in a single response.
    /// Responses exceeding this are rejected.
    pub max_tool_calls_per_response: usize,
    /// Maximum total tool output length in **bytes** across one turn (uses
    /// `String::len`, which counts UTF-8 bytes, not Unicode characters). A
    /// safety valve against runaway tool spam.
    pub max_output_bytes: usize,
    /// Default sampling parameters applied when the caller does not override.
    pub sampling: Sampling,
    /// Default max output budget (completion tokens) forwarded to the model.
    pub max_output: Option<u32>,
    /// Whether to use streaming when the model supports it. When `false`,
    /// the loop always calls [`Model::complete`]. When `true` and the model
    /// reports [`Model::can_stream`], it calls [`Model::stream`].
    pub prefer_streaming: bool,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_model_attempts: 3,
            retry_base_delay: Duration::from_millis(250),
            retry_max_delay: Duration::from_secs(10),
            model_deadline: Some(Duration::from_secs(30)),
            tool_deadline: Some(Duration::from_secs(10)),
            max_tool_calls_per_response: 16,
            max_output_bytes: 100_000,
            sampling: Sampling::new(),
            max_output: None,
            prefer_streaming: false,
        }
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

        if stop.is_stopped() {
            self.emit_failed(events, &session.id, &Error::Stopped);
            return Err(Error::Stopped);
        }

        // Reconcile any interrupted tool history before starting. This runs
        // before the first model request and never invokes tools.
        match session.reconcile_tool_history() {
            Ok(newly_unknown) => {
                if newly_unknown > 0 {
                    events.emit(Event::Reconciled {
                        unknown: newly_unknown,
                    });
                }
            }
            Err(e) => {
                self.emit_failed(events, &session.id, &e);
                return Err(e);
            }
        }

        // If the session has ANY unresolved ToolUnknown (whether pre-existing
        // from a prior run or just inserted), halt. The caller must replace
        // each unknown with a real resolved result before resuming.
        if session.has_unresolved_unknowns() {
            let err = Error::Reconciliation(
                "session has unresolved ToolUnknown results; \
                 caller must reconcile before resuming"
                    .into(),
            );
            self.emit_failed(events, &session.id, &err);
            return Err(err);
        }

        if session.messages.is_empty() {
            session.messages.push(Message::System {
                content: self.system_prompt.clone(),
            });
        }
        session.messages.push(Message::user_text(prompt.into()));

        let result = self.run_inner(session, events, stop).await;

        // After the run (whether success or error), reconcile so that any
        // unmatched tool calls become explicit unknowns in the history.
        // This ensures the history always reflects outcomes explicitly.
        match session.reconcile_tool_history() {
            Ok(newly_unknown) => {
                if newly_unknown > 0 {
                    events.emit(Event::Reconciled {
                        unknown: newly_unknown,
                    });
                }
            }
            Err(e) => {
                // If reconciliation after error fails (e.g. duplicate results),
                // surface that error instead of the original if the original
                // was already an error, or return it alongside.
                if result.is_ok() {
                    self.emit_failed(events, &session.id, &e);
                    return Err(e);
                }
            }
        }

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

    fn emit_failed(&self, events: &dyn EventSink, session_id: &str, error: &Error) {
        events.emit(Event::RunFailed {
            session_id: session_id.to_string(),
            error: error.to_string(),
        });
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

            let request = self.build_request(session);
            let response = self
                .complete_with_retry(request, turn, events, stop)
                .await?;

            // Check for cancellation before accepting the response.
            if stop.is_stopped() {
                return Err(Error::Stopped);
            }

            // Validate the complete response before appending it.
            self.validate_response(&response)?;

            // Collect existing tool-call IDs from history BEFORE appending
            // the new assistant message, so validation checks against all
            // prior turns.
            let existing_ids = session.existing_tool_call_ids();

            // Validate the full batch of tool calls before any side effects
            // and before appending the assistant message. This checks:
            // - unique non-empty IDs within the batch
            // - IDs not reused from prior turns
            // - registered tool names
            if !response.tool_calls.is_empty() {
                validate_tool_calls(&response.tool_calls, &self.tools, &existing_ids)?;
                if response.tool_calls.len() > self.config.max_tool_calls_per_response {
                    return Err(Error::InvalidInput(format!(
                        "model returned {} tool calls, exceeding the per-response limit of {}",
                        response.tool_calls.len(),
                        self.config.max_tool_calls_per_response
                    )));
                }
            }

            // Now safe to append the assistant message (with reasoning).
            session
                .messages
                .push(Message::assistant_from_response(&response));

            if response.tool_calls.is_empty() {
                return Ok(response.content);
            }

            // Execute tools. If any tool returns Stopped or DeadlineExceeded,
            // the loop marks that call as ToolUnknown and halts immediately —
            // no subsequent tools, no subsequent model turn.
            self.execute_tools(&response.tool_calls, turn, session, events, stop)
                .await?;
        }
        Err(Error::MaxTurns(self.config.max_turns))
    }

    fn build_request(&self, session: &Session) -> ModelRequest {
        let mut request = ModelRequest::new(session.id.clone(), session.messages.clone());
        request.tools = self.tools.definitions();
        request.max_output = self.config.max_output;
        request.sampling = Some(self.config.sampling.clone());
        request.deadline = self.config.model_deadline;
        request
    }

    fn validate_response(&self, response: &ModelResponse) -> Result<()> {
        // Reject empty/whitespace-only responses with no tool calls — these
        // are false successes.
        if response.tool_calls.is_empty() && response.content.trim().is_empty() {
            return Err(Error::Model(
                "model returned an empty or whitespace-only response with no tool calls".into(),
            ));
        }
        // Reject non-dispatchable finish reasons. MaxOutput means truncated
        // output; ContentFilter means filtered; Other is unrecognised. We
        // never act on truncated or filtered output.
        if !response.finish_reason.is_dispatchable() {
            return Err(Error::Model(format!(
                "model finished with non-dispatchable reason: {:?}",
                response.finish_reason
            )));
        }
        Ok(())
    }

    /// Execute tools sequentially. If any tool returns
    /// [`Error::Stopped`] or [`Error::DeadlineExceeded`], the loop:
    /// 1. Marks that tool call as `ToolUnknown` in the session history.
    /// 2. Halts immediately — no subsequent tools in the batch execute, no
    ///    subsequent model turn runs.
    ///
    /// This prevents treating a cancelled/timed-out tool as ordinary error
    /// feedback. The physical state is unknown and the caller must
    /// reconcile before resuming.
    async fn execute_tools(
        &self,
        calls: &[ToolCall],
        turn: usize,
        session: &mut Session,
        events: &dyn EventSink,
        stop: &StopToken,
    ) -> Result<()> {
        let mut total_output_bytes = 0usize;
        for call in calls {
            if stop.is_stopped() {
                return Err(Error::Stopped);
            }
            events.emit(Event::ToolStarted {
                turn,
                call_id: call.id.clone(),
                tool: call.name.clone(),
            });

            let result = self
                .execute_single_tool(call, stop, self.config.tool_deadline)
                .await;

            match result {
                Ok(output) => {
                    total_output_bytes += output.content.len();
                    if total_output_bytes > self.config.max_output_bytes {
                        // Mark this call and halt.
                        session.messages.push(Message::ToolUnknown {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            reason: "tool output exceeded the byte limit".into(),
                        });
                        return Err(Error::Tool(format!(
                            "total tool output exceeded the {max_bytes} byte limit",
                            max_bytes = self.config.max_output_bytes
                        )));
                    }
                    events.emit(Event::ToolCompleted {
                        turn,
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        is_error: false,
                        metadata: output.metadata,
                    });
                    session.messages.push(Message::Tool {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        content: output.content,
                        is_error: false,
                    });
                }
                Err(Error::Stopped) => {
                    // Cancellation: mark unknown, halt immediately.
                    session.messages.push(Message::ToolUnknown {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        reason: "tool was cancelled (stop token fired)".into(),
                    });
                    events.emit(Event::ToolCompleted {
                        turn,
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        is_error: true,
                        metadata: json!({"error": "cancelled", "unknown_outcome": true}),
                    });
                    return Err(Error::Stopped);
                }
                Err(Error::DeadlineExceeded) => {
                    // Deadline: mark unknown, halt immediately.
                    session.messages.push(Message::ToolUnknown {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        reason: "tool deadline exceeded".into(),
                    });
                    events.emit(Event::ToolCompleted {
                        turn,
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        is_error: true,
                        metadata: json!({"error": "deadline exceeded", "unknown_outcome": true}),
                    });
                    return Err(Error::DeadlineExceeded);
                }
                Err(error) => {
                    // Ordinary tool error: record as feedback, continue.
                    let msg = error.to_string();
                    events.emit(Event::ToolCompleted {
                        turn,
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        is_error: true,
                        metadata: json!({"error": msg}),
                    });
                    session.messages.push(Message::Tool {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        content: msg,
                        is_error: true,
                    });
                }
            }
        }
        Ok(())
    }

    /// Execute a single tool with a deadline, racing against cancellation.
    ///
    /// # Cancellation caveat
    ///
    /// Dropping the tool future (via timeout or stop-token race) cancels the
    /// Rust-side future but does **not** stop any external effect the tool
    /// has already dispatched (e.g. robot motion). The caller must treat a
    /// cancelled tool as an unknown-outcome and reconcile the physical state
    /// before resuming. Never assume cancelling the future halts motion.
    async fn execute_single_tool(
        &self,
        call: &ToolCall,
        stop: &StopToken,
        deadline: Option<Duration>,
    ) -> Result<crate::ToolOutput> {
        let tool = self.tools.get(&call.name).ok_or_else(|| {
            Error::Tool(format!("requested tool `{}` is not registered", call.name))
        })?;

        let exec = tool.execute(call.arguments.clone());

        let race = async {
            match deadline {
                Some(d) => tokio::time::timeout(d, exec)
                    .await
                    .map_err(|_| Error::DeadlineExceeded)?,
                None => exec.await,
            }
        };
        tokio::select! {
            biased;
            _ = stop.cancelled() => Err(Error::Stopped),
            res = race => res,
        }
    }

    async fn complete_with_retry(
        &self,
        request: ModelRequest,
        turn: usize,
        events: &dyn EventSink,
        stop: &StopToken,
    ) -> Result<ModelResponse> {
        let attempts = self.config.max_model_attempts.max(1);
        let mut last_error: Option<Error> = None;
        for attempt in 1..=attempts {
            if stop.is_stopped() {
                return Err(Error::Stopped);
            }
            events.emit(Event::ModelAttempt { turn, attempt });

            let result = self.call_model(&request, turn, events, stop).await;

            match result {
                Ok(response) => {
                    if stop.is_stopped() {
                        return Err(Error::Stopped);
                    }
                    return Ok(response);
                }
                Err(error) => {
                    let retryable = error.is_retryable();
                    events.emit(Event::ModelRetry {
                        turn,
                        attempt,
                        error: error.to_string(),
                        retryable,
                    });

                    if !retryable {
                        return Err(error);
                    }

                    let retry_after = match &error {
                        Error::ModelTyped(me) => match me.retryability() {
                            Retryability::Transient {
                                retry_after: Some(d),
                            } => Some(*d),
                            _ => None,
                        },
                        _ => None,
                    };

                    last_error = Some(error);

                    if attempt < attempts {
                        let delay = if let Some(after) = retry_after {
                            after.min(self.config.retry_max_delay)
                        } else {
                            let multiplier = 2u32.saturating_pow(attempt - 1);
                            let base = self.config.retry_base_delay.saturating_mul(multiplier);
                            base.min(self.config.retry_max_delay)
                        };
                        if !self.sleep_interruptible(delay, stop).await {
                            return Err(Error::Stopped);
                        }
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::Model("model call failed".into())))
    }

    async fn call_model(
        &self,
        request: &ModelRequest,
        turn: usize,
        events: &dyn EventSink,
        stop: &StopToken,
    ) -> Result<ModelResponse> {
        let use_stream = self.config.prefer_streaming && self.model.can_stream();
        let deadline = request.deadline.or(self.config.model_deadline);

        let call_fut = async {
            if use_stream {
                events.emit(Event::ModelStreamStarted { turn });
                let mut sink = CountingDeltaSink {
                    inner: events,
                    turn,
                };
                self.model.stream(request.clone(), &mut sink).await
            } else {
                self.model.complete(request.clone()).await
            }
        };

        tokio::select! {
            biased;
            _ = stop.cancelled() => Err(Error::Stopped),
            res = async {
                match deadline {
                    Some(d) => tokio::time::timeout(d, call_fut).await
                        .map_err(|_| Error::DeadlineExceeded)?,
                    None => call_fut.await,
                }
            } => res,
        }
    }

    async fn sleep_interruptible(&self, delay: Duration, stop: &StopToken) -> bool {
        if delay.is_zero() {
            return !stop.is_stopped();
        }
        tokio::select! {
            biased;
            _ = stop.cancelled() => false,
            _ = tokio::time::sleep(delay) => true,
        }
    }
}

/// A [`DeltaSink`] that forwards a summary of each delta to the event sink
/// while the loop consumes a stream.
struct CountingDeltaSink<'a> {
    inner: &'a dyn EventSink,
    turn: usize,
}

impl DeltaSink for CountingDeltaSink<'_> {
    fn on_delta(&mut self, delta: StreamDelta) {
        let kind = match &delta {
            StreamDelta::Text { .. } => "text",
            StreamDelta::ToolCall { .. } => "tool_call",
            StreamDelta::Usage { .. } => "usage",
            StreamDelta::Reasoning { .. } => "reasoning",
        };
        self.inner.emit(Event::ModelStreamDelta {
            turn: self.turn,
            kind: kind.to_string(),
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DeltaSink, FinishReason, ModelError, ModelRequest, ModelResponse, NoopEventSink, Reasoning,
        StreamAssembler, StreamDelta, Tool, ToolDefinition, ToolOutput, Usage,
    };
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicBool, AtomicU32, Ordering},
            Arc, Mutex,
        },
    };

    // ---- Test helpers ----------------------------------------------------

    struct ScriptedModel(Mutex<VecDeque<Result<ModelResponse>>>);

    #[async_trait]
    impl Model for ScriptedModel {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(Error::Model("script exhausted".into())))
        }
    }

    struct ScriptedModelWithCalls {
        responses: Mutex<VecDeque<Result<ModelResponse>>>,
        calls: AtomicU32,
    }

    impl ScriptedModelWithCalls {
        fn new(responses: Vec<Result<ModelResponse>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                calls: AtomicU32::new(0),
            }
        }

        fn call_count(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Model for ScriptedModelWithCalls {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(Error::Model("script exhausted".into())))
        }
    }

    /// A model that counts all calls and can be inspected after the run.
    struct CountingModel {
        responses: Mutex<VecDeque<Result<ModelResponse>>>,
        calls: AtomicU32,
    }

    impl CountingModel {
        fn new(responses: Vec<Result<ModelResponse>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                calls: AtomicU32::new(0),
            }
        }

        fn call_count(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Model for CountingModel {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(Error::Model("script exhausted".into())))
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

    fn make_agent(model: Arc<dyn Model>, tools: ToolRegistry) -> AgentLoop {
        AgentLoop::new(model, tools, "test system prompt")
    }

    fn basic_tools() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool).unwrap();
        tools
    }

    // ---- End-to-end tool execution ---------------------------------------

    #[tokio::test]
    async fn executes_tools_and_returns_the_final_response() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([
            Ok(ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: json!({"value": 42}),
                }],
                ..Default::default()
            }),
            Ok(ModelResponse::text("done")),
        ]))));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("test-session");

        let result = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap();

        assert_eq!(result, "done");
        assert!(matches!(session.messages[3], Message::Tool { .. }));
    }

    // ---- Metadata propagation --------------------------------------------

    #[tokio::test]
    async fn usage_and_finish_reason_propagate() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: "hello".into(),
                tool_calls: vec![],
                usage: Usage::new(10, 5),
                finish_reason: FinishReason::Stop,
                reasoning: None,
            },
        )]))));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        let result = agent
            .run(&mut session, "hi", &NoopEventSink, &StopToken::new())
            .await
            .unwrap();
        assert_eq!(result, "hello");
    }

    // ---- Reasoning round-trip --------------------------------------------

    #[tokio::test]
    async fn reasoning_preserved_in_assistant_message() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: "thinking".into(),
                tool_calls: vec![],
                usage: Usage::unknown(),
                finish_reason: FinishReason::Stop,
                reasoning: Some(Reasoning {
                    text: Some("step by step I decided to say thinking".into()),
                }),
            },
        )]))));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap();
        // The assistant message should preserve the reasoning.
        let assistant = session
            .messages
            .iter()
            .find(|m| matches!(m, Message::Assistant { .. }))
            .unwrap();
        match assistant {
            Message::Assistant {
                reasoning: Some(r), ..
            } => {
                assert_eq!(
                    r.text.as_deref(),
                    Some("step by step I decided to say thinking")
                );
            }
            _ => panic!("expected reasoning in assistant message"),
        }
        // Verify serde round-trip preserves reasoning.
        let serialized = serde_json::to_string(assistant).unwrap();
        let back: Message = serde_json::from_str(&serialized).unwrap();
        assert_eq!(assistant, &back);
    }

    // ---- Retry: transient errors retry, permanent don't ------------------

    #[tokio::test]
    async fn transient_errors_retry_then_succeed() {
        let model = Arc::new(ScriptedModelWithCalls::new(vec![
            Err(Error::ModelTyped(ModelError::transient("overloaded"))),
            Ok(ModelResponse::text("recovered")),
        ]));
        let agent = make_agent(model.clone(), basic_tools());
        let mut session = Session::new("s1");
        let result = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap();
        assert_eq!(result, "recovered");
        assert_eq!(model.call_count(), 2);
    }

    #[tokio::test]
    async fn permanent_auth_error_no_retry() {
        let model = Arc::new(ScriptedModelWithCalls::new(vec![Err(Error::ModelTyped(
            ModelError::auth("invalid key"),
        ))]));
        let agent = make_agent(model.clone(), basic_tools());
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ModelTyped(_)));
        assert_eq!(model.call_count(), 1);
    }

    #[tokio::test]
    async fn permanent_invalid_error_no_retry() {
        let model = Arc::new(ScriptedModelWithCalls::new(vec![Err(Error::ModelTyped(
            ModelError::invalid("bad request"),
        ))]));
        let agent = make_agent(model.clone(), basic_tools());
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ModelTyped(_)));
        assert_eq!(model.call_count(), 1);
    }

    // ---- Retry: exact attempts, no sleep (zero delay) --------------------

    #[tokio::test]
    async fn retry_exhausts_attempts_with_zero_delay() {
        let model = Arc::new(ScriptedModelWithCalls::new(vec![
            Err(Error::ModelTyped(ModelError::transient("fail 1"))),
            Err(Error::ModelTyped(ModelError::transient("fail 2"))),
            Err(Error::ModelTyped(ModelError::transient("fail 3"))),
        ]));
        let agent = make_agent(model.clone(), basic_tools()).with_config(LoopConfig {
            retry_base_delay: Duration::ZERO,
            max_model_attempts: 3,
            ..LoopConfig::default()
        });
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ModelTyped(_)));
        assert_eq!(model.call_count(), 3);
    }

    // ---- Retry: rate-limit with retry_after -------------------------------

    #[tokio::test]
    async fn rate_limit_uses_retry_after_with_zero_delay() {
        let model = Arc::new(ScriptedModelWithCalls::new(vec![
            Err(Error::ModelTyped(ModelError::rate_limit(
                "slow down",
                Duration::from_millis(0),
            ))),
            Ok(ModelResponse::text("ok")),
        ]));
        let agent = make_agent(model.clone(), basic_tools()).with_config(LoopConfig {
            retry_base_delay: Duration::ZERO,
            ..LoopConfig::default()
        });
        let mut session = Session::new("s1");
        let result = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(model.call_count(), 2);
    }

    // ---- Deadlines --------------------------------------------------------

    #[tokio::test]
    async fn deadline_exceeded_on_hung_model() {
        struct HungModel;
        #[async_trait]
        impl Model for HungModel {
            async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                unreachable!()
            }
        }
        let model = Arc::new(HungModel);
        let agent = make_agent(model, basic_tools()).with_config(LoopConfig {
            model_deadline: Some(Duration::from_millis(50)),
            retry_base_delay: Duration::ZERO,
            max_model_attempts: 1,
            ..LoopConfig::default()
        });
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::DeadlineExceeded));
    }

    // ---- Tool timeout: marks unknown, halts, no subsequent tools ----------

    #[tokio::test]
    async fn tool_deadline_marks_unknown_and_halts_no_subsequent_tools() {
        struct SlowTool;
        let executed = Arc::new(AtomicBool::new(false));

        #[async_trait]
        impl Tool for SlowTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "slow".into(),
                    description: "slow tool".into(),
                    parameters: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                unreachable!()
            }
        }

        struct FastToolWrapper(Arc<AtomicBool>);
        #[async_trait]
        impl Tool for FastToolWrapper {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "fast".into(),
                    description: "fast tool".into(),
                    parameters: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
                self.0.store(true, Ordering::SeqCst);
                Ok(ToolOutput::text("fast"))
            }
        }

        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "call-1".into(),
                        name: "slow".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "call-2".into(),
                        name: "fast".into(),
                        arguments: json!({}),
                    },
                ],
                ..Default::default()
            },
        )]))));
        let mut tools = ToolRegistry::new();
        tools.register(SlowTool).unwrap();
        tools.register(FastToolWrapper(executed.clone())).unwrap();
        let agent = make_agent(model, tools).with_config(LoopConfig {
            tool_deadline: Some(Duration::from_millis(50)),
            ..LoopConfig::default()
        });
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::DeadlineExceeded));
        // call-1 should be marked ToolUnknown, not a normal Tool error.
        assert!(session.messages.iter().any(|m| matches!(
            m,
            Message::ToolUnknown { call_id, reason, .. }
            if call_id == "call-1" && reason.contains("deadline")
        )));
        // call-2 must NOT have executed.
        assert!(
            !executed.load(Ordering::SeqCst),
            "second tool must not execute after first tool times out"
        );
    }

    // ---- Old tool_deadline_exceeded test (updated for new semantics) -----

    #[tokio::test]
    async fn tool_deadline_exceeded_marks_unknown() {
        struct SlowTool;
        #[async_trait]
        impl Tool for SlowTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "slow".into(),
                    description: "slow tool".into(),
                    parameters: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                unreachable!()
            }
        }
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "slow".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
        )]))));
        let mut tools = ToolRegistry::new();
        tools.register(SlowTool).unwrap();
        let agent = make_agent(model, tools).with_config(LoopConfig {
            tool_deadline: Some(Duration::from_millis(50)),
            ..LoopConfig::default()
        });
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::DeadlineExceeded));
        // The tool should be marked unknown, not an ordinary error.
        assert!(matches!(
            session.messages.last(),
            Some(Message::ToolUnknown { call_id, .. }) if call_id == "call-1"
        ));
    }

    // ---- Cancellation -----------------------------------------------------

    #[tokio::test]
    async fn cancel_pending_model_call() {
        struct GateModel(Arc<tokio::sync::Notify>);
        #[async_trait]
        impl Model for GateModel {
            async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
                self.0.notified().await;
                Ok(ModelResponse::text("late"))
            }
        }
        let notify = Arc::new(tokio::sync::Notify::new());
        let model = Arc::new(GateModel(notify.clone()));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        let stop = StopToken::new();

        let stop_clone = stop.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            stop_clone.stop();
        });

        let err = agent
            .run(&mut session, "go", &NoopEventSink, &stop)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Stopped));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn cancel_during_backoff() {
        let model = Arc::new(ScriptedModelWithCalls::new(vec![
            Err(Error::ModelTyped(ModelError::transient("fail"))),
            Ok(ModelResponse::text("should not reach")),
        ]));
        let agent = make_agent(model.clone(), basic_tools()).with_config(LoopConfig {
            retry_base_delay: Duration::from_secs(60),
            max_model_attempts: 3,
            ..LoopConfig::default()
        });
        let mut session = Session::new("s1");
        let stop = StopToken::new();

        let stop_clone = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            stop_clone.stop();
        });

        let err = agent
            .run(&mut session, "go", &NoopEventSink, &stop)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Stopped));
        assert_eq!(model.call_count(), 1);
    }

    // ---- Cancelled tool: marks unknown, no subsequent tools --------------

    #[tokio::test]
    async fn cancel_pending_tool_marks_unknown_no_subsequent() {
        struct HangingTool;
        let executed = Arc::new(AtomicBool::new(false));

        #[async_trait]
        impl Tool for HangingTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "hang".into(),
                    description: "hang".into(),
                    parameters: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                unreachable!()
            }
        }

        struct FastToolWrapper(Arc<AtomicBool>);
        #[async_trait]
        impl Tool for FastToolWrapper {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "fast".into(),
                    description: "fast".into(),
                    parameters: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
                self.0.store(true, Ordering::SeqCst);
                Ok(ToolOutput::text("fast"))
            }
        }

        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "call-1".into(),
                        name: "hang".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "call-2".into(),
                        name: "fast".into(),
                        arguments: json!({}),
                    },
                ],
                ..Default::default()
            },
        )]))));
        let mut tools = ToolRegistry::new();
        tools.register(HangingTool).unwrap();
        tools.register(FastToolWrapper(executed.clone())).unwrap();
        let agent = make_agent(model, tools);
        let mut session = Session::new("s1");
        let stop = StopToken::new();

        let stop_clone = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            stop_clone.stop();
        });

        let err = agent
            .run(&mut session, "go", &NoopEventSink, &stop)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Stopped));
        // call-1 should be marked ToolUnknown.
        assert!(session.messages.iter().any(|m| matches!(
            m,
            Message::ToolUnknown { call_id, reason, .. }
            if call_id == "call-1" && reason.contains("cancelled")
        )));
        // call-2 must NOT have executed.
        assert!(
            !executed.load(Ordering::SeqCst),
            "second tool must not execute after first tool is cancelled"
        );
    }

    // ---- Batch validation: invalid IDs never execute ---------------------

    #[tokio::test]
    async fn duplicate_tool_call_ids_rejected_before_execution() {
        let executed = Arc::new(AtomicBool::new(false));
        struct CheckTool(Arc<AtomicBool>);
        #[async_trait]
        impl Tool for CheckTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "check".into(),
                    description: "check".into(),
                    parameters: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
                self.0.store(true, Ordering::SeqCst);
                Ok(ToolOutput::text("ran"))
            }
        }
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "dup".into(),
                        name: "check".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "dup".into(),
                        name: "check".into(),
                        arguments: json!({}),
                    },
                ],
                ..Default::default()
            },
        )]))));
        let mut tools = ToolRegistry::new();
        tools.register(CheckTool(executed.clone())).unwrap();
        let agent = make_agent(model, tools);
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
        assert!(
            !executed.load(Ordering::SeqCst),
            "tool must not execute when batch validation fails"
        );
        // Invalid batch must not be persisted as an assistant message.
        assert!(
            !session.messages.iter().any(
                |m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty())
            ),
            "invalid batch assistant message must not be persisted"
        );
    }

    #[tokio::test]
    async fn empty_tool_call_id_rejected_before_execution() {
        let executed = Arc::new(AtomicBool::new(false));
        struct CheckTool(Arc<AtomicBool>);
        #[async_trait]
        impl Tool for CheckTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "check".into(),
                    description: "check".into(),
                    parameters: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
                self.0.store(true, Ordering::SeqCst);
                Ok(ToolOutput::text("ran"))
            }
        }
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: String::new(),
                    name: "check".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
        )]))));
        let mut tools = ToolRegistry::new();
        tools.register(CheckTool(executed.clone())).unwrap();
        let agent = make_agent(model, tools);
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
        assert!(!executed.load(Ordering::SeqCst));
    }

    // ---- Cross-turn reused call ID rejected -------------------------------

    #[tokio::test]
    async fn cross_turn_reused_call_id_rejected() {
        // First run: call-1 executes successfully.
        let model = Arc::new(CountingModel::new(vec![
            Ok(ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            }),
            Ok(ModelResponse::text("done")),
            // Third call (for second run): reuses "call-1".
            Ok(ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(), // reuse from prior turn
                    name: "echo".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            }),
        ]));
        let agent = make_agent(model.clone(), basic_tools());
        let mut session = Session::new("s1");

        // First run succeeds.
        let result = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap();
        assert_eq!(result, "done");

        // Second run: model reuses "call-1" — must be rejected.
        let err = agent
            .run(&mut session, "continue", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::InvalidInput(_)),
            "reusing a call ID from a prior turn must be rejected"
        );
    }

    // ---- No empty/whitespace false success --------------------------------

    #[tokio::test]
    async fn empty_response_rejected() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse::default(),
        )]))));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Model(_)));
    }

    #[tokio::test]
    async fn whitespace_only_response_rejected() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse::text("   \n\t  "),
        )]))));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Model(_)));
    }

    // ---- Non-dispatchable finish reasons rejected -------------------------

    #[tokio::test]
    async fn max_output_finish_reason_rejected() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: "truncated".into(),
                tool_calls: vec![],
                finish_reason: FinishReason::MaxOutput,
                ..Default::default()
            },
        )]))));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Model(_)));
    }

    #[tokio::test]
    async fn content_filter_finish_reason_rejected() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: "filtered".into(),
                tool_calls: vec![],
                finish_reason: FinishReason::ContentFilter,
                ..Default::default()
            },
        )]))));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Model(_)));
    }

    // ---- HALT on existing ToolUnknown -------------------------------------

    #[tokio::test]
    async fn halt_on_existing_tool_unknown_no_model_calls() {
        // Pre-populate a session with a ToolUnknown from a prior run.
        let model = Arc::new(CountingModel::new(vec![]));
        let agent = make_agent(model.clone(), basic_tools());
        let mut session = Session::new("s1");
        session.messages.push(Message::user_text("go"));
        session.messages.push(Message::Assistant {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "echo".into(),
                arguments: json!({}),
            }],
            reasoning: None,
        });
        session.messages.push(Message::ToolUnknown {
            call_id: "call-1".into(),
            name: "echo".into(),
            reason: "prior interruption".into(),
        });

        let err = agent
            .run(&mut session, "continue", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Reconciliation(_)),
            "must halt with Reconciliation error when ToolUnknown exists"
        );
        assert_eq!(
            model.call_count(),
            0,
            "no model calls when unresolved ToolUnknown exists"
        );
    }

    // ---- Interrupted history reconciliation: no replay, then halt ---------

    #[tokio::test]
    async fn interrupted_history_marked_unknown_then_halts() {
        // Simulate a session where the assistant called a tool but no
        // result was recorded (interrupted run). Reconciliation marks it
        // unknown, then the loop halts with Reconciliation.
        let model = Arc::new(CountingModel::new(vec![Ok(ModelResponse::text(
            "should not reach",
        ))]));
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool).unwrap();
        let agent = make_agent(model.clone(), tools);
        let mut session = Session::new("s1");
        session.messages.push(Message::user_text("go"));
        session.messages.push(Message::Assistant {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "echo".into(),
                arguments: json!({}),
            }],
            reasoning: None,
        });
        // No Tool result — interrupted.

        let err = agent
            .run(&mut session, "continue", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        // Must halt with Reconciliation (because unknown was just inserted).
        assert!(matches!(err, Error::Reconciliation(_)));
        // The tool must NOT have been replayed.
        assert_eq!(
            model.call_count(),
            0,
            "model must not be called when history has unresolved unknowns"
        );
        // A ToolUnknown marker should be in the history.
        assert!(
            session
                .messages
                .iter()
                .any(|m| matches!(m, Message::ToolUnknown { call_id, .. } if call_id == "call-1")),
            "history should contain a ToolUnknown marker"
        );
    }

    // ---- Run wrapper reconciles after error -------------------------------

    #[tokio::test]
    async fn run_reconciles_after_model_error() {
        // Model returns an auth error on the first call. The run fails, but
        // the post-error reconciliation should still run (no unmatched calls
        // here, so no unknowns). The key assertion is that the wrapper
        // doesn't panic and the history is clean.
        let model = Arc::new(ScriptedModelWithCalls::new(vec![Err(Error::ModelTyped(
            ModelError::auth("bad key"),
        ))]));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        let _err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        // No unresolved unknowns because no tool calls were made.
        assert!(!session.has_unresolved_unknowns());
    }

    #[tokio::test]
    async fn run_reconciles_after_tool_timeout() {
        // Model returns a tool call, tool times out → ToolUnknown. The run
        // returns DeadlineExceeded. Post-error reconciliation runs and the
        // ToolUnknown is already there (idempotent — no duplicates).
        struct SlowTool;
        #[async_trait]
        impl Tool for SlowTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "slow".into(),
                    description: "slow".into(),
                    parameters: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                unreachable!()
            }
        }
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "slow".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
        )]))));
        let mut tools = ToolRegistry::new();
        tools.register(SlowTool).unwrap();
        let agent = make_agent(model, tools).with_config(LoopConfig {
            tool_deadline: Some(Duration::from_millis(50)),
            ..LoopConfig::default()
        });
        let mut session = Session::new("s1");
        let _err = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap_err();
        // Exactly one ToolUnknown for call-1 (no duplicates from
        // post-error reconciliation).
        let unknown_count = session
            .messages
            .iter()
            .filter(|m| matches!(m, Message::ToolUnknown { call_id, .. } if call_id == "call-1"))
            .count();
        assert_eq!(unknown_count, 1, "exactly one ToolUnknown, no duplicates");
    }

    // ---- Streaming --------------------------------------------------------

    #[tokio::test]
    async fn streaming_default_fallback_works() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse::text("streamed fallback"),
        )]))));
        let agent = make_agent(model, basic_tools()).with_config(LoopConfig {
            prefer_streaming: true,
            ..LoopConfig::default()
        });
        let mut session = Session::new("s1");
        let result = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap();
        assert_eq!(result, "streamed fallback");
    }

    #[tokio::test]
    async fn streaming_model_assembles_deltas() {
        struct TwoTurnStreamModel {
            turn: AtomicU32,
        }
        #[async_trait]
        impl Model for TwoTurnStreamModel {
            async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
                Err(Error::Model("no".into()))
            }
            async fn stream(
                &self,
                _request: ModelRequest,
                sink: &mut (dyn DeltaSink + Send),
            ) -> Result<ModelResponse> {
                let t = self.turn.fetch_add(1, Ordering::SeqCst);
                let mut asm = StreamAssembler::new();
                if t == 0 {
                    let deltas = vec![
                        StreamDelta::Text {
                            text: "call".into(),
                        },
                        StreamDelta::ToolCall {
                            index: 0,
                            id: Some("call-1".into()),
                            name: Some("echo".into()),
                            arguments: "{\"val".into(),
                        },
                        StreamDelta::ToolCall {
                            index: 0,
                            id: None,
                            name: None,
                            arguments: "ue\": 42}".into(),
                        },
                        StreamDelta::Usage {
                            usage: Usage::new(5, 3),
                        },
                    ];
                    for d in deltas {
                        sink.on_delta(d.clone());
                        asm.push(d);
                    }
                } else {
                    let deltas = vec![StreamDelta::Text {
                        text: "done".into(),
                    }];
                    for d in deltas {
                        sink.on_delta(d.clone());
                        asm.push(d);
                    }
                }
                Ok(asm.finish())
            }
            fn can_stream(&self) -> bool {
                true
            }
        }
        let model = Arc::new(TwoTurnStreamModel {
            turn: AtomicU32::new(0),
        });
        let agent = make_agent(model, basic_tools()).with_config(LoopConfig {
            prefer_streaming: true,
            ..LoopConfig::default()
        });
        let mut session = Session::new("s1");
        let result = agent
            .run(&mut session, "go", &NoopEventSink, &StopToken::new())
            .await
            .unwrap();
        assert_eq!(result, "done");
        assert!(matches!(
            session.messages.iter().find(|m| matches!(m, Message::Tool { call_id, .. } if call_id == "call-1")),
            Some(Message::Tool { content, .. }) if content.contains("42")
        ));
    }

    // ---- Terminal event: exactly one for errors --------------------------

    #[tokio::test]
    async fn exactly_one_terminal_run_failed_event() {
        let model = Arc::new(ScriptedModelWithCalls::new(vec![Err(Error::ModelTyped(
            ModelError::auth("bad key"),
        ))]));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink = move |event: Event| {
            events_clone.lock().unwrap().push(event);
        };
        let _ = agent
            .run(&mut session, "go", &sink, &StopToken::new())
            .await;
        let events = events.lock().unwrap();
        let run_failed_count = events
            .iter()
            .filter(|e| matches!(e, Event::RunFailed { .. }))
            .count();
        let run_completed_count = events
            .iter()
            .filter(|e| matches!(e, Event::RunCompleted { .. }))
            .count();
        assert_eq!(run_failed_count, 1, "exactly one RunFailed event");
        assert_eq!(run_completed_count, 0, "no RunCompleted on failure");
    }

    // ---- Stop token pre-stop ---------------------------------------------

    #[tokio::test]
    async fn stop_before_run_returns_stopped() {
        let model = Arc::new(ScriptedModel(Mutex::new(VecDeque::from([Ok(
            ModelResponse::text("x"),
        )]))));
        let agent = make_agent(model, basic_tools());
        let mut session = Session::new("s1");
        let stop = StopToken::new();
        stop.stop();
        let err = agent
            .run(&mut session, "go", &NoopEventSink, &stop)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Stopped));
    }
}
