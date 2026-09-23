//! Interactive terminal shell. Explicit CLI commands retain their NDJSON path.
mod backend;
mod services;
mod types;
mod view;

use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, time::Duration};
use tokio::sync::mpsc;
use types::*;

const ACTIONS: [&str; 7] = [
    "/model",
    "/sessions",
    "/inspect",
    "/new",
    "/update",
    "/help",
    "/quit",
];

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )?;
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            Show,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

pub(crate) async fn run(cfg: crate::Config, startup_error: Option<String>) -> Result<i32, String> {
    let _guard = TerminalGuard::enter().map_err(|e| format!("terminal setup: {e}"))?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).map_err(|e| format!("terminal: {e}"))?;
    terminal.clear().map_err(|e| e.to_string())?;
    let mut app = App::new(services::initial_settings(&cfg));
    if let Some(error) = startup_error {
        app.notice = error;
        app.settings.demo = true;
    }
    let mut runtime = backend::spawn();
    let (service_tx, mut service_rx) = mpsc::unbounded_channel();
    let result: Result<(), String> = async {
        loop {
            while let Ok(event) = runtime.events.try_recv() {
                apply_backend(&mut app, event);
            }
            while let Ok(event) = service_rx.try_recv() {
                apply_service(&mut app, event);
            }
            terminal
                .draw(|frame| view::render(frame, &app))
                .map_err(|e| e.to_string())?;
            if app.quitting && !app.busy && !app.service_busy {
                break;
            }
            if event::poll(Duration::from_millis(35)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        handle_key(&mut app, key, &runtime.commands, &service_tx);
                    }
                    Event::Paste(text) => paste(&mut app, &text),
                    Event::Mouse(_)
                    | Event::Resize(_, _)
                    | Event::FocusGained
                    | Event::FocusLost
                    | Event::Key(_) => {}
                }
            }
            if app.busy
                && (app.quitting || app.status == "Cancellation requested · waiting for cleanup")
            {
                runtime.cancel();
            }
            tokio::task::yield_now().await;
        }
        Ok(())
    }
    .await;
    runtime.cancel();
    let _ = runtime.commands.send(BackendCommand::Shutdown);
    let worker = runtime
        .task
        .await
        .map_err(|e| format!("runtime worker: {e}"));
    result?;
    worker?;
    Ok(0)
}

fn paste(app: &mut App, text: &str) {
    if app.service_busy {
        return;
    }
    // Bracketed paste is always data, never Enter or a slash-command action.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text: String = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    if app.view == View::Chat {
        insert(&mut app.draft, &mut app.cursor, &text);
    } else if app.view == View::Setup && !app.service_busy {
        pending_field(app).push_str(&text.replace(['\n', '\t'], ""));
    } else if matches!(app.view, View::Commands | View::Sessions | View::Models) {
        app.query.push_str(&text.replace(['\n', '\t'], ""));
        app.selection = 0;
    }
}

fn insert(text: &mut String, cursor: &mut usize, value: &str) {
    *cursor = (*cursor).min(text.len());
    while !text.is_char_boundary(*cursor) {
        *cursor -= 1;
    }
    // Bound UI input independently of provider limits; never silently submit a prefix.
    if text.len().saturating_add(value.len()) > 1024 * 1024 {
        return;
    }
    text.insert_str(*cursor, value);
    *cursor += value.len();
}

fn pending_field(app: &mut App) -> &mut String {
    match app.field {
        0 => &mut app.pending.provider,
        1 => &mut app.pending.base_url,
        _ => &mut app.pending.model,
    }
}

fn navigate(app: &mut App, next: View) {
    app.return_view = if app.view == View::Welcome {
        View::Welcome
    } else {
        View::Chat
    };
    app.view = next;
    app.selection = 0;
    app.detail_scroll = 0;
    app.query.clear();
}

