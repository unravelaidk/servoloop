//! Live model demo for the servoloop-providers OpenAI-compatible adapter.
//!
//! This example demonstrates discovery and invocation of an
//! OpenAI-compatible provider. It requires a live API key and makes
//! real network requests — it is **not** part of CPU-only CI.
//!
//! # Prerequisites
//!
//! Set one of the following environment variables:
//! - `OPENAI_API_KEY` — for OpenAI
//! - `NVIDIA_API_KEY` — for NVIDIA
//! - `OPENROUTER_API_KEY` — for OpenRouter
//! - `OLLAMA_BASE_URL` — for a local Ollama server (no key needed)
//!
//! # Running
//!
//! ```bash
//! # OpenAI
//! OPENAI_API_KEY=sk-... cargo run --bin servoloop-openai-compatible-demo -- --provider openai --model gpt-4o-mini --prompt "Say hello in one sentence."
//!
//! # NVIDIA
//! NVIDIA_API_KEY=... cargo run --bin servoloop-openai-compatible-demo -- --provider nvidia --model meta/llama-3.1-405b-instruct --prompt "Say hello."
//!
//! # OpenRouter
//! OPENROUTER_API_KEY=... cargo run --bin servoloop-openai-compatible-demo -- --provider openrouter --model openai/gpt-4o-mini --prompt "Say hello."
//!
//! # Local Ollama (no key)
//! cargo run --bin servoloop-openai-compatible-demo -- --provider ollama --model llama3:8b --prompt "Say hello."
//!
//! # Discover available models
//! cargo run --bin servoloop-openai-compatible-demo -- --provider openai --discover
//! ```
//!
//! # Discovery ≠ Invocation
//!
//! Discovery describes capability metadata. It does not guarantee that
//! invocation will succeed. A model that reports `tool_call: true` may
//! still fail at runtime due to rate limits, model unavailability, or
//! provider-side changes. Always handle typed errors from invocation.

use servoloop_core::{Message, Model, ModelRequest, Session};
use servoloop_providers::{
    Discovery, DiscoveryOptions, OpenAiCompatProvider, ProviderError, ProviderSpec, Secret,
};
use std::env;

fn parse_args() -> Args {
    let mut provider = "openai".to_string();
    let mut model = String::new();
    let mut prompt = "Say hello in one sentence.".to_string();
    let mut discover = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--provider" => {
                if let Some(v) = args.next() {
                    provider = v;
                }
            }
            "--model" => {
                if let Some(v) = args.next() {
                    model = v;
                }
            }
            "--prompt" => {
                if let Some(v) = args.next() {
                    prompt = v;
                }
            }
            "--discover" => {
                discover = true;
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: servoloop-openai-compatible-demo --provider <id> [--model <id>] \
                     [--prompt <text>] [--discover]"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    Args {
        provider,
        model,
        prompt,
        discover,
    }
}

struct Args {
    provider: String,
    model: String,
    prompt: String,
    discover: bool,
}

fn build_spec(provider: &str) -> Result<ProviderSpec, ProviderError> {
    match provider {
        "openai" => {
            let key =
                env::var("OPENAI_API_KEY").map_err(|_| ProviderError::missing_key("openai"))?;
            Ok(ProviderSpec::openai(key))
        }
        "nvidia" => {
            let key =
                env::var("NVIDIA_API_KEY").map_err(|_| ProviderError::missing_key("nvidia"))?;
            Ok(ProviderSpec::nvidia(key))
        }
        "openrouter" => {
            let key = env::var("OPENROUTER_API_KEY")
                .map_err(|_| ProviderError::missing_key("openrouter"))?;
            Ok(ProviderSpec::openrouter(key))
        }
        "ollama" => Ok(ProviderSpec::ollama()),
        _ => {
            // Custom provider: expect SERVOLOOP_BASE_URL and optionally
            // SERVOLOOP_API_KEY.
            let base_url = env::var("SERVOLOOP_BASE_URL").map_err(|_| {
                ProviderError::invalid(format!(
                    "custom provider `{provider}` requires SERVOLOOP_BASE_URL"
                ))
            })?;
            let key = env::var("SERVOLOOP_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
                .map(Secret::new);
            Ok(ProviderSpec::custom(
                "custom",
                "Custom Provider",
                base_url,
                key,
            ))
        }
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let spec = match build_spec(&args.provider) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(1);
        }
    };

    // Validate the spec.
    if let Err(e) = spec.validate() {
        eprintln!("Provider validation failed: {e}");
        std::process::exit(1);
    }

    if args.discover {
        println!("Discovering models for provider `{}`…", args.provider);
        println!("(Discovery describes capabilities; it does not guarantee invocation success.)\n");

        let discovery = Discovery::new();
        let options = DiscoveryOptions::default();
        match discovery.discover(&spec, &options).await {
            Ok(models) => {
                if models.is_empty() {
                    println!("No models discovered.");
                } else {
                    println!(
                        "{:<40} {:<30} {:<10} {:<12} {:<10}",
                        "ID", "Name", "Tools", "Deprecated", "Endpoint"
                    );
                    println!("{}", "-".repeat(102));
                    for model in &models {
                        println!(
                            "{:<40} {:<30} {:<10} {:<12} {:<10}",
                            model.model_id,
                            model.name,
                            format!("{:?}", model.tool_support),
                            model.deprecated,
                            model.from_endpoint,
                        );
                    }
                    println!("\n{} model(s) discovered.", models.len());
                }
            }
            Err(e) => {
                eprintln!("Discovery failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.model.is_empty() {
        eprintln!("Error: --model <id> is required for invocation (or use --discover).");
        std::process::exit(1);
    }

    let provider = match OpenAiCompatProvider::new(spec.clone(), &args.model) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to create provider: {e}");
            std::process::exit(1);
        }
    };

    println!("Provider: {} ({})", spec.display_name, spec.id);
    println!("Model: {}", args.model);
    println!("Endpoint: {}", spec.resolve_endpoint());
    println!("Prompt: {}\n", args.prompt);
    println!("---");

    let mut session = Session::new("demo");
    session.messages.push(Message::user_text(&args.prompt));

    let request = ModelRequest::new("demo", session.messages);

    match provider.complete(request).await {
        Ok(response) => {
            println!("Response: {}", response.content);
            if !response.usage.is_unknown() {
                println!(
                    "\nUsage: prompt={:?}, completion={:?}, total={:?}",
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    response.usage.total_tokens
                );
            }
            println!("Finish reason: {:?}", response.finish_reason);
        }
        Err(e) => {
            eprintln!("Invocation failed: {e}");
            eprintln!("(Discovery describes capabilities; invocation may still fail.)");
            std::process::exit(1);
        }
    }
}
