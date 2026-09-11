/// Typed front-end for the operator CLI. The execution layer still receives
/// the original argv so this remains compatible with existing integrations.
#[derive(Debug, Parser)]
#[command(name = "servoloop", version, about = "ServoLoop robot operator CLI")]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Debug, Subcommand)]
enum Command {
    Run(RunArgs),
    Resume(ResumeArgs),
    Models(ModelsArgs),
    Providers,
    Config(ConfigArgs),
    Sessions(SessionsArgs),
}
#[derive(Debug, Args)]
struct CommonArgs {
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long, value_name = "FORMAT", value_parser = ["ndjson", "json"])]
    output: Option<String>,
}
#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long)]
    demo: bool,
    #[arg(long)]
    driver: Option<String>,
    #[arg(long, allow_hyphen_values = true)]
    prompt: Option<String>,
    #[arg(long)]
    session: Option<String>,
    #[arg(long, default_value_t = 50)]
    max_turns: usize,
    #[arg(long, value_name = "SECONDS")]
    model_timeout: Option<u64>,
    #[arg(long, value_name = "SECONDS")]
    tool_timeout: Option<u64>,
}
#[derive(Debug, Args)]
struct ResumeArgs {
    #[command(flatten)]
    common: CommonArgs,
    session: String,
    #[arg(long)]
    demo: bool,
    #[arg(long, allow_hyphen_values = true)]
    prompt: Option<String>,
    #[arg(long)]
    driver: Option<String>,
    #[arg(long, default_value_t = 50)]
    max_turns: usize,
    #[arg(long, value_name = "SECONDS")]
    model_timeout: Option<u64>,
    #[arg(long, value_name = "SECONDS")]
    tool_timeout: Option<u64>,
}
#[derive(Debug, Args)]
struct ModelsArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long)]
    offline: bool,
}
#[derive(Debug, Args)]
struct ConfigArgs {
    #[arg(default_value = "show")]
    action: String,
    #[arg(long)]
    config: Option<PathBuf>,
}
#[derive(Debug, Args)]
struct SessionsArgs {
    action: Option<String>,
    id: Option<String>,
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    /// Include the persisted terminal snapshot instead of only journal records.
    #[arg(long)]
    snapshot: bool,
}

pub(crate) fn value(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}
pub(crate) fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
pub(crate) fn machine_output(args: &[String]) -> bool {
    has(args, "--json") || value(args, "--output").as_deref() == Some("json")
}
pub(crate) fn config_action(args: &[String]) -> &str {
    args.iter()
        .skip(1)
        .find(|arg| matches!(arg.as_str(), "init" | "validate" | "show"))
        .map(String::as_str)
        .unwrap_or("show")
}
pub(crate) fn validate_args(args: &[String]) -> Result<(), String> {
    let command = args.first().map(String::as_str).unwrap_or("");
    let flags: &[&str] = match command {
        "run" => &[
            "--demo",
            "--store",
            "--session",
            "--driver",
            "--provider",
            "--model",
            "--base-url",
            "--config",
            "--prompt",
            "--max-turns",
            "--model-timeout",
            "--tool-timeout",
            "--json",
            "--output",
        ],
        "resume" => &[
            "--store",
            "--provider",
            "--model",
            "--base-url",
            "--config",
            "--prompt",
            "--driver",
            "--max-turns",
            "--model-timeout",
            "--tool-timeout",
            "--demo",
            "--json",
            "--output",
        ],
        "models" => &[
            "--provider",
            "--model",
            "--base-url",
            "--offline",
            "--config",
            "--json",
            "--output",
        ],
        "config" => &["--config"],
        "sessions" => &["--store", "--config", "--snapshot"],
        "providers" => &[],
        _ => return Err(format!("unknown command `{command}`")),
    };
    let value_flags = [
        "--store",
        "--session",
        "--driver",
        "--provider",
        "--model",
        "--base-url",
        "--config",
        "--prompt",
        "--max-turns",
        "--model-timeout",
        "--tool-timeout",
        "--output",
    ];
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with('-') && !flags.contains(&a.as_str()) {
            return Err(format!("unknown option `{a}`"));
        }
        if value_flags.contains(&a.as_str()) {
            let stdin_prompt = a == "--prompt" && args.get(i + 1).map(String::as_str) == Some("-");
            if args.get(i + 1).is_none() || (args[i + 1].starts_with('-') && !stdin_prompt) {
                return Err(format!("{a} requires a value"));
            }
            i += 1;
        }
        i += 1;
    }
    Ok(())
}

pub(crate) fn prompt_from_args(args: &[String]) -> Result<String, String> {
    const MAX_STDIN_PROMPT_BYTES: u64 = 1024 * 1024;
    let prompt = value(args, "--prompt").ok_or("--prompt is required for live runs")?;
    if prompt != "-" {
        return Ok(prompt);
    }
    if io::stdin().is_terminal() {
        return Err("--prompt - reads stdin; refusing interactive terminal input".into());
    }
    let mut text = String::new();
    io::stdin()
        .take(MAX_STDIN_PROMPT_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("stdin: {e}"))?;
    if text.len() as u64 > MAX_STDIN_PROMPT_BYTES {
        return Err(format!(
            "stdin prompt exceeds {MAX_STDIN_PROMPT_BYTES} bytes"
        ));
    }
    if text.trim().is_empty() {
        return Err("stdin prompt is empty".into());
    }
    Ok(text)
}

use clap::{Args, Parser, Subcommand};
use std::{
    io::{self, IsTerminal, Read},
    path::PathBuf,
};
