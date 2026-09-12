#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) version: Option<u32>,
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) store: Option<PathBuf>,
    pub(crate) driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider_profile: Option<crate::catalog_provider::CatalogConnection>,
}

fn init_config(args: &[String]) -> Result<(), String> {
    let path = value(args, "--config")
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);
    if path.exists() {
        return Err(format!("config already exists: {}", path.display()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("config: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, b"{\n  \"version\": 1\n}\n").map_err(|e| format!("config: {e}"))?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("config: {e}"));
    }
    println!("{}", path.display());
    Ok(())
}

pub(crate) fn init(args: &[String]) -> ExitCode {
    match init_config(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

pub(crate) fn store(args: &[String], config: Option<&Config>) -> Result<Store, String> {
    Store::open(store_path(args, config)).map_err(|e| e.to_string())
}

/// Resolve without touching the filesystem, so a preview has no side effects.
pub(crate) fn store_path(args: &[String], config: Option<&Config>) -> PathBuf {
    value(args, "--store")
        .map(PathBuf::from)
        .or_else(|| env::var_os("SERVOLOOP_STORE").map(PathBuf::from))
        .or_else(|| config.and_then(|c| c.store.clone()))
        .unwrap_or_else(default_store_path)
}
fn default_store_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ServoLoop")
            .join("store")
    } else {
        env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("servoloop")
    }
}
fn default_config_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ServoLoop/config.json")
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("servoloop/config.json")
    }
}
pub(crate) fn load(args: &[String]) -> Result<Config, String> {
    let explicit = value(args, "--config").is_some() || env::var_os("SERVOLOOP_CONFIG").is_some();
    let path = config_path(args);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if !explicit && e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("config: {e}")),
    };
    let c: Config = serde_json::from_str(&text).map_err(|e| format!("config must be JSON: {e}"))?;
    if c.version != Some(1) {
        return Err(format!(
            "unsupported config version {:?}; expected 1",
            c.version
        ));
    }
    Ok(c)
}

pub(crate) fn config_path(args: &[String]) -> PathBuf {
    value(args, "--config")
        .map(PathBuf::from)
        .or_else(|| env::var_os("SERVOLOOP_CONFIG").map(PathBuf::from))
        .unwrap_or_else(default_config_path)
}

use crate::args::value;
use serde::{Deserialize, Serialize};
use servoloop_store::Store;
use std::process::ExitCode;
use std::{env, fs, io, path::PathBuf};
