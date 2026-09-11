//! `servoloop`: a deliberately small machine-oriented operator CLI.

mod args;
mod commands;
mod config;
mod execution;
mod journal_tool;
mod output;
mod simulation;

use args::{config_action, validate_args, Cli};
use clap::{error::ErrorKind, Parser};
use std::{env, process::ExitCode};

fn usage() {
    eprintln!("usage: servoloop <run|resume|providers|models|config|sessions> [options]\n  run --demo [--store DIR] [--session ID]\n  run --prompt TEXT --provider ID --model ID [--store DIR] [--session ID]\n  resume SESSION --prompt TEXT --provider ID --model ID [--store DIR]\n  models --provider ID [--offline --model ID]\n  sessions list|show ID|delete ID");
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    // A lone `-` is a meaningful prompt value (stdin), but clap otherwise
    // treats it as the beginning of another option.
    let mut parse_args = args.clone();
    for i in 0..parse_args.len().saturating_sub(1) {
        if parse_args[i] == "--prompt" && parse_args[i + 1] == "-" {
            parse_args[i] = "--prompt=-".into();
            parse_args.remove(i + 1);
            break;
        }
    }
    let parse = std::iter::once(String::from("servoloop")).chain(parse_args);
    if let Err(error) = Cli::try_parse_from(parse) {
        let code = match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
            _ => 2,
        };
        eprint!(
            "{}",
            error
                .to_string()
                .replace("unexpected argument", "unknown option")
        );
        return ExitCode::from(code);
    }
    if args.is_empty() {
        usage();
        return ExitCode::from(2);
    }
    if let Err(error) = validate_args(&args) {
        eprintln!("error: {error}");
        return ExitCode::from(2);
    }
    let command = &args[0];
    if command == "providers" {
        return commands::providers();
    }
    if command == "config" && config_action(&args) == "init" {
        return config::init(&args);
    }
    let cfg = match config::load(&args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let result = match command.as_str() {
        "run" => execution::run(&args, &cfg).await,
        "resume" => execution::resume(&args, &cfg).await,
        "models" => commands::models(&args, &cfg).await,
        "config" => commands::config(&args, &cfg),
        "sessions" => commands::sessions(&args, &cfg).map(|_| 0),
        _ => {
            usage();
            Err("unknown command".into())
        }
    };
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(error) => {
            eprintln!("error: {}", output::redact(&error));
            ExitCode::from(2)
        }
    }
}
