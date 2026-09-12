//! Stateful Paper workflows. Navigation does not dispatch tools. Network
//! discovery and session inspection are explicit, cancellable operations.
use super::state::safe_text;
use crate::{
    commands::provider,
    config::{store, store_path, Config},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use servoloop_providers::{
    DiscoveredModel, Discovery, DiscoveryOptions, ToolSupport, BUILTIN_NVIDIA, BUILTIN_OLLAMA,
    BUILTIN_OPENAI, BUILTIN_OPENROUTER,
};
use std::sync::Arc;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Page {
    Setup,
    Sessions,
    Preflight,
    Conversation,
    Palette,
    Updates,
}

pub(super) enum Intent {
    None,
    Back,
    Demo,
    Inspect,
    Help,
    Send,
}

enum Loaded {
    Models(Vec<DiscoveredModel>),
    Sessions(Vec<String>),
    Preflight { id: String, history: Vec<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PickerKind {
    Provider,
    Model,
}

pub(super) struct Picker {
    pub kind: PickerKind,
    pub query: String,
    pub selected: usize,
}

pub(super) struct Choice {
    pub id: String,
    pub title: String,
    pub detail: String,
}

fn providers() -> Vec<Choice> {
    [
        BUILTIN_OPENAI,
        BUILTIN_OPENROUTER,
        BUILTIN_NVIDIA,
        BUILTIN_OLLAMA,
    ]
    .into_iter()
    .map(|p| Choice {
        id: p.id.into(),
        title: p.display_name.into(),
        detail: format!(
            "{} · {}",
            p.default_endpoint,
            if p.env_key_names.is_empty() {
                "No API key required".into()
            } else {
                p.env_key_names.join(" / ")
            }
        ),
    })
    .collect()
}

pub(super) struct Workspace {
    pub page: Page,
    pub return_page: Page,
    pub config: Config,
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub draft: String,
    pub query: String,
    pub selected: usize,
    pub field: usize,
    pub editing: bool,
    pub status: String,
    pub sessions: Vec<String>,
    pub models: Vec<DiscoveredModel>,
    pub session: Option<String>,
    pub history: Vec<String>,
    pub preflight_ok: bool,
    pub settings: Option<super::settings::Settings>,
    pub picker: Option<Picker>,
    discovery: Arc<Discovery>,
    job: Option<JoinHandle<Result<Loaded, String>>>,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            job.abort();
        }
    }
}

impl Workspace {
    pub fn paste(&mut self, text: &str) {
        let target = if let Some(picker) = &mut self.picker {
            picker.selected = 0;
            &mut picker.query
        } else if self.editing {
            match self.page {
                Page::Setup => {
                    if self.field == 1 {
                        &mut self.endpoint
                    } else {
                        return;
                    }
                }
                Page::Conversation => &mut self.draft,
                _ => &mut self.query,
            }
        } else {
            return;
        };
        for ch in text.chars().filter(|c| !c.is_control()) {
            if target.len() + ch.len_utf8() > 8192 {
                break;
            }
            target.push(ch);
        }
    }

    pub fn new(cfg: &Config) -> Self {
        let mut cfg = cfg.clone();
        cfg.provider = std::env::var("SERVOLOOP_PROVIDER").ok().or(cfg.provider);
        cfg.model = std::env::var("SERVOLOOP_MODEL").ok().or(cfg.model);
        cfg.base_url = std::env::var("SERVOLOOP_BASE_URL").ok().or(cfg.base_url);
        Self {
            page: Page::Setup,
            return_page: Page::Conversation,
            config: cfg.clone(),
            provider: cfg.provider.clone().unwrap_or_else(|| "openai".into()),
            endpoint: cfg.base_url.clone().unwrap_or_default(),
            model: cfg.model.clone().unwrap_or_default(),
            draft: String::new(),
            query: String::new(),
            selected: 0,
            field: 0,
            editing: false,
            status: String::new(),
            sessions: vec![],
            models: vec![],
            session: None,
            history: vec![],
            preflight_ok: false,
            settings: None,
            picker: None,
            discovery: Arc::new(Discovery::new()),
            job: None,
        }
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub fn open(&mut self, page: Page, args: &[String]) {
        self.picker = None;
        self.cancel_job();
        self.page = page;
        self.selected = 0;
        self.field = 0;
        self.editing = false;
        self.query.clear();
        self.status.clear();
        if page == Page::Setup {
            self.provider = self
                .config
                .provider
                .clone()
                .unwrap_or_else(|| "openai".into());
            self.endpoint = self.config.base_url.clone().unwrap_or_default();
            self.model = self.config.model.clone().unwrap_or_default();
        }
        if page == Page::Sessions {
            self.sessions.clear();
            let args = args.to_vec();
            let cfg = self.config.clone();
            self.status = "Loading local sessions…".into();
            self.job = Some(tokio::task::spawn_blocking(move || {
                if !store_path(&args, Some(&cfg)).exists() {
                    return Ok(Loaded::Sessions(vec![]));
                }
                let mut sessions = store(&args, Some(&cfg))?
                    .sessions()
                    .map_err(|e| e.to_string())?;
                sessions.sort();
                Ok(Loaded::Sessions(sessions))
            }));
        }
    }

    pub fn cancel_job(&mut self) {
        if let Some(job) = self.job.take() {
            job.abort();
        }
    }

    pub async fn poll(&mut self) {
        if !self.job.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        let result = self
            .job
            .take()
            .expect("finished job")
            .await
            .unwrap_or_else(|e| Err(format!("Background operation failed: {e}")));
        match result {
            Ok(Loaded::Models(models)) => {
                let endpoint_count = models.iter().filter(|m| m.from_endpoint).count();
                self.models = models;
                self.status = if endpoint_count > 0 {
                    format!("Endpoint returned {endpoint_count} models. This is not a tool-execution test.")
                } else {
                    "No endpoint models confirmed. Enter a model ID manually; capabilities remain unknown.".into()
                };
            }
            Ok(Loaded::Sessions(sessions)) => {
                self.status = format!("{} saved sessions · local storage", sessions.len());
                self.sessions = sessions;
            }
            Ok(Loaded::Preflight { id, history }) => {
                self.session = Some(id);
                self.history = history;
                self.preflight_ok = true;
                self.status =
                    "Session and journal consistency checks passed. Rechecked on Send.".into();
            }
            Err(error) => {
                self.status = safe_text(&error);
                self.preflight_ok = false;
            }
        }
    }

    pub fn filtered_sessions(&self) -> Vec<&String> {
        self.sessions
            .iter()
            .filter(|s| s.to_lowercase().contains(&self.query.to_lowercase()))
            .collect()
    }

    pub fn palette(&self) -> Vec<(&'static str, &'static str)> {
        [
            ("/model", "Choose a provider and model"),
            ("/sessions", "Find and resume a saved conversation"),
            ("/inspect", "Review the active execution and environment"),
            ("/update", "Installation and update guidance"),
            ("/help", "Keyboard shortcuts and simulator limitations"),
            ("/demo", "Review the fixed offline demo"),
        ]
        .into_iter()
        .filter(|(name, detail)| format!("{name} {detail}").contains(&self.query.to_lowercase()))
        .collect()
    }

    pub fn key(&mut self, key: KeyEvent, args: &[String], running: bool) -> Intent {
        if self.picker.is_some() {
            self.picker_key(key);
            return Intent::None;
        }
        if key.code == KeyCode::Esc {
            if self.editing {
                self.editing = false;
                return Intent::None;
            }
            self.cancel_job();
            match self.page {
                Page::Preflight => self.open(Page::Sessions, args),
                Page::Palette => {
                    self.page = self.return_page;
                    self.query.clear();
                }
                _ => return Intent::Back,
            }
            return Intent::None;
        }
        if self.editing {
            if key.code == KeyCode::Enter {
                self.editing = false;
                return Intent::None;
            }
            let target = match self.page {
                Page::Setup => match self.field {
                    0 => &mut self.provider,
                    1 => &mut self.endpoint,
                    _ => &mut self.model,
                },
                Page::Conversation => &mut self.draft,
                _ => &mut self.query,
            };
            edit(target, key);
            return Intent::None;
        }
        if key.code == KeyCode::Char('/') {
            self.return_page = self.page;
            self.open(Page::Palette, args);
            return Intent::None;
        }
        match self.page {
            Page::Setup => match key.code {
                KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % 5,
                KeyCode::BackTab | KeyCode::Up => self.field = (self.field + 4) % 5,
                KeyCode::Enter if self.field == 0 => self.open_picker(PickerKind::Provider),
                KeyCode::Enter if self.field == 1 => self.editing = true,
                KeyCode::Enter if self.field == 2 || self.field == 3 => {
                    self.open_picker(PickerKind::Model)
                }
                KeyCode::Enter if self.field == 4 => self.apply(),
                _ => {}
            },
            Page::Sessions => match key.code {
                KeyCode::Char('e') => self.editing = true,
                KeyCode::Char('r') => self.open(Page::Sessions, args),
                KeyCode::Down => {
                    self.selected = self
                        .selected
                        .saturating_add(1)
                        .min(self.filtered_sessions().len().saturating_sub(1))
                }
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Enter if !self.busy() => {
                    if let Some(id) = self
                        .filtered_sessions()
                        .get(self.selected)
                        .map(|s| (*s).clone())
                    {
                        self.preflight(args, id);
                    }
                }
                _ => {}
            },
            Page::Preflight => {
                if key.code == KeyCode::Enter && self.preflight_ok && !self.busy() {
                    self.page = Page::Conversation;
                    self.status =
                        "Conversation restored. Fresh simulator; no commands replayed.".into();
                }
            }
            Page::Conversation => match key.code {
                KeyCode::Char('m') => {
                    self.open(Page::Setup, args);
                    self.field = 2;
                    self.open_picker(PickerKind::Model);
                }
                KeyCode::Char('e') => self.editing = true,
                KeyCode::Char('s') if !running && !self.draft.trim().is_empty() => {
                    return Intent::Send
                }
                KeyCode::Char('i') => return Intent::Inspect,
                KeyCode::Char('1') => self.draft = "Inspect the current joint positions.".into(),
                KeyCode::Char('2') => {
                    self.draft =
                        "Move the shoulder to 0.2 radians and verify the final position.".into()
                }
                KeyCode::Char('3') => {
                    self.draft = "Explain the execution limits before any motion.".into()
                }
                _ => {}
            },
            Page::Palette => match key.code {
                KeyCode::Char('e') => self.editing = true,
                KeyCode::Down => {
                    self.selected = self
                        .selected
                        .saturating_add(1)
                        .min(self.palette().len().saturating_sub(1))
                }
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Enter => {
                    if let Some((name, _)) = self.palette().get(self.selected).copied() {
                        match name {
                            "/model" => self.open(Page::Setup, args),
                            "/sessions" if !running => self.open(Page::Sessions, args),
                            "/sessions" => {
                                self.status =
                                    "Wait for the active run to settle before switching sessions."
                                        .into()
                            }
                            "/inspect" => return Intent::Inspect,
                            "/help" => return Intent::Help,
                            "/demo" if !running => return Intent::Demo,
                            "/demo" => {
                                self.status = "Wait for cleanup before opening another run.".into()
                            }
                            "/update" => self.open(Page::Updates, args),
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            Page::Updates => {}
        }
        Intent::None
    }

    fn discover(&mut self) {
        if let Err(error) = validate_endpoint(&self.endpoint) {
            self.status = error;
            return;
        }
        self.cancel_job();
        let spec = match provider(
            &self.provider,
            (!self.endpoint.is_empty()).then(|| self.endpoint.clone()),
        ) {
            Ok(spec) => spec,
            Err(error) => {
                self.status = safe_text(&error);
                return;
            }
        };
        self.models.clear();
        self.selected = 0;
        self.status = "Loading Models.dev catalog and endpoint models… Nothing applied.".into();
        let discovery = self.discovery.clone();
        self.job = Some(tokio::spawn(async move {
            let options = DiscoveryOptions {
                include_models_dev: true,
                ..Default::default()
            };
            discovery
                .discover(&spec, &options)
                .await
                .map(Loaded::Models)
                .map_err(|e| e.to_string())
        }));
    }

    pub fn choices(&self, picker: &Picker) -> Vec<Choice> {
        let query = picker.query.trim().to_lowercase();
        let mut choices: Vec<_> = match picker.kind {
            PickerKind::Provider => providers(),
            PickerKind::Model => self
                .models
                .iter()
                .map(|m| Choice {
                    id: m.model_id.clone(),
                    title: if m.name.is_empty() || m.name == m.model_id {
                        m.model_id.clone()
                    } else {
                        format!("{} · {}", m.model_id, m.name)
                    },
                    detail: format!(
                        "Tools: {} · Images: {} · Context: {} · {}{}",
                        match m.tool_support {
                            ToolSupport::Yes => "supported",
                            ToolSupport::No => "not supported",
                            ToolSupport::Unknown => "unknown",
                        },
                        if m.modalities.image_input {
                            "supported"
                        } else {
                            "unknown"
                        },
                        if m.context_window == 0 {
                            "unknown".into()
                        } else {
                            m.context_window.to_string()
                        },
                        if m.from_endpoint {
                            "endpoint listed"
                        } else {
                            "catalog only"
                        },
                        if m.deprecated { " · deprecated" } else { "" }
                    ),
                })
                .collect(),
        }
        .into_iter()
        .filter(|c| {
            format!("{} {}", c.title, c.id)
                .to_lowercase()
                .contains(&query)
        })
        .collect();
        if picker.kind == PickerKind::Model
            && !picker.query.trim().is_empty()
            && !self
                .models
                .iter()
                .any(|m| m.model_id == picker.query.trim())
        {
            choices.push(Choice {
                id: picker.query.trim().into(),
                title: format!("Use custom model: {}", picker.query.trim()),
                detail: "Not discovered · capabilities unknown · selection does not test the model"
                    .into(),
            });
        }
        choices
    }

    pub fn open_picker(&mut self, kind: PickerKind) {
        self.editing = false;
        self.picker = Some(Picker {
            kind,
            query: String::new(),
            selected: 0,
        });
        if kind == PickerKind::Model {
            self.discover();
        } else {
            self.status =
                "Choose a supported provider. Endpoint overrides are edited separately.".into();
            if let Some(index) = providers().iter().position(|p| p.id == self.provider) {
                self.picker.as_mut().expect("opened picker").selected = index;
            }
        }
    }

    fn picker_key(&mut self, key: KeyEvent) {
        let mut picker = self.picker.take().expect("open picker");
        match key.code {
            KeyCode::Esc => {
                self.cancel_job();
                self.status = "Selection cancelled. Pending settings unchanged.".into();
                return;
            }
            KeyCode::Up | KeyCode::BackTab => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                picker.selected = picker
                    .selected
                    .saturating_add(1)
                    .min(self.choices(&picker).len().saturating_sub(1))
            }
            KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(5),
            KeyCode::PageDown => {
                picker.selected = picker
                    .selected
                    .saturating_add(5)
                    .min(self.choices(&picker).len().saturating_sub(1))
            }
            KeyCode::F(5) if picker.kind == PickerKind::Model => {
                picker.selected = 0;
                self.discovery.clear_cache();
                self.discover();
            }
            KeyCode::Enter => {
                if let Some(choice) = self.choices(&picker).get(picker.selected) {
                    match picker.kind {
                        PickerKind::Provider if choice.id != self.provider => {
                            self.provider = choice.id.clone();
                            self.endpoint.clear();
                            self.model.clear();
                            self.models.clear();
                            self.cancel_job();
                            self.status = "Provider selected. Choose a model; Save applies the pending settings.".into();
                        }
                        PickerKind::Provider => {
                            self.status = "Provider selection retained. Nothing saved.".into()
                        }
                        PickerKind::Model => {
                            self.model = choice.id.clone();
                            self.cancel_job();
                            self.status =
                                "Model selected. Save and continue to apply it to the next run."
                                    .into();
                        }
                    }
                    return;
                }
            }
            _ => {
                edit(&mut picker.query, key);
                picker.selected = 0;
            }
        }
        self.picker = Some(picker);
    }

    fn apply(&mut self) {
        if let Err(error) = validate_endpoint(&self.endpoint) {
            self.status = error;
            return;
        }
        if self.model.trim().is_empty() {
            self.status = "Enter an exact model ID before applying.".into();
            return;
        }
        let spec = provider(
            &self.provider,
            (!self.endpoint.is_empty()).then(|| self.endpoint.clone()),
        );
        if let Err(error) = spec.and_then(|p| p.validate().map_err(|e| e.to_string())) {
            self.status = safe_text(&error);
            return;
        }
        self.cancel_job();
        let mut config = self.config.clone();
        config.provider = Some(self.provider.clone());
        config.model = Some(self.model.clone());
        config.base_url = (!self.endpoint.is_empty()).then(|| self.endpoint.clone());
        if let Some(settings) = &mut self.settings {
            if let Err(error) = settings.save(&config) {
                self.status = safe_text(&error);
                return;
            }
        }
        self.config = config;
        self.page = Page::Conversation;
        self.status = "Settings applied to the next run. Active execution unchanged.".into();
    }

    fn preflight(&mut self, args: &[String], id: String) {
        self.cancel_job();
        self.page = Page::Preflight;
        self.preflight_ok = false;
        self.session = None;
        self.history.clear();
        self.status = "Checking session, journal, and execution lease…".into();
        let args = args.to_vec();
        let cfg = self.config.clone();
        self.job = Some(tokio::task::spawn_blocking(move || {
            let st = store(&args, Some(&cfg))?;
            if !st.sessions().map_err(|e| e.to_string())?.contains(&id) {
                return Err("Session no longer exists.".into());
            }
            let guard = st
                .acquire_session(&id)
                .map_err(|e| format!("Session cannot be opened: {e}"))?;
            let session = st
                .load_snapshot_guarded(&guard)
                .map_err(|e| format!("Session cannot be resumed: {e}"))?;
            if !st.unresolved(&id).map_err(|e| e.to_string())?.is_empty() {
                return Err("Unresolved outcomes: resume blocked. No motion replay.".into());
            }
            let history = session
                .messages
                .iter()
                .rev()
                .take(64)
                .rev()
                .map(|message| safe_text(&serde_json::to_string(message).unwrap_or_default()))
                .collect();
            Ok(Loaded::Preflight { id, history })
        }));
    }

    pub fn run_args(&self, args: &[String]) -> Result<Vec<String>, String> {
        let provider_name = self
            .config
            .provider
            .as_ref()
            .ok_or("Configure a provider before Send.")?;
        let model = self
            .config
            .model
            .as_ref()
            .ok_or("Choose a model before Send.")?;
        let mut run = vec![
            "run".into(),
            "--store".into(),
            store_path(args, Some(&self.config))
                .to_string_lossy()
                .into_owned(),
            "--provider".into(),
            provider_name.clone(),
            "--model".into(),
            model.clone(),
            "--prompt".into(),
            self.draft.clone(),
        ];
        if let Some(endpoint) = &self.config.base_url {
            run.extend(["--base-url".into(), endpoint.clone()]);
        }
        if let Some(session) = &self.session {
            run[0] = "resume".into();
            run.insert(1, session.clone());
        }
        Ok(run)
    }
}

fn validate_endpoint(endpoint: &str) -> Result<(), String> {
    if endpoint.is_empty() {
        return Ok(());
    }
    let url =
        url::Url::parse(endpoint).map_err(|_| "Enter an absolute HTTP(S) endpoint.".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("Enter an absolute HTTP(S) endpoint.".into());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Endpoint must not contain credentials, query parameters, or fragments. Use the provider credential environment variable.".into());
    }
    Ok(())
}

fn edit(text: &mut String, key: KeyEvent) {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return;
    }
    match key.code {
        KeyCode::Backspace => {
            text.pop();
        }
        KeyCode::Char(ch) if !ch.is_control() && text.len() + ch.len_utf8() <= 8192 => {
            text.push(ch)
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn workspace() -> Workspace {
        Workspace::new(&Config {
            provider: Some("ollama".into()),
            model: Some("local-test".into()),
            ..Config::default()
        })
    }
    async fn settle(workspace: &mut Workspace) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while workspace.busy() {
                workspace.poll().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(servoloop_store::new_id("ui-workflow-test")))
        }
        fn args(&self) -> Vec<String> {
            vec![
                "ui".into(),
                "--store".into(),
                self.0.to_string_lossy().into_owned(),
            ]
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if self.0.exists() {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    #[test]
    fn suggestions_and_editor_enter_do_not_send() {
        let mut w = workspace();
        w.page = Page::Conversation;
        assert!(matches!(
            w.key(key(KeyCode::Char('2')), &[], false),
            Intent::None
        ));
        assert!(w.draft.contains("0.2"));
        w.key(key(KeyCode::Char('e')), &[], false);
        w.key(key(KeyCode::Char('q')), &[], false);
        assert!(w.draft.ends_with('q'));
        assert!(matches!(
            w.key(key(KeyCode::Enter), &[], false),
            Intent::None
        ));
        assert!(matches!(
            w.key(key(KeyCode::Char('s')), &[], true),
            Intent::None
        ));
        assert!(matches!(
            w.key(key(KeyCode::Char('s')), &[], false),
            Intent::Send
        ));
    }

    #[test]
    fn palette_preserves_draft_and_configuration_requires_apply() {
        let mut w = workspace();
        w.page = Page::Conversation;
        w.draft = "inspect only".into();
        w.key(key(KeyCode::Char('/')), &[], false);
        assert_eq!(w.page, Page::Palette);
        w.key(key(KeyCode::Esc), &[], false);
        assert_eq!(w.page, Page::Conversation);
        assert_eq!(w.draft, "inspect only");
        let previous = w.config.model.clone();
        w.open(Page::Setup, &[]);
        w.model = "pending-model".into();
        assert_eq!(w.config.model, previous);
        w.open(Page::Setup, &[]);
        assert_eq!(Some(w.model.clone()), previous);
        w.model = "new-model".into();
        w.apply();
        assert_eq!(w.config.model.as_deref(), Some("new-model"));
        assert_eq!(w.draft, "inspect only");
    }

    #[tokio::test]
    async fn empty_session_browser_creates_nothing() {
        let fixture = Fixture::new();
        let mut w = workspace();
        w.open(Page::Sessions, &fixture.args());
        settle(&mut w).await;
        assert!(w.sessions.is_empty());
        assert!(!fixture.0.exists());
    }

    #[tokio::test]
    async fn preflight_checks_snapshot_and_lease_without_replaying() {
        let fixture = Fixture::new();
        let st = servoloop_store::Store::open(&fixture.0).unwrap();
        st.create_session("saved").unwrap();
        st.save_snapshot(&servoloop_core::Session::new("saved"))
            .unwrap();
        let mut w = workspace();
        w.open(Page::Sessions, &fixture.args());
        settle(&mut w).await;
        assert_eq!(w.sessions, vec!["saved"]);
        w.key(key(KeyCode::Enter), &fixture.args(), false);
        settle(&mut w).await;
        assert!(w.preflight_ok);
        assert_eq!(w.session.as_deref(), Some("saved"));
        assert!(st.records("saved").unwrap().is_empty());
        assert!(!fixture.0.join("saved/session.lock").exists());
        let guard = st.acquire_session("saved").unwrap();
        w.preflight(&fixture.args(), "saved".into());
        settle(&mut w).await;
        assert!(!w.preflight_ok);
        assert!(w.status.contains("busy"));
        assert!(fixture.0.join("saved/session.lock").exists());
        assert!(matches!(
            w.key(key(KeyCode::Enter), &fixture.args(), false),
            Intent::None
        ));
        assert_eq!(w.page, Page::Preflight);
        drop(guard);
    }

    #[tokio::test]
    async fn corrupt_snapshot_is_not_resumable() {
        let fixture = Fixture::new();
        let st = servoloop_store::Store::open(&fixture.0).unwrap();
        st.create_session("broken").unwrap();
        std::fs::write(fixture.0.join("broken/snapshot.json"), "not json").unwrap();
        let mut w = workspace();
        w.preflight(&fixture.args(), "broken".into());
        settle(&mut w).await;
        assert!(!w.preflight_ok);
        assert!(w.session.is_none());
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("broken/snapshot.json")).unwrap(),
            "not json"
        );
    }

    #[test]
    fn selected_settings_override_environment_and_resume_keeps_id() {
        let fixture = Fixture::new();
        let mut w = workspace();
        w.session = Some("saved".into());
        w.draft = "observe only".into();
        let args = w.run_args(&fixture.args()).unwrap();
        assert_eq!(&args[..2], &["resume", "saved"]);
        assert_eq!(
            crate::args::value(&args, "--prompt").as_deref(),
            Some("observe only")
        );
        assert!(!fixture.0.exists());
    }

    #[test]
    fn provider_picker_search_cancel_and_confirm_preserve_applied_settings() {
        let mut w = workspace();
        let original = w.provider.clone();
        w.open_picker(PickerKind::Provider);
        for ch in "openrouter".chars() {
            w.key(key(KeyCode::Char(ch)), &[], false);
        }
        assert_eq!(w.choices(w.picker.as_ref().unwrap()).len(), 1);
        w.key(key(KeyCode::Esc), &[], false);
        assert_eq!(w.provider, original);
        assert!(w.picker.is_none());
        w.open_picker(PickerKind::Provider);
        for ch in "OPENROUTER".chars() {
            w.key(key(KeyCode::Char(ch)), &[], false);
        }
        w.key(key(KeyCode::Enter), &[], false);
        assert_eq!(w.provider, "openrouter");
        assert!(w.endpoint.is_empty());
        assert!(w.model.is_empty());
        assert_eq!(w.config.provider.as_deref(), Some(original.as_str()));
    }

    #[test]
    fn model_picker_preserves_capability_unknowns_and_allows_custom_ids() {
        let mut w = workspace();
        w.models = vec![DiscoveredModel {
            provider_id: "ollama".into(),
            model_id: "listed-model".into(),
            name: "Listed".into(),
            tool_support: ToolSupport::Unknown,
            deprecated: false,
            reasoning: false,
            modalities: Default::default(),
            context_window: 0,
            max_output: 0,
            from_endpoint: true,
        }];
        w.picker = Some(Picker {
            kind: PickerKind::Model,
            query: "listed".into(),
            selected: 0,
        });
        let choices = w.choices(w.picker.as_ref().unwrap());
        assert!(choices[0].detail.contains("Tools: unknown"));
        assert!(choices[0].detail.contains("Images: unknown"));
        assert!(choices[0].detail.contains("Context: unknown"));
        assert!(choices[0].detail.contains("endpoint listed"));
        let applied = w.config.model.clone();
        w.key(key(KeyCode::Enter), &[], false);
        assert_eq!(w.model, "listed-model");
        assert_eq!(w.config.model, applied);
        w.picker = Some(Picker {
            kind: PickerKind::Model,
            query: "custom-model".into(),
            selected: 0,
        });
        assert_eq!(w.choices(w.picker.as_ref().unwrap()).len(), 1);
        w.key(key(KeyCode::Enter), &[], false);
        assert_eq!(w.model, "custom-model");
        assert_eq!(w.config.model, applied);
    }

    #[test]
    fn endpoint_validation_rejects_embedded_secrets_and_non_http_urls() {
        for endpoint in [
            "file:///etc/passwd",
            "https://user:secret@host/v1",
            "https://host/v1?api_key=secret",
            "relative",
        ] {
            assert!(validate_endpoint(endpoint).is_err());
        }
        assert!(validate_endpoint("http://127.0.0.1:11434/v1").is_ok());
    }

    #[test]
    fn paste_edits_search_or_draft_without_confirming_or_dispatching() {
        let mut w = workspace();
        let original = w.config.provider.clone();
        w.open_picker(PickerKind::Provider);
        w.paste("openai\r\n");
        assert_eq!(w.picker.as_ref().unwrap().query, "openai");
        assert_eq!(w.config.provider, original);
        assert!(w.picker.is_some());
        w.key(key(KeyCode::Esc), &[], false);
        w.page = Page::Conversation;
        w.editing = true;
        w.paste("q\r\ns\r\n");
        assert_eq!(w.draft, "qs");
        assert!(w.editing);
        assert!(!w.busy());
    }
}