fn action(
    app: &mut App,
    name: &str,
    commands: &mpsc::UnboundedSender<BackendCommand>,
    tx: &mpsc::UnboundedSender<ServiceEvent>,
) {
    match name {
        "/model" => {
            app.pending = app.settings.clone();
            app.pending.demo = false;
            app.field = 0;
            navigate(app, View::Setup);
        }
        "/sessions" => {
            navigate(app, View::Sessions);
            app.service_busy = true;
            let settings = app.settings.clone();
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                let _ = tx.send(ServiceEvent::Sessions(services::list_sessions(&settings)));
            });
        }
        "/inspect" => navigate(app, View::Inspect),
        "/help" => navigate(app, View::Help),
        "/update" => navigate(app, View::Update),
        "/new" if !app.busy && !app.blocked => {
            let settings = app.settings.clone();
            if commands.send(BackendCommand::NewSession).is_ok() {
                *app = App::new(settings);
                app.view = View::Chat;
                app.status = "New session · no commands sent".into();
            }
        }
        "/new" => {
            app.notice =
                "New session unavailable while execution is active or an outcome is unresolved."
                    .into()
        }
        "/quit" => {
            app.quitting = true;
            if app.busy {
                app.notice = "Quitting after cancellation and cleanup…".into();
            }
        }
        _ => app.notice = "Unknown command. Open / commands to see available actions.".into(),
    }
}

fn submit(
    app: &mut App,
    commands: &mpsc::UnboundedSender<BackendCommand>,
    tx: &mpsc::UnboundedSender<ServiceEvent>,
) {
    if app.draft.trim().is_empty() {
        return;
    }
    let command = app.draft.trim().to_owned();
    if ACTIONS.contains(&command.as_str()) {
        app.draft.clear();
        app.cursor = 0;
        action(app, &command, commands, tx);
        return;
    }
    if app.busy || app.blocked {
        app.notice = if app.blocked {
            "Send unavailable · resolve the recorded failure before continuing."
        } else {
            "Draft retained · a run is already active."
        }
        .into();
        return;
    }
    if !app.settings.demo
        && (app.settings.provider.trim().is_empty() || app.settings.model.trim().is_empty())
    {
        app.notice = "Choose a provider and model before sending, or use the offline demo.".into();
        return;
    }
    if commands
        .send(BackendCommand::Submit {
            prompt: app.draft.clone(),
            settings: app.settings.clone(),
        })
        .is_err()
    {
        app.notice = "Runtime unavailable. Draft retained; nothing submitted.".into();
        return;
    }
    app.messages.push(ChatMessage {
        role: Role::User,
        text: crate::redact(&app.draft),
    });
    app.last_prompt = app.draft.clone();
    app.run_start_tool_count = app.tools.len();
    app.draft.clear();
    app.cursor = 0;
    app.busy = true;
    app.snapshot_saved = false;
    app.notice.clear();
    app.status = "Starting run · no outcome yet".into();
    app.scroll = 0;
}

