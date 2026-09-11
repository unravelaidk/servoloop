//! OpenAI-compatible Chat Completions adapter implementing
//! [`servoloop_core::Model`].
//!
//! Supports:
//! - Non-streaming (`complete`) and streaming (`stream`) chat completions.
//! - Tool calls with stable IDs and argument assembly across fragments.
//! - Multimodal image payloads (data URLs and remote URLs).
//! - Reasoning trace preservation.
//! - Usage reporting (no fabricated counts).
//! - Structured permanent/transient error mapping with credential
//!   redaction.
//! - SSE fragmentation handling with proper UTF-8 boundary safety.
//! - `[DONE]` sentinel and final finish reasons.
//!
//! Truncated tool calls (non-dispatchable finish reason) are never
//! executed — the assembler returns the finish reason and the loop
//! rejects it.

use crate::error::{redact_url, ProviderError, ProviderResult};
use crate::messages::{to_openai_messages, to_openai_tools};
use crate::spec::ProviderSpec;
use crate::transport::{
    build_client, extract_retry_after, is_done, join_url, map_http_error_with_secret,
    process_sse_chunk, read_bounded_json, read_bounded_text,
};
use crate::Discovery;
use servoloop_core::{
    DeltaSink, FinishReason, Model, ModelRequest, ModelResponse, Reasoning, StreamDelta, Usage,
};
use std::sync::Arc;
use std::time::Duration;

/// Default per-request timeout if the request doesn't carry a deadline.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// An OpenAI-compatible Chat Completions provider.
///
/// Implements the core [`Model`] trait. Construct with an explicit
/// [`ProviderSpec`] and model ID. The provider owns its HTTP client and
/// optional discovery cache.
pub struct OpenAiCompatProvider {
    spec: ProviderSpec,
    model_id: String,
    client: reqwest::Client,
    discovery: Option<Arc<Discovery>>,
}

impl std::fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatProvider")
            .field("provider", &self.spec.id)
            .field("model", &self.model_id)
            .field("endpoint", &redact_url(&self.spec.resolve_endpoint()))
            .field("has_discovery", &self.discovery.is_some())
            .finish()
    }
}

impl OpenAiCompatProvider {
    /// Create a new provider with the given spec and model ID.
    ///
    /// Validates that the protocol is supported and (if required) a key
    /// is available.
    pub fn new(spec: ProviderSpec, model_id: impl Into<String>) -> ProviderResult<Self> {
        spec.validate()?;
        let client = build_client()?;
        Ok(Self {
            spec,
            model_id: model_id.into(),
            client,
            discovery: None,
        })
    }

    /// Attach a discovery instance for model catalog operations.
    pub fn with_discovery(mut self, discovery: Arc<Discovery>) -> Self {
        self.discovery = Some(discovery);
        self
    }

    /// The provider spec (for endpoint/key access).
    pub fn spec(&self) -> &ProviderSpec {
        &self.spec
    }

    /// The model ID this provider is configured to use.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Build the chat completions endpoint URL.
    fn completions_url(&self) -> String {
        join_url(&self.spec.resolve_endpoint(), "chat/completions")
    }

    /// Build the request payload.
    fn build_payload(
        &self,
        request: &ModelRequest,
        stream: bool,
    ) -> ProviderResult<serde_json::Value> {
        let messages = to_openai_messages(&request.messages)?;
        let tools = to_openai_tools(&request.tools);

        let mut payload = serde_json::json!({
            "model": self.model_id,
            "messages": messages,
            "stream": stream,
        });

        if !tools.is_empty() {
            payload["tools"] = serde_json::Value::Array(tools);
            payload["tool_choice"] = serde_json::json!("auto");
        }

        if stream {
            payload["stream_options"] = serde_json::json!({"include_usage": true});
        }

        if let Some(max_output) = request.max_output {
            payload["max_tokens"] = serde_json::json!(max_output);
        }

        if let Some(ref sampling) = request.sampling {
            if let Some(temp) = sampling.temperature {
                payload["temperature"] = serde_json::json!(temp);
            }
            if let Some(top_p) = sampling.top_p {
                payload["top_p"] = serde_json::json!(top_p);
            }
            if let Some(max_tokens) = sampling.max_tokens {
                payload["max_tokens"] = serde_json::json!(max_tokens);
            }
        }

        Ok(payload)
    }

