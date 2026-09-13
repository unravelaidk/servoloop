//! Opt-in presentation over the existing CLI execution engine. No provider,
//! storage, or robot operations run merely by opening a view.
mod settings;
mod state;
mod view;
mod workspace;

use crate::{args::value, config::Config, execution};
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use state::{Phase, Report, UiOutput};
use std::{
    env,
    io::{self, IsTerminal, Stdout},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;
use unravel_agent_runtime::StopToken;
use view::{Theme, View};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Welcome,
    Review,
    Activity,
    Inspect,
    Help,
    Workspace,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Start,
    Send,
    Cancel,
    Quit,
}

struct Navigation {
    screen: Screen,
    previous: Screen,
    selected: usize,
    scroll: usize,
    activity_scroll: usize,
}

impl Default for Navigation {
    fn default() -> Self {
        Self {
            screen: Screen::Welcome,
            previous: Screen::Welcome,
            selected: 0,
            scroll: 0,
            activity_scroll: 0,
        }
    }
}

impl Navigation {
    fn key(&mut self, key: KeyEvent, phase: Phase, busy: bool) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        let cancel =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if cancel || key.code == KeyCode::Char('q') {
            return if busy { Action::Cancel } else { Action::Quit };
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Action::None;
        }
        match key.code {
            KeyCode::Char('?') if self.screen != Screen::Help => {
                self.previous = self.screen;
                if self.screen == Screen::Activity {
                    self.activity_scroll = self.scroll;
                }
                self.screen = Screen::Help;
                self.scroll = 0;
            }
            KeyCode::Esc => {
                self.screen = match self.screen {
                    Screen::Review => Screen::Welcome,
                    Screen::Inspect => self.previous,
                    Screen::Help => self.previous,
                    screen => screen,
                };
                self.scroll = if self.screen == Screen::Activity {
                    self.activity_scroll
                } else {
                    0
                };
            }
            KeyCode::Up if self.screen == Screen::Welcome => {
                self.selected = (self.selected + 2) % 3
            }
            KeyCode::Down | KeyCode::Tab if self.screen == Screen::Welcome => {
                self.selected = (self.selected + 1) % 3
            }
            KeyCode::Enter if self.screen == Screen::Welcome => {
                self.screen = if self.selected == 0 {
                    Screen::Review
                } else {
                    Screen::Workspace
                };
                self.previous = Screen::Welcome;
                self.scroll = 0;
            }
            KeyCode::Char('r') if self.screen == Screen::Review && !busy => {
                self.screen = Screen::Activity;
                self.scroll = 0;
                self.activity_scroll = 0;
                return Action::Start;
            }
            KeyCode::Char('i') if self.screen == Screen::Activity => {
                self.previous = Screen::Activity;
                self.activity_scroll = self.scroll;
                self.screen = Screen::Inspect;
                self.scroll = 0;
            }
            KeyCode::Char('n')
                if self.screen == Screen::Activity && phase == Phase::Complete && !busy =>
            {
                self.screen = Screen::Review;
                self.scroll = 0;
            }
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1).min(1000),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10).min(1000),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Home => self.scroll = 0,
            _ => {}
        }
        Action::None
    }
}

struct TerminalSession(Terminal<CrosstermBackend<Stdout>>);

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

impl TerminalSession {
    fn enter() -> Result<Self, String> {
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            restore_terminal();
            original_hook(panic);
        }));
        enable_raw_mode().map_err(|e| format!("terminal: {e}"))?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste) {
            restore_terminal();
            return Err(format!("terminal: {error}"));
        }
        match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => Ok(Self(terminal)),
            Err(error) => {
                restore_terminal();
                Err(format!("terminal: {error}"))
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
    }
}

