//! Settings and read-only inspection services; no robot or agent loop is constructed here.
use super::types::{ModelItem, SessionItem, Settings};
use servoloop_core::{Message, Model, ModelRequest};
use servoloop_providers::{
    Discovery, DiscoveryOptions, OpenAiCompatProvider, ProviderSpec, ToolSupport,
};
use servoloop_store::{Store, StoreError};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub fn initial_settings(cfg: &crate::Config) -> Settings {
    let provider = crate::setting(
        &[],
        "--provider",
        "SERVOLOOP_PROVIDER",
        cfg.provider.clone(),
    )
    .unwrap_or_default();
    let model =
        crate::setting(&[], "--model", "SERVOLOOP_MODEL", cfg.model.clone()).unwrap_or_default();
    let base_url = crate::setting(
        &[],
        "--base-url",
        "SERVOLOOP_BASE_URL",
        cfg.base_url.clone(),
    )
    .unwrap_or_default();
    // Never populate an editable field with credentials from an existing config/env.
    let base_url = if validate_url(&base_url).is_ok()
        && crate::redact(&base_url) == base_url
        && !env::var("OLLAMA_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .is_some_and(|k| base_url.contains(&k))
    {
        base_url
    } else {
        "[unsafe URL removed; enter a credential-free endpoint]".into()
    };
    Settings {
        demo: provider.trim().is_empty() || model.trim().is_empty(),
        provider: safe(&provider),
        model: safe(&model),
        base_url,
        store_path: env::var_os("SERVOLOOP_STORE")
            .map(PathBuf::from)
            .or_else(|| cfg.store.clone())
            .unwrap_or_else(crate::default_store_path),
        config_path: env::var_os("SERVOLOOP_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(crate::default_config_path),
    }
}

/// Strip known keys (including Ollama's key) and whole URLs from diagnostics.
/// Whole-URL removal also covers query tokens, fragments, and credential-bearing paths.
fn safe(text: &str) -> String {
    let mut text = crate::redact(text);
    if let Ok(key) = env::var("OLLAMA_API_KEY") {
        if !key.is_empty() {
            text = text.replace(&key, "[REDACTED]");
        }
    }
    sanitize_urls(&text)
}

fn sanitize_urls(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_inclusive(char::is_whitespace) {
        if let Some(scheme) = word.find("://") {
            let start = word[..scheme]
                .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '-' && c != '.')
                .map_or(0, |i| i + 1);
            out.push_str(&word[..start]);
            out.push_str("[URL redacted]");
            out.extend(
                word.chars()
                    .rev()
                    .take_while(|c| c.is_whitespace())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        } else {
            out.push_str(word);
        }
    }
    out.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

// No URL parser dependency is needed: accept a deliberately narrow credential-free
// base URL grammar; the provider's HTTP client performs full URL validation.
fn validate_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Ok(());
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or("base URL must start with http:// or https://")?;
    if rest.is_empty()
        || rest.starts_with('/')
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
        || url.contains(['@', '?', '#', '\\', '%'])
    {
        return Err("base URL must not contain credentials, queries, fragments, escapes, or whitespace; use an environment API key".into());
    }
    Ok(())
}

fn spec(settings: &Settings) -> Result<ProviderSpec, String> {
    validate_url(&settings.base_url)?;
    let spec = crate::provider(
        settings.provider.trim(),
        (!settings.base_url.is_empty()).then(|| settings.base_url.clone()),
    )
    .map_err(|e| safe(&e))?;
    // Provider-specific base URL env vars are consulted by ProviderSpec too.
    validate_url(&spec.resolve_endpoint())?;
    spec.validate().map_err(|e| safe(&e.to_string()))?;
    Ok(spec)
}

/// Discovery is a catalog lookup, never proof that a model can be invoked.
/// The explicit ID survives endpoint failure; all capabilities remain uncertain
/// without metadata. No third-party models.dev request is made.
pub async fn discover_models(settings: &Settings) -> Result<Vec<ModelItem>, String> {
    let spec = spec(settings)?;
    let options = DiscoveryOptions {
        include_models_dev: false,
        filter_for_tools: false,
        explicit_model_ids: if settings.model.trim().is_empty() {
            vec![]
        } else {
            vec![settings.model.trim().into()]
        },
        timeout: Duration::from_secs(8),
        ..Default::default()
    };
    let discovery = Discovery::new();
    let models = tokio::time::timeout(Duration::from_secs(10), discovery.discover(&spec, &options))
        .await
        .map_err(|_| {
            "model catalog lookup timed out; manual model ID is still available".to_string()
        })?
        .map_err(|e| safe(&e.to_string()))?;
    Ok(models
        .into_iter()
        .map(|m| ModelItem {
            id: safe(&m.model_id),
            tools: match m.tool_support {
                ToolSupport::Yes => "yes",
                ToolSupport::No => "no",
                ToolSupport::Unknown => "unknown",
            }
            .into(),
        })
        .collect())
}

/// Exactly one small text completion, bounded in time and output. No tools,
/// session persistence, retry loop, or robot objects exist on this path.
pub async fn test_connection(settings: &Settings) -> Result<String, String> {
    if settings.demo {
        return Ok(
            "Offline demo selected; no model connection was tested and no robot was contacted."
                .into(),
        );
    }
    if settings.model.trim().is_empty() {
        return Err("enter a model ID before testing".into());
    }
    let model = OpenAiCompatProvider::new(spec(settings)?, settings.model.trim())
        .map_err(|e| safe(&e.to_string()))?;
    let mut request = ModelRequest::new(
        "servoloop-connection-test",
        vec![Message::user_text(
            "Reply with the single word hello. This is a text-only connection check.",
        )],
    );
    request.max_output = Some(16);
    request.deadline = Some(Duration::from_secs(15));
    let response = tokio::time::timeout(Duration::from_secs(15), model.complete(request)).await
        .map_err(|_| "model call timed out after 15 seconds; catalog access alone does not prove invocation works".to_string())?
        .map_err(|e| safe(&format!("model call failed (catalog access is not proof of invocation): {e}")))?;
    if !response.tool_calls.is_empty() {
        return Err("provider returned unexpected tool calls; nothing was executed".into());
    }
    // Do not display untrusted model output or claim tool/motion capability.
    Ok(format!("Model call succeeded (not merely catalog access). No tools or robot were used; tool/image support remains untested.{}", env_notice()))
}

fn env_notice() -> String {
    let names = [
        "SERVOLOOP_PROVIDER",
        "SERVOLOOP_MODEL",
        "SERVOLOOP_BASE_URL",
        "SERVOLOOP_STORE",
        "SERVOLOOP_CONFIG",
        "OPENAI_BASE_URL",
        "OPENROUTER_BASE_URL",
        "NVIDIA_BASE_URL",
        "OLLAMA_BASE_URL",
    ];
    let active: Vec<_> = names
        .into_iter()
        .filter(|n| env::var_os(n).is_some())
        .collect();
    if active.is_empty() {
        String::new()
    } else {
        format!(" Environment settings present: {}. Setup selections apply to this UI; environment values take precedence over saved config on next launch (provider URL env applies when base URL is blank).", active.join(", "))
    }
}

pub fn list_sessions(settings: &Settings) -> Result<Vec<SessionItem>, String> {
    list_sessions_at(&settings.store_path).map_err(|e| safe(&e))
}

fn list_sessions_at(path: &Path) -> Result<Vec<SessionItem>, String> {
    check_ancestors(path)?;
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.to_string()),
        Ok(m) if !m.is_dir() => return Err("store path is not a directory".into()),
        _ => {}
    }
    let store = Store::open(path).map_err(|e| e.to_string())?;
    // Enumerate first: Store::records/acquire_session create a directory for unknown IDs.
    let ids = store.sessions().map_err(|e| e.to_string())?;
    Ok(ids.into_iter().map(|id| {
        let result = inspect_session(&store, &id);
        let (status, resumable, detail) = match result {
            Ok((status, resumable, detail)) => (status, resumable, detail),
            Err(StoreError::Busy | StoreError::LockTimeout) => ("busy", false, "Session is locked; not inspected. Retry after its owner exits.".into()),
            Err(StoreError::Unresolved) => ("unresolved", false, "Unknown tool outcome; automatic resume is blocked. Operator reconciliation is required.".into()),
            Err(e) => ("invalid", false, safe(&e.to_string())),
        };
        SessionItem { id, status: status.into(), resumable, detail }
    }).collect())
}

