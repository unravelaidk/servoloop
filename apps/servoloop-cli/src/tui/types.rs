//! Shared presentation contracts. Runtime facts are separate from UI navigation.
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Settings {
    pub demo: bool,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub store_path: PathBuf,
    pub config_path: PathBuf,
}

impl Settings {
    pub fn model_label(&self) -> String {
        if self.demo {
            "Scripted / offline demo".into()
        } else {
            format!("{} / {}", self.model, self.provider)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub role: Role,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ToolEntry {
    pub id: String,
    pub name: String,
    pub status: String,
    pub details: String,
    pub is_error: bool,
}

#[derive(Clone, Debug)]
pub struct SessionItem {
    pub id: String,
    pub status: String,
    pub resumable: bool,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct ModelItem {
    pub id: String,
    pub tools: String,
}

#[derive(Clone, Debug)]
pub enum BackendCommand {
    Submit { prompt: String, settings: Settings },
    NewSession,
    Resume { id: String, settings: Settings },
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum BackendEvent {
    SessionReady {
        id: String,
        history: Vec<ChatMessage>,
        resumed: bool,
    },
    Status(String),
    Tool(ToolEntry),
    Message(ChatMessage),
    Finished {
        summary: String,
        blocked: bool,
        snapshot_saved: bool,
    },
    Error {
        message: String,
        blocked: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Welcome,
    Chat,
    Setup,
    Models,
    Sessions,
    Preflight,
    Inspect,
    Commands,
    Help,
    Update,
}

#[derive(Clone, Debug)]
pub enum ServiceEvent {
    Models(Result<Vec<ModelItem>, String>),
    Connection(Result<String, String>),
    Sessions(Result<Vec<SessionItem>, String>),
    Saved(Result<String, String>),
}

pub struct App {
    pub view: View,
    pub return_view: View,
    pub settings: Settings,
    pub pending: Settings,
    pub draft: String,
    pub cursor: usize,
    pub last_prompt: String,
    pub run_start_tool_count: usize,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolEntry>,
    pub sessions: Vec<SessionItem>,
    pub models: Vec<ModelItem>,
    pub selection: usize,
    pub field: usize,
    pub query: String,
    pub session_id: String,
    pub status: String,
    pub notice: String,
    pub busy: bool,
    pub blocked: bool,
    pub service_busy: bool,
    pub snapshot_saved: bool,
    pub resumed: bool,
    pub scroll: u16,
    pub detail_scroll: u16,
    pub unseen: usize,
    pub quitting: bool,
}

impl App {
    pub fn new(settings: Settings) -> Self {
        Self {
            view: View::Welcome,
            return_view: View::Chat,
            pending: settings.clone(),
            settings,
            draft: String::new(),
            cursor: 0,
            last_prompt: String::new(),
            run_start_tool_count: 0,
            messages: vec![],
            tools: vec![],
            sessions: vec![],
            models: vec![],
            selection: 0,
            field: 0,
            query: String::new(),
            session_id: String::new(),
            status: "First launch · no commands sent".into(),
            notice: String::new(),
            busy: false,
            blocked: false,
            service_busy: false,
            snapshot_saved: false,
            resumed: false,
            scroll: 0,
            detail_scroll: 0,
            unseen: 0,
            quitting: false,
        }
    }

    pub fn selected_session(&self) -> Option<&SessionItem> {
        self.sessions
            .iter()
            .filter(|s| s.id.to_lowercase().contains(&self.query.to_lowercase()))
            .nth(self.selection)
    }
}
