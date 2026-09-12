pub(crate) fn provider(name: &str, base: Option<String>) -> Result<ProviderSpec, String> {
    let p = match name {
        "openai" => ProviderSpec::openai(env::var("OPENAI_API_KEY").unwrap_or_default()),
        "openrouter" => {
            ProviderSpec::openrouter(env::var("OPENROUTER_API_KEY").unwrap_or_default())
        }
        "nvidia" => ProviderSpec::nvidia(env::var("NVIDIA_API_KEY").unwrap_or_default()),
        "ollama" => ProviderSpec::ollama(),
        _ => return Err(format!("unknown provider `{name}`")),
    };
    Ok(base.map_or(p.clone(), |u| p.with_base_url(u)))
}

pub(crate) fn configured_provider(
    name: &str,
    base: Option<String>,
    cfg: &Config,
) -> Result<ProviderSpec, String> {
    if let Some(profile) = cfg.provider_profile.as_ref().filter(|p| p.id == name) {
        profile.spec(base)
    } else {
        provider(name, base)
    }
}

pub(crate) fn setting(
    args: &[String],
    flag: &str,
    environment: &str,
    config: Option<String>,
) -> Option<String> {
    value(args, flag)
        .or_else(|| env::var(environment).ok())
        .or(config)
}

pub(crate) async fn models(args: &[String], cfg: &Config) -> Result<i32, String> {
    let name = setting(
        args,
        "--provider",
        "SERVOLOOP_PROVIDER",
        cfg.provider.clone(),
    )
    .ok_or("--provider is required")?;
    let model = setting(args, "--model", "SERVOLOOP_MODEL", cfg.model.clone());
    let spec = configured_provider(
        &name,
        setting(
            args,
            "--base-url",
            "SERVOLOOP_BASE_URL",
            cfg.base_url.clone(),
        ),
        cfg,
    )?;
    let opts = DiscoveryOptions {
        explicit_model_ids: model.into_iter().collect(),
        ..Default::default()
    };
    if has(args, "--offline") {
        if opts.explicit_model_ids.is_empty() {
            return Err("--offline requires --model".into());
        }
        print_value(serde_json::to_value(&opts.explicit_model_ids).map_err(|e| e.to_string())?)?;
        return Ok(0);
    };
    let found = Discovery::new()
        .discover(&spec, &opts)
        .await
        .map_err(|e| e.to_string())?;
    print_value(serde_json::to_value(&found).map_err(|e| e.to_string())?)?;
    Ok(0)
}
pub(crate) fn sessions(args: &[String], cfg: &Config) -> Result<(), String> {
    let st = store(args, Some(cfg))?;
    match args.get(1).map(String::as_str) {
        Some("list") | None => print_value(json!(st.sessions().map_err(|e| e.to_string())?))?,
        Some("show") => {
            let value = if has(args, "--snapshot") {
                serde_json::to_value(
                    st.load_snapshot(args.get(2).ok_or("session ID required")?)
                        .map_err(|e| e.to_string())?,
                )
            } else {
                serde_json::to_value(
                    st.records(args.get(2).ok_or("session ID required")?)
                        .map_err(|e| e.to_string())?,
                )
            }
            .map_err(|e| e.to_string())?;
            print_value(value)?;
        }
        Some("delete") => {
            st.delete(args.get(2).ok_or("session ID required")?)
                .map_err(|e| e.to_string())?;
            println!("deleted")
        }
        _ => return Err("sessions requires list, show, or delete".into()),
    }
    Ok(())
}

use crate::{
    args::{has, value},
    config::{store, Config},
    output::{print_value, safe_config},
};
use serde_json::json;
use servoloop_providers::{Discovery, DiscoveryOptions, ProviderSpec};
use std::{env, process::ExitCode};

pub(crate) fn providers() -> ExitCode {
    println!(
        "{}",
        json!({"version":1,"providers":["openai","openrouter","nvidia","ollama"]})
    );
    ExitCode::SUCCESS
}

pub(crate) fn config(args: &[String], cfg: &Config) -> Result<i32, String> {
    match crate::args::config_action(args) {
        "validate" => {
            println!("valid");
            Ok(0)
        }
        "show" => {
            print_value(safe_config(cfg))?;
            Ok(0)
        }
        "init" => Err("config init must be handled before loading configuration".into()),
        action => Err(format!("unknown config action `{action}`")),
    }
}