    /// Build the authenticated request builder.
    fn build_request(&self, payload: &serde_json::Value) -> reqwest::RequestBuilder {
        let url = self.completions_url();
        let mut builder = self
            .client
            .post(&url)
            .json(payload)
            .timeout(REQUEST_TIMEOUT);

        if let Some(key) = self.spec.resolve_key() {
            builder = builder.bearer_auth(key.as_str());
        }

        // OpenRouter requires these headers for rankings and rate limits.
        if self.spec.id == "openrouter" {
            if let Ok(val) =
                reqwest::header::HeaderValue::from_str("https://github.com/unravelaidk/servoloop")
            {
                builder = builder.header("HTTP-Referer", val);
            }
            let val = reqwest::header::HeaderValue::from_static("ServoLoop");
            builder = builder.header("X-Title", val);
        }

        builder
    }

    /// Parse a non-streaming completion response body.
    fn parse_completion(body: &serde_json::Value) -> ProviderResult<ModelResponse> {
        let choice = body
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| ProviderError::malformed("response did not include any choices"))?;

        let message = choice
            .get("message")
            .ok_or_else(|| ProviderError::malformed("choice did not include a message"))?;

        let content = message
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // Extract reasoning from common field names.
        let reasoning = extract_reasoning_text(message).map(|text| Reasoning { text: Some(text) });

        // Parse tool calls — reject invalid JSON and non-object arguments.
        let tool_calls: Vec<servoloop_core::ToolCall> = match message.get("tool_calls") {
            None => Vec::new(),
            Some(calls) => {
                let calls = calls.as_array().ok_or_else(|| {
                    ProviderError::malformed("message tool_calls must be an array")
                })?;
                let mut result = Vec::with_capacity(calls.len());
                for call in calls {
                    match parse_tool_call(call) {
                        Some(ParsedToolCall::Ok(tc)) => result.push(tc),
                        Some(ParsedToolCall::InvalidArgumentsJson { id, name, raw }) => {
                            return Err(ProviderError::invalid(format!(
                                "tool call `{id}` (`{name}`) has arguments that are not valid JSON: {raw}"
                            )));
                        }
                        Some(ParsedToolCall::NonObjectArguments { id, name, parsed }) => {
                            return Err(ProviderError::invalid(format!(
                                "tool call `{id}` (`{name}`) arguments must be a JSON object, got `{}`",
                                json_type(&parsed)
                            )));
                        }
                        Some(ParsedToolCall::Malformed) => {
                            return Err(ProviderError::malformed(
                                "tool call is missing a string id, function, or function name",
                            ));
                        }
                        None => {
                            return Err(ProviderError::malformed(
                                "tool call is missing a string id, function, or function name",
                            ));
                        }
                    }
                }
                result
            }
        };

        let usage = parse_usage(body);

        let finish_reason = choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .map(parse_finish_reason)
            .ok_or_else(|| {
                ProviderError::malformed("completion did not include a finish reason")
            })?;

        Ok(ModelResponse {
            content,
            tool_calls,
            usage,
            finish_reason,
            reasoning,
        })
    }
}