fn inspect_session(store: &Store, id: &str) -> Result<(&'static str, bool, String), StoreError> {
    let dir = store.root().join(id);
    if !fs::symlink_metadata(&dir)?.is_dir() {
        return Err(StoreError::InvalidId);
    }
    // Guard against dangling links too (the store's exists-based check cannot).
    for name in ["journal.ndjson", "snapshot.json"] {
        match fs::symlink_metadata(dir.join(name)) {
            Ok(m) if !m.is_file() || m.file_type().is_symlink() => {
                return Err(StoreError::InvalidId)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
            _ => {}
        }
    }
    let guard = store.acquire_session(id)?;
    match fs::symlink_metadata(dir.join("journal.lock")) {
        Ok(_) => return Err(StoreError::Busy),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if !store.unresolved(id)?.is_empty() {
        return Err(StoreError::Unresolved);
    }
    match store.load_snapshot_guarded(&guard) {
        Ok(session) => Ok((
            "ready",
            true,
            format!(
                "Validated snapshot; {} messages. Resume will revalidate under its own lease.",
                session.messages.len()
            ),
        )),
        Err(StoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            let count = store.records(id)?.len();
            Ok((
                if count == 0 { "empty" } else { "journal-only" },
                false,
                format!("{count} journal records; no resumable snapshot."),
            ))
        }
        Err(e) => Err(e),
    }
}

pub fn save_settings(settings: &Settings) -> Result<String, String> {
    save_settings_at(settings, &settings.config_path).map_err(|e| safe(&e))?;
    Ok(safe(&format!("Saved config v1 to {}. Existing store/driver settings were preserved. Demo mode is UI-only and is not stored in config v1.{}", settings.config_path.display(), env_notice())))
}

fn check_ancestors(path: &Path) -> Result<(), String> {
    for part in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        match fs::symlink_metadata(part) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err("symlinked paths are not allowed".into())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("path inspection: {e}")),
            _ => {}
        }
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct ConfigImage {
    bytes: Vec<u8>,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

fn read_config_image(path: &Path) -> Result<Option<ConfigImage>, String> {
    check_ancestors(path)?;
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("config read: {e}")),
    };
    if !meta.is_file() {
        return Err("config must be a regular file".into());
    }
    const LIMIT: u64 = 1024 * 1024;
    if meta.len() > LIMIT {
        return Err("config exceeds 1 MiB".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("config exceeds 1 MiB".into());
    }
    Ok(Some(ConfigImage {
        bytes,
        modified: meta.modified().ok(),
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
        },
    }))
}