pub(crate) async fn run(args: &[String], cfg: Config) -> Result<i32, String> {
    if !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || env::var("TERM").as_deref() == Ok("dumb")
    {
        return Err("ui requires an interactive terminal; use `servoloop run --demo` for line-oriented NDJSON output".into());
    }
    let theme_name = if env::var_os("NO_COLOR").is_some() {
        "mono".into()
    } else {
        value(args, "--theme").unwrap_or_else(|| "dark".into())
    };
    let theme = Theme::named(&theme_name);
    let mut terminal = TerminalSession::enter()?;
    let report = Arc::new(Mutex::new(Report {
        store: state::safe_text(&crate::config::store_path(args, Some(&cfg)).to_string_lossy()),
        ..Report::default()
    }));
    let mut stop = StopToken::new();
    let mut task = None;
    let result = interact(
        &mut terminal,
        args,
        &cfg,
        report,
        &mut stop,
        &mut task,
        theme,
    )
    .await;
    // A failed draw/read must not abandon an in-flight run. Keep its lease
    // alive and wait for the shared runner's bounded stop/cleanup path.
    if let Some(task) = task {
        stop.stop();
        let _ = task.await;
    }
    drop(terminal);
    result
}

async fn interact(
    terminal: &mut TerminalSession,
    args: &[String],
    cfg: &Config,
    report: Arc<Mutex<Report>>,
    stop: &mut StopToken,
    task: &mut Option<JoinHandle<Result<i32, String>>>,
    theme: Theme,
) -> Result<i32, String> {
    let mut nav = Navigation::default();
    let mut workspace = workspace::Workspace::new(cfg);
    workspace.settings = Some(settings::Settings::open(crate::config::config_path(args))?);
    let mut started = None;
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    let mut events = EventStream::new();
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        workspace.poll().await;
        if task.as_ref().is_some_and(JoinHandle::is_finished) {
            let result = task
                .take()
                .expect("finished task")
                .await
                .unwrap_or_else(|e| Err(format!("execution task failed: {e}")));
            let mut report = report.lock().map_err(|_| "terminal report lock failed")?;
            report.finish(result);
            if report.phase == Phase::Complete && !report.demo {
                workspace.session = report.session.clone();
                workspace
                    .history
                    .push(format!("ServoLoop: {}", report.answer));
                if workspace.history.len() > 64 {
                    workspace.history.drain(..workspace.history.len() - 64);
                }
            }
        }
        let snapshot = report
            .lock()
            .map_err(|_| "terminal report lock failed")?
            .clone();
        let mut usable = false;
        terminal
            .0
            .draw(|frame| {
                usable = frame.area().width >= 48 && frame.area().height >= 18;
                nav.scroll = view::draw_workspace(
                    frame,
                    &View {
                        screen: nav.screen,
                        selected: nav.selected,
                        report: &snapshot,
                        theme,
                        scroll: nav.scroll,
                        elapsed: started.map(|s: Instant| s.elapsed().as_secs()).unwrap_or(0),
                    },
                    Some(&workspace),
                )
            })
            .map_err(|e| format!("terminal draw: {e}"))?;

        let action = tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) => {
                    let exit_or_cancel = key.code == KeyCode::Char('q') ||
                        (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
                    if (!usable && !exit_or_cancel) || key.kind != KeyEventKind::Press { Action::None }
                    else if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        if workspace.busy() { workspace.cancel_job(); workspace.status = "Operation cancelled. Nothing applied.".into(); Action::None }
                        else { nav.key(key, snapshot.phase, task.is_some()) }
                    }
                    else if nav.screen == Screen::Workspace {
                        if key.code == KeyCode::Char('q') && !workspace.editing && workspace.picker.is_none() && workspace.key_entry.is_none() {
                            nav.key(key, snapshot.phase, task.is_some())
                        } else {
                            match workspace.key(key, args, task.is_some()) {
                                workspace::Intent::Back => { nav.screen = if task.is_some() { Screen::Activity } else { Screen::Welcome }; nav.scroll = 0; Action::None },
                                workspace::Intent::Demo => { nav.screen = Screen::Review; nav.scroll = 0; Action::None },
                                workspace::Intent::Inspect => { nav.previous = Screen::Workspace; nav.screen = Screen::Inspect; nav.scroll = 0; Action::None },
                                workspace::Intent::Help => { nav.previous = Screen::Workspace; nav.screen = Screen::Help; nav.scroll = 0; Action::None },
                                workspace::Intent::Send => Action::Send,
                                workspace::Intent::None => {
                                    if matches!(key.code, KeyCode::PageDown | KeyCode::PageUp | KeyCode::Home) && !workspace.editing && workspace.picker.is_none() && workspace.key_entry.is_none() {
                                        nav.key(key, snapshot.phase, task.is_some());
                                    }
                                    Action::None
                                },
                            }
                        }
                    } else if key.code == KeyCode::Char('/') {
                        workspace.open(workspace::Page::Palette, args); nav.screen = Screen::Workspace; nav.scroll = 0; Action::None
                    } else if key.code == KeyCode::Char('c') && nav.screen == Screen::Activity && snapshot.phase == Phase::Complete {
                        workspace.session = snapshot.session.clone();
                        workspace.page = workspace::Page::Conversation;
                        nav.screen = Screen::Workspace; nav.scroll = 0; Action::None
                    } else {
                        let action = nav.key(key, snapshot.phase, task.is_some());
                        if nav.screen == Screen::Workspace {
                            workspace.open(if nav.selected == 1 { workspace::Page::Setup } else { workspace::Page::Sessions }, args);
                        }
                        action
                    }
                },
                Some(Ok(Event::Paste(text))) if usable && nav.screen == Screen::Workspace => {
                    workspace.paste(&text);
                    Action::None
                },
                // Pasting into non-editable views cannot launch the demo.
                Some(Ok(_)) => Action::None,
                Some(Err(error)) => return Err(format!("terminal input: {error}")),
                None => return Err("terminal input closed".into()),
            },
            _ = interval.tick() => continue,
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|e| format!("terminal signal: {e}"))?;
                if task.is_some() { Action::Cancel } else { Action::Quit }
            }
        };
        match action {
            Action::Send => {
                if matches!(snapshot.phase, Phase::Failed | Phase::Interrupted) {
                    workspace.status = "Inspect the failed/interrupted run before continuing. No retry from this workspace.".into();
                    continue;
                }
                let run_args = match workspace.run_args(args) {
                    Ok(args) => args,
                    Err(error) => {
                        workspace.status = error;
                        continue;
                    }
                };
                if task.is_some() {
                    continue;
                }
                *report.lock().map_err(|_| "terminal report lock failed")? = Report {
                    phase: Phase::Running,
                    store: snapshot.store.clone(),
                    demo: false,
                    ..Report::default()
                };
                *stop = StopToken::new();
                let run_stop = stop.clone();
                let output = Arc::new(UiOutput(report.clone()));
                let cfg = workspace.config.clone();
                let key = workspace.applied_key();
                workspace
                    .history
                    .push(format!("You: {}", state::safe_text(&workspace.draft)));
                workspace.draft.clear();
                started = Some(Instant::now());
                nav.screen = Screen::Activity;
                nav.scroll = 0;
                *task = Some(tokio::spawn(async move {
                    execution::interactive_request(&run_args, &cfg, output, run_stop, key).await
                }));
            }
            Action::Start => {
                *report.lock().map_err(|_| "terminal report lock failed")? = Report {
                    phase: Phase::Running,
                    store: snapshot.store.clone(),
                    ..Report::default()
                };
                *stop = StopToken::new();
                let run_stop = stop.clone();
                let output = Arc::new(UiOutput(report.clone()));
                let args = args.to_vec();
                let cfg = cfg.clone();
                started = Some(Instant::now());
                *task = Some(tokio::spawn(async move {
                    execution::interactive_demo(&args, &cfg, output, run_stop).await
                }));
            }
            Action::Cancel => {
                report
                    .lock()
                    .map_err(|_| "terminal report lock failed")?
                    .cancel();
                stop.stop();
            }
            Action::Quit => return Ok(0),
            Action::None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn opening_and_reviewing_never_starts_motion() {
        let mut nav = Navigation::default();
        assert_eq!(
            nav.key(press(KeyCode::Char('r')), Phase::Ready, false),
            Action::None
        );
        assert_eq!(
            nav.key(press(KeyCode::Enter), Phase::Ready, false),
            Action::None
        );
        assert_eq!(nav.screen, Screen::Review);
        assert_eq!(
            nav.key(press(KeyCode::Enter), Phase::Ready, false),
            Action::None
        );
        assert_eq!(
            nav.key(press(KeyCode::Char('r')), Phase::Ready, false),
            Action::Start
        );
        assert_eq!(
            nav.key(press(KeyCode::Char('r')), Phase::Running, true),
            Action::None
        );
    }

    #[test]
    fn escape_is_not_cancel_and_quit_waits_for_cleanup() {
        let mut nav = Navigation {
            screen: Screen::Activity,
            scroll: 4,
            ..Navigation::default()
        };
        assert_eq!(
            nav.key(press(KeyCode::Char('i')), Phase::Running, true),
            Action::None
        );
        assert_eq!(nav.screen, Screen::Inspect);
        assert_eq!(
            nav.key(press(KeyCode::Esc), Phase::Running, true),
            Action::None
        );
        assert_eq!(nav.scroll, 4);
        assert_eq!(
            nav.key(press(KeyCode::Char('q')), Phase::Running, true),
            Action::Cancel
        );
        assert_eq!(
            nav.key(press(KeyCode::Char('q')), Phase::Cancelling, true),
            Action::Cancel
        );
        assert_eq!(
            nav.key(press(KeyCode::Char('q')), Phase::Interrupted, false),
            Action::Quit
        );
    }

    #[test]
    fn failed_or_interrupted_runs_offer_no_new_motion() {
        let mut nav = Navigation {
            screen: Screen::Activity,
            ..Navigation::default()
        };
        for phase in [Phase::Failed, Phase::Interrupted] {
            assert_eq!(
                nav.key(press(KeyCode::Char('n')), phase, false),
                Action::None
            );
            assert_eq!(nav.screen, Screen::Activity);
        }
    }

    #[tokio::test]
    async fn ui_uses_real_runner_and_persists_verified_evidence() {
        let root = std::env::temp_dir().join(servoloop_store::new_id("ui-test"));
        let args = vec![
            "ui".into(),
            "--store".into(),
            root.to_string_lossy().into_owned(),
        ];
        let report = Arc::new(Mutex::new(Report::default()));
        execution::interactive_demo(
            &args,
            &Config::default(),
            Arc::new(UiOutput(report.clone())),
            StopToken::new(),
        )
        .await
        .unwrap();
        let report = report.lock().unwrap().clone();
        assert_eq!(report.phase, Phase::Complete);
        assert_eq!(report.initial, Some(0.0));
        assert_eq!(report.verified, Some(0.2));
        assert!(report.snapshot_saved && report.journal_recorded);
        let store = servoloop_store::Store::open(&root).unwrap();
        let session = report.session.unwrap();
        assert_eq!(store.records(&session).unwrap().len(), 2);
        assert!(store.load_snapshot(&session).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn storage_setup_failure_never_reports_motion_verification() {
        let path = std::env::temp_dir().join(servoloop_store::new_id("ui-file"));
        std::fs::write(&path, "not a directory").unwrap();
        let args = vec![
            "ui".into(),
            "--store".into(),
            path.to_string_lossy().into_owned(),
        ];
        let report = Arc::new(Mutex::new(Report::default()));
        let result = execution::interactive_demo(
            &args,
            &Config::default(),
            Arc::new(UiOutput(report.clone())),
            StopToken::new(),
        )
        .await;
        assert!(result.is_err());
        assert!(!report.lock().unwrap().command_started);
        assert_eq!(report.lock().unwrap().verified, None);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn pre_cancelled_ui_run_never_dispatches_or_saves_success() {
        let root = std::env::temp_dir().join(servoloop_store::new_id("ui-cancel"));
        let args = vec![
            "ui".into(),
            "--store".into(),
            root.to_string_lossy().into_owned(),
        ];
        let report = Arc::new(Mutex::new(Report::default()));
        let stop = StopToken::new();
        stop.stop();
        let result = execution::interactive_demo(
            &args,
            &Config::default(),
            Arc::new(UiOutput(report.clone())),
            stop,
        )
        .await;
        assert!(result.is_err());
        let report = report.lock().unwrap();
        assert_eq!(report.phase, Phase::Interrupted);
        assert!(!report.command_started);
        assert!(!report.snapshot_saved);
        assert_eq!(report.verified, None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