/// Extract reasoning text from common OpenAI-compatible field names.
fn extract_reasoning_text(message: &serde_json::Value) -> Option<String> {
    ["reasoning_content", "reasoning", "reasoning_text"]
        .iter()
        .find_map(|field| {
            message
                .get(*field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        })
}

/// Extract reasoning delta from a streaming delta object.
fn extract_reasoning_delta(delta: &serde_json::Value) -> Option<String> {
    ["reasoning_content", "reasoning", "reasoning_text"]
        .iter()
        .find_map(|field| {
            delta
                .get(*field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        })
}

/// Parsed tool call result: either a valid call or an error explaining
/// why the call was rejected.
enum ParsedToolCall {
    Ok(servoloop_core::ToolCall),
    /// Required wire shape is missing or has the wrong type.
    Malformed,
    /// Arguments are not valid JSON.
    InvalidArgumentsJson {
        id: String,
        name: String,
        raw: String,
    },
    /// Arguments are valid JSON but not an object (e.g. a scalar,
    /// array, or null).
    NonObjectArguments {
        id: String,
        name: String,
        parsed: serde_json::Value,
    },
}

/// Parse a tool call from a non-streaming response.
///
/// Returns [`ParsedToolCall`]. The caller is responsible for converting
/// `InvalidArgumentsJson` or `NonObjectArguments` into a
/// [`ProviderError`] — invalid tool calls are **never** silently
/// repaired, defaulted, or string-fallbacked.
fn parse_tool_call(call: &serde_json::Value) -> Option<ParsedToolCall> {
    let id = match call.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return Some(ParsedToolCall::Malformed),
    };
    let function = match call.get("function") {
        Some(function) => function,
        None => return Some(ParsedToolCall::Malformed),
    };
    let name = match function.get("name").and_then(|v| v.as_str()) {
        Some(name) => name.to_string(),
        None => return Some(ParsedToolCall::Malformed),
    };
    let arguments_value = match function.get("arguments") {
        Some(arguments) => arguments,
        None => {
            return Some(ParsedToolCall::InvalidArgumentsJson {
                id,
                name,
                raw: "<missing>".to_string(),
            });
        }
    };
    let arguments_str = match arguments_value.as_str() {
        Some(arguments) => arguments,
        None => {
            return Some(ParsedToolCall::InvalidArgumentsJson {
                id,
                name,
                raw: arguments_value.to_string(),
            });
        }
    };

    if arguments_str.is_empty() {
        return Some(ParsedToolCall::NonObjectArguments {
            id,
            name,
            parsed: serde_json::Value::Null,
        });
    }

    match serde_json::from_str::<serde_json::Value>(arguments_str) {
        Ok(parsed) if parsed.is_object() => Some(ParsedToolCall::Ok(servoloop_core::ToolCall {
            id,
            name,
            arguments: parsed,
        })),
        Ok(parsed) => Some(ParsedToolCall::NonObjectArguments { id, name, parsed }),
        Err(_) => Some(ParsedToolCall::InvalidArgumentsJson {
            id,
            name,
            raw: arguments_str.to_string(),
        }),
    }
}

/// Parse usage from a response body.
fn parse_usage(body: &serde_json::Value) -> Usage {
    let usage = body.get("usage");
    if usage.is_none() {
        return Usage::unknown();
    }
    let usage = usage.unwrap();

    let prompt = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let completion = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    if prompt.is_none() && completion.is_none() && total.is_none() {
        return Usage::unknown();
    }

    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
    }
}

/// Parse a finish reason string into the canonical enum.
fn parse_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCall,
        "length" => FinishReason::MaxOutput,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

/// Describe the JSON type of a value for error messages.
fn json_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Validate tool call arguments in a completed `ModelResponse`.
///
/// Tool call arguments **must** be a JSON object. The OpenAI API
/// specification requires `arguments` to be a JSON-encoded object
/// string. The core `StreamAssembler` is intentionally lenient — it
/// falls back to a raw string for unparseable fragments. This function
/// is the provider's boundary check: it rejects any tool call whose
/// arguments are not a JSON object before the response reaches the
/// agent loop.
///
/// This catches:
/// - Invalid JSON arguments (assembler fallback to `Value::String`).
/// - Valid JSON that is a scalar (number, string, bool, null).
/// - Valid JSON that is an array.
/// - Empty arguments (assembler fallback to `Value::Null`).
fn validate_tool_call_arguments(response: &ModelResponse) -> ProviderResult<()> {
    for call in &response.tool_calls {
        if call.id.is_empty() || call.name.is_empty() {
            return Err(ProviderError::invalid(
                "tool call is missing a non-empty id or function name",
            ));
        }
        if !call.arguments.is_object() {
            return Err(ProviderError::invalid(format!(
                "tool call `{}` (`{}`) arguments must be a JSON object, got `{}`",
                call.id,
                call.name,
                json_type(&call.arguments)
            )));
        }
    }
    Ok(())
}

/// Parse usage from a choice-level fallback (some providers put usage
/// inside choices[0]).
fn choice_usage(payload: &serde_json::Value) -> Option<serde_json::Value> {
    payload
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|c| c.first())
        .and_then(|choice| choice.get("usage"))
        .cloned()
}