struct TempConfig(PathBuf);
impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn save_settings_at(settings: &Settings, path: &Path) -> Result<(), String> {
    save_settings_with_hook(settings, path, || {})
}

/// Validate editable values without making a request or requiring credentials yet.
pub fn validate_settings(settings: &Settings) -> Result<(), String> {
    validate_url(&settings.base_url)?;
    if !settings.provider.is_empty()
        && !["openai", "openrouter", "nvidia", "ollama"].contains(&settings.provider.as_str())
    {
        return Err("unknown provider; choose openai, openrouter, nvidia, or ollama".into());
    }
    if !settings.demo && (settings.provider.trim().is_empty() || settings.model.trim().is_empty()) {
        return Err("live mode requires a provider and model ID".into());
    }
    for value in [&settings.provider, &settings.model, &settings.base_url] {
        if value.chars().any(char::is_control)
            || crate::redact(value) != *value
            || env::var("OLLAMA_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
                .is_some_and(|k| value.contains(&k))
        {
            return Err("settings contain a secret or control character; nothing was saved".into());
        }
    }
    Ok(())
}

// Hook makes the optimistic concurrency check deterministic in tests.
fn save_settings_with_hook(
    settings: &Settings,
    path: &Path,
    before_commit: impl FnOnce(),
) -> Result<(), String> {
    validate_settings(settings)?;
    let previous = read_config_image(path)?;
    let mut value = match &previous {
        Some(image) => {
            // Deserialize the strict struct first (including duplicate-field validation).
            let config: crate::Config = serde_json::from_slice(&image.bytes).map_err(|_| {
                "existing config is invalid or has unsupported fields; left untouched".to_string()
            })?;
            if config.version != Some(1) {
                return Err("unsupported config version; expected 1; left untouched".into());
            }
            serde_json::from_slice::<serde_json::Value>(&image.bytes).map_err(|e| e.to_string())?
        }
        None => serde_json::json!({"version": 1}),
    };
    for (name, text) in [
        ("provider", &settings.provider),
        ("model", &settings.model),
        ("base_url", &settings.base_url),
    ] {
        if text.is_empty() {
            value.as_object_mut().unwrap().remove(name);
        } else {
            value[name] = serde_json::Value::String(text.clone());
        }
    }
    // Store and driver are deliberately not written: Settings holds the effective
    // store path, which might be an environment override, not the persisted value.
    let mut bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("config directory: {e}"))?;
    check_ancestors(parent)?;
    let temp = parent.join(servoloop_store::new_id(".servoloop-config"));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|e| format!("config temporary file: {e}"))?;
    let _cleanup = TempConfig(temp.clone());
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("config write: {e}"))?;
    drop(file);
    before_commit();
    if read_config_image(path)? != previous {
        return Err("config changed while saving; retry after reviewing the current file".into());
    }
    // Atomic replacement: never unlink the destination. Parent directories must
    // be trusted; portable std cannot eliminate the final check/rename race.
    fs::rename(&temp, path)
        .map_err(|e| format!("config replacement failed; previous file retained: {e}"))?;
    // Rename is the commit point. A later sync error must not report that the old
    // file survived; syncing the temporary file above guarantees its data first.
    #[cfg(unix)]
    {
        let _ = fs::File::open(parent).and_then(|f| f.sync_all());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path =
                PathBuf::from("/tmp/opencode").join(servoloop_store::new_id("tui-services-test"));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn settings(&self) -> Settings {
            Settings {
                demo: false,
                provider: "ollama".into(),
                model: "test-model".into(),
                base_url: "http://localhost:11434/v1".into(),
                store_path: self.0.join("store"),
                config_path: self.0.join("config.json"),
            }
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn save_preserves_unrelated_fields() {
        let dir = Scratch::new();
        let settings = dir.settings();
        fs::write(
            &settings.config_path,
            br#"{"version":1,"store":"original-store","driver":"simulated","model":"old"}"#,
        )
        .unwrap();
        save_settings_at(&settings, &settings.config_path).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&settings.config_path).unwrap()).unwrap();
        assert_eq!(value["store"], "original-store");
        assert_eq!(value["driver"], "simulated");
        assert_eq!(value["model"], "test-model");
        assert!(value.get("demo").is_none());
    }

    #[test]
    fn invalid_config_and_secret_urls_are_untouched() {
        let dir = Scratch::new();
        let mut settings = dir.settings();
        for original in [
            "garbage",
            "{\"version\":2}",
            "{\"version\":1,\"api_key\":\"secret\"}",
            "{\"version\":1,\"version\":1}",
        ] {
            fs::write(&settings.config_path, original).unwrap();
            assert!(save_settings_at(&settings, &settings.config_path).is_err());
            assert_eq!(fs::read_to_string(&settings.config_path).unwrap(), original);
        }
        fs::write(&settings.config_path, "{\"version\":1}").unwrap();
        for url in [
            "http://user:password@example.com/v1",
            "https://example.com/v1?key=secret",
            "https://example.com/%73ecret",
        ] {
            settings.base_url = url.into();
            assert!(save_settings_at(&settings, &settings.config_path).is_err());
            assert_eq!(
                fs::read_to_string(&settings.config_path).unwrap(),
                "{\"version\":1}"
            );
        }
    }

    #[test]
    fn concurrent_change_is_not_clobbered() {
        let dir = Scratch::new();
        let settings = dir.settings();
        fs::write(&settings.config_path, "{\"version\":1}").unwrap();
        let changed = "{\"version\":1,\"driver\":\"other\"}";
        let result = save_settings_with_hook(&settings, &settings.config_path, || {
            fs::write(&settings.config_path, changed).unwrap()
        });
        assert!(result.unwrap_err().contains("changed"));
        assert_eq!(fs::read_to_string(&settings.config_path).unwrap(), changed);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn existing_and_dangling_symlinks_are_untouched() {
        use std::os::unix::fs::symlink;
        let dir = Scratch::new();
        let settings = dir.settings();
        let target = dir.0.join("target");
        fs::write(&target, "{\"version\":1}").unwrap();
        symlink(&target, &settings.config_path).unwrap();
        assert!(save_settings_at(&settings, &settings.config_path).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"version\":1}");
        fs::remove_file(&target).unwrap();
        assert!(save_settings_at(&settings, &settings.config_path).is_err());
        assert!(fs::symlink_metadata(&settings.config_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!target.exists());
    }

    #[test]
    fn sessions_include_journals_and_release_inspection_locks() {
        let dir = Scratch::new();
        let settings = dir.settings();
        assert!(list_sessions_at(&settings.store_path).unwrap().is_empty());
        assert!(!settings.store_path.exists());
        let store = Store::open(&settings.store_path).unwrap();
        store.create_session("empty").unwrap();
        store
            .save_snapshot(&servoloop_core::Session::new("ready"))
            .unwrap();
        store.create_session("invalid").unwrap();
        fs::write(settings.store_path.join("invalid/snapshot.json"), "garbage").unwrap();
        let busy = store.acquire_session("busy").unwrap();
        store
            .append(servoloop_store::JournalRecord {
                version: 1,
                sequence: 0,
                session_id: "unresolved".into(),
                intent_id: "intent".into(),
                kind: "intent".into(),
                arguments: serde_json::json!({}),
                outcome: None,
            })
            .unwrap();
        store
            .append(servoloop_store::JournalRecord {
                version: 1,
                sequence: 0,
                session_id: "journal".into(),
                intent_id: "intent".into(),
                kind: "intent".into(),
                arguments: serde_json::json!({}),
                outcome: None,
            })
            .unwrap();
        store
            .append(servoloop_store::JournalRecord {
                version: 1,
                sequence: 0,
                session_id: "journal".into(),
                intent_id: "intent".into(),
                kind: "result".into(),
                arguments: serde_json::json!({}),
                outcome: Some("verified".into()),
            })
            .unwrap();
        let items = list_sessions_at(&settings.store_path).unwrap();
        for (id, status, resumable) in [
            ("ready", "ready", true),
            ("busy", "busy", false),
            ("invalid", "invalid", false),
            ("unresolved", "unresolved", false),
            ("empty", "empty", false),
            ("journal", "journal-only", false),
        ] {
            let item = items.iter().find(|s| s.id == id).unwrap();
            assert_eq!(item.status, status);
            assert_eq!(item.resumable, resumable);
            if id != "busy" {
                assert!(!settings.store_path.join(id).join("session.lock").exists());
            }
        }
        assert_eq!(store.sessions().unwrap().len(), 6);
        drop(busy);
    }

    #[test]
    fn url_diagnostics_do_not_expose_credentials_or_queries() {
        assert_eq!(
            sanitize_urls("failed https://user:pass@host/path?token=secret next"),
            "failed [URL redacted] next"
        );
    }
}
