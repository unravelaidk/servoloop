# servoloop-providers

OpenAI-compatible model providers and capability-aware model discovery
for ServoLoop.

## Overview

This crate delivers the first real provider vertical slice:

- **OpenAI-compatible Chat Completions** adapter with tools, streaming,
  usage, and multimodal image payloads.
- **Capability-aware model discovery** from Models.dev, generic `/models`
  endpoints, and local Ollama.
- **Instance-owned TTL caches** keyed by endpoint + account identity —
  no process-global mutable state, no secrets in cache keys.
- **Credential redaction** in all Debug/log/error output.
- **Structured error mapping** (permanent vs. transient) with
  retryability classification.

## Quick start

```rust
use servoloop_core::{Message, ModelRequest, Session};
use servoloop_providers::{OpenAiCompatProvider, ProviderSpec};

# async fn run() -> servoloop_core::Result<()> {
let spec = ProviderSpec::openai("sk-...".to_string());
let provider = OpenAiCompatProvider::new(spec, "gpt-4o-mini")?;

let mut session = Session::new("demo");
session.messages.push(Message::user_text("Hello!"));

let request = ModelRequest::new("demo", session.messages);
let response = provider.complete(request).await?;
println!("{}", response.content);
# Ok(())
# }
```

## Provider specification

Built-in specs provide defaults for common providers:

| Provider | Spec constructor | Key env | Default endpoint |
|---|---|---|---|
| OpenAI | `ProviderSpec::openai(key)` | `OPENAI_API_KEY` | `https://api.openai.com/v1` |
| NVIDIA | `ProviderSpec::nvidia(key)` | `NVIDIA_API_KEY` | `https://integrate.api.nvidia.com/v1` |
| OpenRouter | `ProviderSpec::openrouter(key)` | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1` |
| Ollama (local) | `ProviderSpec::ollama()` | `OLLAMA_API_KEY` (optional) | `http://localhost:11434/v1` |

Custom OpenAI-compatible endpoints:

```rust
use servoloop_providers::{ProviderSpec, OpenAiCompatProvider};

let spec = ProviderSpec::custom(
    "my-provider",
    "My Provider",
    "https://api.my-provider.com/v1".to_string(),
    None, // or Some(Secret::new("key"))
);
let provider = OpenAiCompatProvider::new(spec, "my-model")?;
```

### Endpoint precedence

1. Explicit override (`spec.with_base_url(...)`)
2. Environment variable (e.g. `OPENAI_BASE_URL`)
3. Built-in default

### API key precedence

1. Explicit key (`spec.with_key(...)`)
2. Environment variable (e.g. `OPENAI_API_KEY`)
3. None (valid for keyless providers like local Ollama)

## Discovery

Discovery describes capability metadata. It does **not** guarantee
invocation success. A model that reports `tool_call: true` may still
fail at runtime.

```rust
use servoloop_providers::{Discovery, DiscoveryOptions, ProviderSpec};

# async fn run() -> servoloop_providers::ProviderResult<()> {
let spec = ProviderSpec::openai("sk-...".to_string());
let discovery = Discovery::new();
let options = DiscoveryOptions::default();
let models = discovery.discover(&spec, &options).await?;

for model in &models {
    println!("{}: tools={:?}, deprecated={}, from_endpoint={}",
        model.model_id, model.tool_support, model.deprecated, model.from_endpoint);
}
# Ok(())
# }
```

### Tool workflow filtering

Use `DiscoveryOptions::for_tools()` to filter out deprecated and
explicitly non-tool-capable models. Unknown capability is preserved
(not silently upgraded to supported).

### Explicit model overrides

Add explicit model IDs that are included even if not discovered:

```rust
let options = DiscoveryOptions {
    explicit_model_ids: vec!["my-custom-model".to_string()],
    ..DiscoveryOptions::default()
};
```

### Cache behavior

- Instance-owned (no process-global state).
- TTL: 5 minutes by default (`Discovery::with_ttl` to customize).
- Keyed by: endpoint + account identity + protocol + discovery options.
- No secrets in cache keys (uses a hash of the API key).

## Ollama discovery

Ollama exposes a native `/api/tags` endpoint (separate from the
OpenAI-compatible `/v1/models`). The `/v1` base URL is used for the
chat completions runtime, while `/api/tags` (at the root, not under
`/v1`) is used for native discovery.

```rust
use servoloop_providers::discovery::fetch_ollama_tags;

# async fn run() -> servoloop_providers::ProviderResult<()> {
let tags = fetch_ollama_tags("http://localhost:11434/v1").await?;
for tag in &tags {
    println!("Ollama model: {tag}");
}
# Ok(())
# }
```

## Error handling

Errors are classified as permanent or transient:

- **Permanent**: auth failure, invalid request, unsupported protocol,
  deprecated/non-tool model, malformed response. Not retried.
- **Transient**: rate limit (with Retry-After), server error, network
  failure. Retried by the agent loop.

All error messages are redacted — no API keys, bearer tokens, or
secret-bearing URLs leak into error output.

## Credential safety

- Secrets are wrapped in `Secret` with redacted `Debug` and `Display`.
- No plaintext persistence.
- No secrets in cache keys (uses a hash).
- No secrets in error messages (defense-in-depth redaction).
- Explicit API key over env variable, but env is supported as fallback.

## HTTP transport

- `reqwest` with `rustls` (no native TLS dependency).
- No redirects (`redirect::Policy::none()`).
- Bounded response body (4 MiB for JSON, 16 MiB for SSE streams).
- Per-request timeouts.
- SSE fragmentation handling with proper UTF-8 boundary safety.

## Streaming

The adapter implements both `complete` and `stream` on the core `Model`
trait. Streaming handles:

- Fragmented SSE events across chunk boundaries.
- Multi-byte UTF-8 characters split across chunks.
- Interleaved tool-call assembly with stable IDs.
- `[DONE]` sentinel detection.
- Final finish reason reporting.
- Usage reporting (no fabricated counts).
- Truncated tool calls are never executed (non-dispatchable finish
  reasons are rejected by the loop).

## Attribution

See `NOTICE` for AGPL attribution of adapted Khadim code.

## Live smoke test

The `servoloop-openai-compatible-demo` example provides a live smoke
test procedure. It is **not** part of CPU-only CI — it requires real
credentials and makes real network requests.

```bash
# Discover models
OPENAI_API_KEY=sk-... cargo run --bin servoloop-openai-compatible-demo -- --provider openai --discover

# Invoke a model
OPENAI_API_KEY=sk-... cargo run --bin servoloop-openai-compatible-demo -- --provider openai --model gpt-4o-mini --prompt "Say hello."
```

## Limitations

- Only `OpenAiChatCompletions` protocol is implemented. `OpenAiResponses`
  and `AnthropicMessages` are recognized but rejected with a typed
  error (follow-up work).
- No OAuth implementation (explicit API key or env variable only).
- No automatic live API calls in tests (all tests use mock HTTP
  servers).
- Discovery ≠ invocation: capability metadata does not guarantee
  runtime success.