#[async_trait::async_trait]
impl Model for OpenAiCompatProvider {
    async fn complete(&self, request: ModelRequest) -> servoloop_core::Result<ModelResponse> {
        let payload = self.build_payload(&request, false)?;
        let response = self
            .build_request(&payload)
            .send()
            .await
            .map_err(|e| ProviderError::transient(format!("request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            // Extract Retry-After before consuming the body.
            let retry_after = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                extract_retry_after(&response)
            } else {
                None
            };
            let body = read_bounded_text(response).await;
            let mut err = map_http_error_with_secret(
                status,
                &body,
                self.spec.resolve_key().as_ref().map(|key| key.as_str()),
            );
            if let Some(delay) = retry_after {
                if let ProviderError::Transient {
                    redacted_detail, ..
                } = &err
                {
                    err = ProviderError::transient_with_retry(redacted_detail.clone(), delay);
                }
            }
            return Err(err.into());
        }

        let body = read_bounded_json(response).await?;
        Self::parse_completion(&body).map_err(Into::into)
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &mut (dyn DeltaSink + Send),
    ) -> servoloop_core::Result<ModelResponse> {
        let payload = self.build_payload(&request, true)?;
        let response = self
            .build_request(&payload)
            .send()
            .await
            .map_err(|e| ProviderError::transient(format!("streaming request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                extract_retry_after(&response)
            } else {
                None
            };
            let body = read_bounded_text(response).await;
            let mut err = map_http_error_with_secret(
                status,
                &body,
                self.spec.resolve_key().as_ref().map(|key| key.as_str()),
            );
            if let Some(delay) = retry_after {
                if let ProviderError::Transient {
                    redacted_detail, ..
                } = &err
                {
                    err = ProviderError::transient_with_retry(redacted_detail.clone(), delay);
                }
            }
            return Err(err.into());
        }

        // SSE stream processing.
        let mut buffer = Vec::new();
        let mut total_bytes = 0usize;
        let mut assembler = servoloop_core::StreamAssembler::new();
        let mut finish_reason: Option<FinishReason> = None;

        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();

        let mut saw_done = false;
        'chunks: while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| ProviderError::transient(format!("stream chunk error: {e}")))?;

            let (events, _exceeded) = process_sse_chunk(&mut buffer, &chunk, &mut total_bytes)?;

            for event in events {
                if is_done(&event.data) {
                    if finish_reason.is_none() {
                        return Err(ProviderError::malformed(
                            "SSE stream ended with [DONE] but no finish reason",
                        )
                        .into());
                    }
                    saw_done = true;
                    break 'chunks;
                }

                let payload: serde_json::Value =
                    serde_json::from_str(&event.data).map_err(|e| {
                        ProviderError::malformed(format!("failed to parse SSE JSON: {e}"))
                    })?;
                if payload.get("error").is_some() {
                    return Err(
                        ProviderError::invalid("SSE stream returned an error payload").into(),
                    );
                }

                // Check for usage at the top level or in choices[0].
                if let Some(raw_usage) = payload
                    .get("usage")
                    .cloned()
                    .or_else(|| choice_usage(&payload))
                {
                    let usage = parse_usage(&serde_json::json!({"usage": raw_usage}));
                    let d = StreamDelta::Usage { usage };
                    assembler.push(d.clone());
                    sink.on_delta(d);
                }

                // Process choices.
                if let Some(choice) = payload
                    .get("choices")
                    .and_then(|v| v.as_array())
                    .and_then(|c| c.first())
                {
                    if finish_reason.is_some()
                        && (choice
                            .get("delta")
                            .and_then(|d| d.get("content"))
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty())
                            || choice
                                .get("delta")
                                .and_then(|d| d.get("reasoning_content"))
                                .and_then(|v| v.as_str())
                                .is_some_and(|s| !s.is_empty())
                            || choice
                                .get("delta")
                                .and_then(|d| d.get("tool_calls"))
                                .is_some())
                    {
                        return Err(ProviderError::malformed(
                            "SSE stream mutated completion after finish reason",
                        )
                        .into());
                    }
                    let delta = choice.get("delta").cloned().unwrap_or_default();

                    // Text content.
                    if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                        if !content.is_empty() {
                            let d = StreamDelta::Text {
                                text: content.to_string(),
                            };
                            assembler.push(d.clone());
                            sink.on_delta(d);
                        }
                    }

                    // Reasoning content.
                    if let Some(reasoning) = extract_reasoning_delta(&delta) {
                        let d = StreamDelta::Reasoning { text: reasoning };
                        assembler.push(d.clone());
                        sink.on_delta(d);
                    }

                    // Tool calls.
                    if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for call in calls {
                            let index = call
                                .get("index")
                                .and_then(|v| v.as_u64())
                                .map(|v| v as u32)
                                .unwrap_or(0);

                            let id = call
                                .get("id")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            let name = call
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            let arguments = call
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");

                            let d = StreamDelta::ToolCall {
                                index,
                                id,
                                name,
                                arguments: arguments.to_string(),
                            };
                            assembler.push(d.clone());
                            sink.on_delta(d);
                        }
                    }

                    // Check for finish_reason.
                    if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                        finish_reason = Some(parse_finish_reason(reason));
                    }
                }
            }
        }

        if !buffer.is_empty() {
            return Err(
                ProviderError::malformed("SSE stream ended with an incomplete event").into(),
            );
        }
        if finish_reason.is_none() {
            return Err(ProviderError::malformed(if saw_done {
                "SSE stream ended with [DONE] but no finish reason"
            } else {
                "SSE stream ended without a finish reason"
            })
            .into());
        }

        // Finalize the assembled response.
        let mut response = assembler.finish();

        // Override the finish reason if the stream reported one.
        if let Some(reason) = finish_reason {
            response.finish_reason = reason;
        }

        // Validate tool call arguments: reject invalid JSON and
        // non-object arguments. The StreamAssembler in core produces
        // a raw string fallback for unparseable args; we catch that
        // here and reject the response before it reaches the loop.
        // This is the provider's responsibility — the core assembler
        // is intentionally lenient (it doesn't know the wire format).
        validate_tool_call_arguments(&response)?;

        Ok(response)
    }

    fn can_stream(&self) -> bool {
        true
    }
}