fn handle_key(
    app: &mut App,
    key: KeyEvent,
    commands: &mpsc::UnboundedSender<BackendCommand>,
    tx: &mpsc::UnboundedSender<ServiceEvent>,
) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if app.service_busy {
        if ctrl && key.code == KeyCode::Char('q') {
            app.quitting = true;
            app.notice = "Closing after the pending operation completes…".into();
        } else if ctrl && key.code == KeyCode::Char('c') {
            app.notice = "Operation pending · Ctrl+Q exits after completion.".into();
        }
        return;
    }
    if ctrl {
        match key.code {
            KeyCode::Char('q') => {
                action(app, "/quit", commands, tx);
                return;
            }
            KeyCode::Char('c') => {
                if app.busy {
                    app.status = "Cancellation requested · waiting for cleanup".into();
                } else if app.service_busy {
                    app.notice = "Operation pending · Ctrl+Q exits after completion.".into();
                } else if app.draft.is_empty() {
                    app.quitting = true;
                } else {
                    app.notice = "Draft retained · Ctrl+Q quits.".into();
                }
                return;
            }
            KeyCode::Char('k') => {
                navigate(app, View::Commands);
                return;
            }
            KeyCode::Char('h') => {
                navigate(app, View::Help);
                return;
            }
            _ => {}
        }
    }
    if key.code == KeyCode::Esc {
        if app.view != View::Chat && app.view != View::Welcome {
            if app.service_busy && matches!(app.view, View::Setup | View::Models) {
                app.notice =
                    "Operation pending · wait for its result before leaving settings.".into();
                return;
            }
            app.pending = app.settings.clone();
            app.view = app.return_view;
            app.query.clear();
            app.selection = 0;
        } else if app.view == View::Chat {
            navigate(app, View::Commands);
        }
        return;
    }
    if app.view != View::Chat && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
        app.detail_scroll = if key.code == KeyCode::PageUp {
            app.detail_scroll.saturating_sub(8)
        } else {
            app.detail_scroll.saturating_add(8)
        };
        return;
    }
    match app.view {
        View::Welcome => match key.code {
            KeyCode::Up => app.selection = app.selection.saturating_sub(1),
            KeyCode::Down => app.selection = (app.selection + 1).min(2),
            KeyCode::Enter => match app.selection {
                0 => {
                    app.settings.demo = true;
                    app.view = View::Chat;
                    app.status = "Ready · offline demo · no commands sent".into();
                }
                1 => action(app, "/model", commands, tx),
                _ => action(app, "/sessions", commands, tx),
            },
            _ => {}
        },
        View::Chat => match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                insert(&mut app.draft, &mut app.cursor, "\n")
            }
            KeyCode::Char('j') if ctrl => insert(&mut app.draft, &mut app.cursor, "\n"),
            KeyCode::Enter => submit(app, commands, tx),
            KeyCode::Char('/') if app.draft.is_empty() => navigate(app, View::Commands),
            KeyCode::Char('l') if ctrl => {
                app.draft.clear();
                app.cursor = 0;
            }
            KeyCode::Char('u') if ctrl => {
                app.draft.drain(..app.cursor);
                app.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                insert(&mut app.draft, &mut app.cursor, &c.to_string())
            }
            KeyCode::Backspace if app.cursor > 0 => {
                let previous = app.draft[..app.cursor]
                    .char_indices()
                    .last()
                    .map_or(0, |(i, _)| i);
                app.draft.drain(previous..app.cursor);
                app.cursor = previous;
            }
            KeyCode::Delete if app.cursor < app.draft.len() => {
                app.draft.remove(app.cursor);
            }
            KeyCode::Left if app.cursor > 0 => {
                app.cursor = app.draft[..app.cursor]
                    .char_indices()
                    .last()
                    .map_or(0, |(i, _)| i);
            }
            KeyCode::Right if app.cursor < app.draft.len() => {
                app.cursor += app.draft[app.cursor..].chars().next().unwrap().len_utf8();
            }
            KeyCode::Home => {
                app.cursor = app.draft[..app.cursor].rfind('\n').map_or(0, |i| i + 1);
            }
            KeyCode::End => {
                app.cursor = app.draft[app.cursor..]
                    .find('\n')
                    .map_or(app.draft.len(), |i| app.cursor + i);
            }
            KeyCode::PageUp => {
                app.scroll = app.scroll.saturating_add(8);
            }
            KeyCode::PageDown => {
                app.scroll = app.scroll.saturating_sub(8);
                if app.scroll == 0 {
                    app.unseen = 0;
                }
            }
            KeyCode::Tab if app.messages.is_empty() && app.draft.is_empty() => {
                app.draft = if app.selection.is_multiple_of(2) {
                    "Inspect the current joint positions."
                } else {
                    "Explain the execution limits."
                }
                .into();
                app.selection += 1;
                app.cursor = app.draft.len();
            }
            _ => {}
        },
        View::Setup => {
            if app.service_busy {
                return;
            }
            match key.code {
                KeyCode::Tab => app.field = (app.field + 1) % 3,
                KeyCode::BackTab => app.field = (app.field + 2) % 3,
                KeyCode::Char('u') if ctrl => pending_field(app).clear(),
                KeyCode::Char('t') if ctrl => {
                    app.service_busy = true;
                    app.notice = "Testing model endpoint · no robot tools available…".into();
                    let settings = app.pending.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(ServiceEvent::Connection(
                            services::test_connection(&settings).await,
                        ));
                    });
                }
                KeyCode::Char('d') if ctrl => {
                    app.service_busy = true;
                    app.notice = "Discovering models · manual model ID remains available.".into();
                    let settings = app.pending.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(ServiceEvent::Models(
                            services::discover_models(&settings).await,
                        ));
                    });
                }
                KeyCode::Char('s') if ctrl => {
                    app.service_busy = true;
                    app.notice = "Saving configuration…".into();
                    let settings = app.pending.clone();
                    let tx = tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = tx.send(ServiceEvent::Saved(services::save_settings(&settings)));
                    });
                }
                KeyCode::Enter => {
                    if app.pending.provider.trim().is_empty() || app.pending.model.trim().is_empty()
                    {
                        app.notice =
                            "Provider and model ID are required. Tab moves between fields.".into();
                    } else if let Err(e) = services::validate_settings(&app.pending) {
                        app.notice = crate::redact(&e);
                    } else {
                        app.settings = app.pending.clone();
                        app.settings.demo = false;
                        app.view = View::Chat;
                        app.notice =
                            "Applied for the next run · not saved to disk. Draft retained.".into();
                    }
                }
                KeyCode::Backspace => {
                    pending_field(app).pop();
                }
                KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                    pending_field(app).push(c);
                }
                _ => {}
            }
        }
        View::Commands | View::Sessions | View::Models => {
            let count = match app.view {
                View::Commands => ACTIONS.iter().filter(|v| v.contains(&app.query)).count(),
                View::Sessions => app
                    .sessions
                    .iter()
                    .filter(|s| s.id.to_lowercase().contains(&app.query.to_lowercase()))
                    .count(),
                _ => app
                    .models
                    .iter()
                    .filter(|s| s.id.to_lowercase().contains(&app.query.to_lowercase()))
                    .count(),
            };
            match key.code {
                KeyCode::Up => app.selection = app.selection.saturating_sub(1),
                KeyCode::Down => app.selection = (app.selection + 1).min(count.saturating_sub(1)),
                KeyCode::Backspace => {
                    app.query.pop();
                    app.selection = 0;
                }
                KeyCode::Char(c) if !ctrl => {
                    app.query.push(c);
                    app.selection = 0;
                }
                KeyCode::Enter => match app.view {
                    View::Commands => {
                        if let Some(name) = ACTIONS
                            .iter()
                            .filter(|v| v.contains(&app.query))
                            .nth(app.selection)
                        {
                            action(app, name, commands, tx);
                        }
                    }
                    View::Sessions if app.selected_session().is_some() => {
                        app.view = View::Preflight;
                    }
                    View::Models => {
                        if let Some(model) = app
                            .models
                            .iter()
                            .filter(|m| m.id.to_lowercase().contains(&app.query.to_lowercase()))
                            .nth(app.selection)
                        {
                            app.pending.model = model.id.clone();
                            app.view = View::Setup;
                            app.query.clear();
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        View::Preflight => {
            if key.code == KeyCode::Enter && !app.busy {
                if app.blocked {
                    app.notice =
                        "Resolve the current session's recorded failure before switching sessions."
                            .into();
                    return;
                }
                if let Some(item) = app.selected_session().cloned() {
                    if item.id == app.session_id {
                        app.view = View::Chat;
                        app.notice =
                            "This conversation is already open; no history replayed.".into();
                        return;
                    }
                    if !item.resumable {
                        app.notice = item.detail;
                        return;
                    }
                    if commands
                        .send(BackendCommand::Resume {
                            id: item.id,
                            settings: app.settings.clone(),
                        })
                        .is_ok()
                    {
                        app.busy = true;
                        app.notice = "Rechecking session under its execution lease…".into();
                    }
                }
            }
        }
        View::Inspect => match key.code {
            KeyCode::Up => {
                app.selection = app.selection.saturating_sub(1);
                app.detail_scroll = 0;
            }
            KeyCode::Down => {
                app.selection = (app.selection + 1).min(app.tools.len().saturating_sub(1));
                app.detail_scroll = 0;
            }
            KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(8),
            KeyCode::PageDown => app.detail_scroll = app.detail_scroll.saturating_add(8),
            _ => {}
        },
        View::Help | View::Update => {}
    }
}

fn apply_backend(app: &mut App, event: BackendEvent) {
    if app.scroll > 0 {
        app.unseen += 1;
        app.scroll = app.scroll.saturating_add(1);
    }
    match event {
        BackendEvent::SessionReady {
            id,
            history,
            resumed,
        } => {
            app.session_id = id;
            app.resumed = resumed;
            if resumed {
                app.messages = history;
                app.tools.clear();
                app.busy = false;
                app.blocked = false;
                app.view = View::Chat;
                app.status = "Conversation restored · current simulator not observed".into();
                app.notice = "History restored, not robot state. No movement or observation has been replayed.".into();
            }
        }
        BackendEvent::Status(status) => app.status = status,
        BackendEvent::Tool(tool) => {
            if let Some(existing) = app.tools.iter_mut().find(|t| t.id == tool.id) {
                *existing = tool;
            } else {
                app.tools.push(tool);
            }
        }
        BackendEvent::Message(message) => app.messages.push(message),
        BackendEvent::Finished {
            summary,
            blocked,
            snapshot_saved,
        } => {
            app.busy = false;
            app.blocked = blocked;
            app.snapshot_saved = snapshot_saved;
            app.status = if blocked {
                "Execution blocked · outcome or persistence unresolved".into()
            } else if snapshot_saved && summary.starts_with("Run completed") {
                "Complete · snapshot saved".into()
            } else {
                summary
            };
        }
        BackendEvent::Error { message, blocked } => {
            app.busy = false;
            app.blocked |= blocked;
            app.notice = message;
            if !app.blocked && app.tools.len() == app.run_start_tool_count && app.draft.is_empty() {
                app.draft = app.last_prompt.clone();
                app.cursor = app.draft.len();
            }
            app.status = if app.blocked {
                "Execution blocked · inspect the recorded outcome"
            } else {
                "Request failed · no new request submitted"
            }
            .into();
        }
    }
}

fn apply_service(app: &mut App, event: ServiceEvent) {
    app.service_busy = false;
    match event {
        ServiceEvent::Models(Ok(models)) => {
            app.models = models;
            app.view = View::Models;
            app.query.clear();
            app.selection = 0;
            app.notice =
                "Model catalog · not a connection test. Capability unknowns remain unknown.".into();
        }
        ServiceEvent::Connection(Ok(message)) => app.notice = message,
        ServiceEvent::Sessions(Ok(mut sessions)) => {
            for item in &mut sessions {
                if item.id == app.session_id {
                    item.status = "active here".into();
                    item.resumable = false;
                    item.detail =
                        "This conversation is already open. Return to chat; no restore is needed."
                            .into();
                }
            }
            app.sessions = sessions;
            app.selection = 0;
        }
        ServiceEvent::Saved(Ok(message)) => {
            app.settings = app.pending.clone();
            app.settings.demo = false;
            app.notice = message;
            app.view = View::Chat;
        }
        ServiceEvent::Models(Err(error))
        | ServiceEvent::Connection(Err(error))
        | ServiceEvent::Sessions(Err(error))
        | ServiceEvent::Saved(Err(error)) => app.notice = crate::redact(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> App {
        App::new(Settings {
            demo: true,
            provider: "openai".into(),
            model: "".into(),
            base_url: "".into(),
            store_path: "/unused".into(),
            config_path: "/unused.json".into(),
        })
    }
    fn channels() -> (
        mpsc::UnboundedSender<BackendCommand>,
        mpsc::UnboundedReceiver<BackendCommand>,
        mpsc::UnboundedSender<ServiceEvent>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (sx, _) = mpsc::unbounded_channel();
        (tx, rx, sx)
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn opening_demo_and_suggestions_never_execute() {
        let mut app = app();
        let (tx, mut rx, sx) = channels();
        handle_key(&mut app, key(KeyCode::Enter), &tx, &sx);
        handle_key(&mut app, key(KeyCode::Tab), &tx, &sx);
        assert_eq!(app.view, View::Chat);
        assert!(!app.draft.is_empty());
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn paste_is_multiline_data_and_never_a_command() {
        let mut app = app();
        app.view = View::Chat;
        paste(&mut app, "hello\r\n/quit\r\n");
        assert_eq!(app.draft, "hello\n/quit\n");
        assert!(!app.quitting);
        assert!(!app.busy);
    }
    #[test]
    fn q_is_text_not_quit_and_unicode_editing_is_safe() {
        let mut app = app();
        app.view = View::Chat;
        let (tx, _, sx) = channels();
        for c in "q猫é".chars() {
            handle_key(&mut app, key(KeyCode::Char(c)), &tx, &sx);
        }
        handle_key(&mut app, key(KeyCode::Left), &tx, &sx);
        handle_key(&mut app, key(KeyCode::Backspace), &tx, &sx);
        assert_eq!(app.draft, "qé");
        assert!(!app.quitting);
        assert_eq!(app.cursor, 1);
    }
    #[test]
    fn settings_cancel_retains_draft_and_configuration() {
        let mut app = app();
        app.view = View::Chat;
        app.draft = "inspect shoulder".into();
        app.cursor = app.draft.len();
        let (tx, _, sx) = channels();
        action(&mut app, "/model", &tx, &sx);
        app.pending.model = "not-applied".into();
        handle_key(&mut app, key(KeyCode::Esc), &tx, &sx);
        assert!(app.settings.model.is_empty());
        assert_eq!(app.draft, "inspect shoulder");
        assert_eq!(app.view, View::Chat);
    }
    #[test]
    fn blocked_or_busy_send_retains_draft() {
        for blocked in [false, true] {
            let mut app = app();
            app.view = View::Chat;
            app.blocked = blocked;
            app.busy = !blocked;
            app.draft = "move shoulder to 0.2".into();
            let (tx, mut rx, sx) = channels();
            submit(&mut app, &tx, &sx);
            assert!(!app.draft.is_empty());
            assert!(rx.try_recv().is_err());
        }
    }
    #[test]
    fn resumed_idle_can_explicitly_request_fresh_observation() {
        let mut app = app();
        app.busy = true;
        apply_backend(
            &mut app,
            BackendEvent::SessionReady {
                id: "saved".into(),
                history: vec![],
                resumed: true,
            },
        );
        assert!(!app.busy);
        assert!(!app.blocked);
        app.draft = "inspect".into();
        let (tx, mut rx, sx) = channels();
        submit(&mut app, &tx, &sx);
        assert!(matches!(rx.try_recv(), Ok(BackendCommand::Submit { .. })));
    }
    #[test]
    fn output_does_not_force_history_to_live_tail() {
        let mut app = app();
        app.scroll = 16;
        apply_backend(&mut app, BackendEvent::Status("tool running".into()));
        assert!(app.scroll >= 16);
        assert_eq!(app.unseen, 1);
    }
}