// The handle_error_response helper was inlined into the complete and
// stream methods above so that Retry-After headers are checked before
// the body is consumed.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_finish_reason_mappings() {
        assert_eq!(parse_finish_reason("stop"), FinishReason::Stop);
        assert_eq!(parse_finish_reason("tool_calls"), FinishReason::ToolCall);
        assert_eq!(parse_finish_reason("function_call"), FinishReason::ToolCall);
        assert_eq!(parse_finish_reason("length"), FinishReason::MaxOutput);
        assert_eq!(
            parse_finish_reason("content_filter"),
            FinishReason::ContentFilter
        );
        assert_eq!(
            parse_finish_reason("weird"),
            FinishReason::Other("weird".to_string())
        );
    }

    #[test]
    fn parse_usage_from_body() {
        let body = serde_json::json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
            }
        });
        let usage = parse_usage(&body);
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(5));
        assert_eq!(usage.total_tokens, Some(15));
    }

    #[test]
    fn parse_usage_unknown_when_absent() {
        let body = serde_json::json!({"choices": []});
        let usage = parse_usage(&body);
        assert!(usage.is_unknown());
    }

    #[test]
    fn parse_completion_with_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "echo",
                            "arguments": "{\"x\": 42}",
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
            },
        });
        let response = OpenAiCompatProvider::parse_completion(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "echo");
        assert_eq!(response.tool_calls[0].arguments, json!({"x": 42}));
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
        assert_eq!(response.usage.prompt_tokens, Some(10));
    }

    #[test]
    fn parse_completion_text_only() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "hello world",
                },
                "finish_reason": "stop",
            }],
        });
        let response = OpenAiCompatProvider::parse_completion(&body).unwrap();
        assert_eq!(response.content, "hello world");
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert!(response.usage.is_unknown());
    }

    #[test]
    fn parse_completion_rejects_malformed_tool_shape() {
        let body = json!({
            "choices": [{
                "message": {"tool_calls": [{"function": {"name": "echo", "arguments": "{}"}}]},
                "finish_reason": "tool_calls"
            }]
        });
        assert!(OpenAiCompatProvider::parse_completion(&body).is_err());

        let wrong_type = json!({
            "choices": [{
                "message": {"tool_calls": "not-an-array"},
                "finish_reason": "stop"
            }]
        });
        assert!(OpenAiCompatProvider::parse_completion(&wrong_type).is_err());
    }

    #[test]
    fn parse_completion_with_reasoning() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "answer",
                    "reasoning_content": "step by step",
                },
                "finish_reason": "stop",
            }],
        });
        let response = OpenAiCompatProvider::parse_completion(&body).unwrap();
        assert_eq!(
            response.reasoning.as_ref().and_then(|r| r.text.as_deref()),
            Some("step by step")
        );
    }

    #[test]
    fn parse_completion_missing_choices_is_error() {
        let body = serde_json::json!({"error": "bad"});
        let result = OpenAiCompatProvider::parse_completion(&body);
        assert!(result.is_err());
    }
}
