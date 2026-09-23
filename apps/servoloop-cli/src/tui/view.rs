//! Cell-native rendering of the paper console. No runtime work happens here.
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::types::{App, Role, View};

const BG: Color = Color::Rgb(20, 20, 20);
const SURFACE: Color = Color::Rgb(32, 32, 32);
const INPUT: Color = Color::Rgb(41, 41, 41);
const TEXT: Color = Color::Rgb(237, 237, 237);
const MUTED: Color = Color::Rgb(176, 176, 176);
const BLUE: Color = Color::Rgb(154, 193, 255);
const GREEN: Color = Color::Rgb(155, 212, 173);
const AMBER: Color = Color::Rgb(237, 202, 130);
const RED: Color = Color::Rgb(244, 155, 155);

fn line(text: impl Into<String>, color: Color) -> Line<'static> {
    Line::from(Span::styled(text.into(), Style::default().fg(color)))
}

fn title(text: &str) -> Line<'static> {
    line(text, TEXT).style(Style::default().add_modifier(Modifier::BOLD))
}

/// Wrap ourselves so tail offsets and cursor rows use the very same cell model.
/// Graphemes (including wide CJK and combining marks) are never split.
fn wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    for source in lines {
        let mut row = Line::default().style(source.style);
        let mut used = 0;
        for span in source.spans {
            for g in span.styled_graphemes(source.style) {
                if g.symbol == "\n" {
                    out.push(row);
                    row = Line::default().style(source.style);
                    used = 0;
                    continue;
                }
                let cells = Span::raw(g.symbol).width();
                if used + cells > width && used > 0 {
                    out.push(row);
                    row = Line::default().style(source.style);
                    used = 0;
                }
                if cells <= width {
                    row.spans.push(Span::styled(g.symbol.to_owned(), g.style));
                    used += cells;
                }
            }
        }
        out.push(row);
    }
    out
}

fn paint(frame: &mut Frame, area: Rect, rows: Vec<Line<'static>>, offset: usize, bg: Color) {
    frame.render_widget(
        Paragraph::new(
            rows.into_iter()
                .skip(offset)
                .take(area.height as usize)
                .collect::<Vec<_>>(),
        )
        .style(Style::default().fg(TEXT).bg(bg)),
        area,
    );
}

fn choice(rows: &mut Vec<Line<'static>>, selected: bool, label: &str, detail: &str) {
    rows.push(
        line(
            format!("{} {label}", if selected { "›" } else { " " }),
            if selected { BLUE } else { TEXT },
        )
        .style(Style::default().bg(if selected { INPUT } else { BG })),
    );
    if !detail.is_empty() {
        rows.push(line(format!("  {detail}"), MUTED));
    }
}

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        area,
    );
    if area.width < 4 || area.height < 4 {
        return;
    }
    let margin = if area.width >= 80 { 3 } else { 1 };
    let inner = Rect::new(
        area.x + margin,
        area.y,
        area.width.saturating_sub(margin * 2),
        area.height,
    );
    let header =
        if area.width < 80 {
            vec![
                title("[↻] servoloop"),
                line("● Simulation only · local workspace", GREEN),
            ]
        } else {
            let left = "[↻] servoloop";
            let right = "local workspace     ● Simulation only";
            vec![Line::from(vec![
                Span::styled(left, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
                Span::raw(" ".repeat(
                    inner.width as usize - Span::raw(left).width() - Span::raw(right).width(),
                )),
                Span::styled(right, Style::default().fg(GREEN)),
            ])]
        };
    let header_height = header.len() as u16;
    paint(
        frame,
        Rect::new(inner.x, inner.y, inner.width, header_height),
        header,
        0,
        BG,
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(INPUT)),
        Rect::new(inner.x, inner.y + header_height, inner.width, 1),
    );

    // Critical state and cancellation remain visible even while reading history.
    let mut footer = Vec::new();
    if app.blocked {
        footer.push(line(
            "Do not repeat movement · inspect the recorded outcome",
            RED,
        ));
    }
    if app.busy {
        footer.push(line("Ctrl+C cancel · cleanup, not rollback", AMBER));
    } else if app.service_busy {
        footer.push(line("Working · waiting for the operation to finish", AMBER));
    }
    let status = if app.blocked {
        format!("Blocked · {}", app.status)
    } else if app.quitting {
        "Closing · waiting for cleanup".into()
    } else {
        app.status.clone()
    };
    let mut status_rows = wrap(
        vec![line(status, if app.blocked { RED } else { MUTED })],
        inner.width,
    );
    status_rows.truncate(2);
    footer.extend(status_rows);
    footer.push(line(
        if app.view == View::Chat {
            "/ commands · Esc back"
        } else {
            "Ctrl+K commands · Esc back"
        },
        MUTED,
    ));
    let footer_height = (footer.len() as u16).min(inner.height.saturating_sub(header_height + 1));
    let footer_y = inner.bottom() - footer_height;
    paint(
        frame,
        Rect::new(inner.x, footer_y, inner.width, footer_height),
        footer,
        0,
        BG,
    );
    let body = Rect::new(
        inner.x,
        inner.y + header_height + 1,
        inner.width,
        footer_y.saturating_sub(inner.y + header_height + 1),
    );
    if app.view == View::Chat {
        chat(frame, body, app);
    } else {
        page(frame, body, app);
    }
}

fn page(frame: &mut Frame, area: Rect, app: &App) {
    let mut rows = Vec::new();
    let mut anchor = None;
    match app.view {
        View::Welcome => {
            rows.push(title("Start with a simulated arm."));
            if area.height > 12 {
                rows.push(line(
                    "Describe a task, inspect execution, verify the result.",
                    MUTED,
                ));
            }
            rows.push(line("No physical hardware is connected.", GREEN));
            if area.height > 12 {
                rows.push(Line::default());
            }
            for (i, (label, detail)) in [
                ("Try offline demo", "Scripted · no account or network"),
                ("Configure a provider", "Use an OpenAI-compatible endpoint"),
                ("Open a saved session", "Restore history, not robot state"),
            ]
            .iter()
            .enumerate()
            {
                choice(&mut rows, app.selection == i, label, detail);
            }
            rows.push(line("↑ ↓ choose · Enter open", BLUE));
        }
        View::Setup => {
            rows.push(title("Configure a provider"));
            rows.push(line("Tab field · Enter apply · Ctrl+S save", BLUE));
            rows.push(line("Ctrl+T test · Ctrl+D discover", BLUE));
            for (i, (label, value)) in [
                ("Provider", &app.pending.provider),
                ("Endpoint", &app.pending.base_url),
                ("Model ID", &app.pending.model),
            ]
            .iter()
            .enumerate()
            {
                choice(
                    &mut rows,
                    app.field == i,
                    &format!(
                        "{label}: {}",
                        if value.is_empty() {
                            "(enter a value)"
                        } else {
                            value
                        }
                    ),
                    "",
                );
            }
            rows.push(line("Pending edits · active run unchanged", MUTED));
            rows.push(line("Credentials: environment · values hidden", MUTED));
            rows.push(line("Tools / images: unknown until evidenced", MUTED));
            rows.push(line("Manual model IDs work without discovery.", MUTED));
        }
        View::Models => {
            rows.push(title("Choose a model"));
            rows.push(line(format!("Filter: {}", app.query), BLUE));
            rows.push(line("Enter selects → setup · ↑ ↓ choose", MUTED));
            let models: Vec<_> = app
                .models
                .iter()
                .filter(|m| m.id.to_lowercase().contains(&app.query.to_lowercase()))
                .collect();
            if models.is_empty() {
                rows.push(line(
                    if app.service_busy {
                        "Discovering models…"
                    } else {
                        "No matching models."
                    },
                    AMBER,
                ));
                rows.push(line("Esc to setup to enter a model ID manually.", MUTED));
            }
            for (i, model) in models.iter().enumerate() {
                if i == app.selection {
                    anchor = Some(wrap(rows.clone(), area.width).len());
                }
                choice(
                    &mut rows,
                    i == app.selection,
                    &model.id,
                    &format!("Tools: {} · images: unknown", model.tools),
                );
            }
            rows.push(line("Unknown does not mean supported.", AMBER));
        }
        View::Sessions => {
            rows.push(title("Saved sessions"));
            rows.push(line(format!("Search: {}", app.query), BLUE));
            rows.push(line("Enter review · ↑ ↓ choose", MUTED));
            rows.push(line("History is not robot state. No motion replay.", AMBER));
            let sessions: Vec<_> = app
                .sessions
                .iter()
                .filter(|s| s.id.to_lowercase().contains(&app.query.to_lowercase()))
                .collect();
            if sessions.is_empty() {
                rows.push(line(
                    if app.service_busy {
                        "Loading local sessions…"
                    } else {
                        "No matching saved sessions."
                    },
                    MUTED,
                ));
                rows.push(line(
                    "Start an offline demo to create local history.",
                    MUTED,
                ));
            }
            for (i, session) in sessions.iter().enumerate() {
                if i == app.selection {
                    anchor = Some(wrap(rows.clone(), area.width).len());
                }
                choice(&mut rows, app.selection == i, &session.id, &session.status);
            }
        }
        View::Preflight => {
            rows.push(title("Resume preflight"));
            rows.push(line("Restore conversation, not robot state.", AMBER));
            rows.push(line("Fresh simulator · current state not observed", MUTED));
            rows.push(line("Motion replay: disabled · no tool execution", GREEN));
            if let Some(session) = app.selected_session() {
                rows.push(title(&session.id));
                rows.push(line(format!("Stored status: {}", session.status), MUTED));
                let resumable = session.resumable && !app.blocked && !app.busy;
                rows.push(line(
                    if resumable {
                        "Enter resume conversation"
                    } else {
                        "Resume unavailable · inspect recorded status"
                    },
                    if resumable { BLUE } else { RED },
                ));
                rows.push(line(&session.detail, TEXT));
            } else {
                rows.push(line("No session selected. Esc to saved sessions.", MUTED));
            }
        }
        View::Inspect => {
            rows.push(title("Execution details"));
            rows.push(line("↑ ↓ select tool · PgUp/PgDn scroll", BLUE));
            rows.push(line(
                format!(
                    "Session: {}",
                    if app.session_id.is_empty() {
                        "not started"
                    } else {
                        &app.session_id
                    }
                ),
                MUTED,
            ));
            rows.push(line(
                if app.snapshot_saved {
                    "Snapshot: saved"
                } else {
                    "Snapshot: not confirmed saved"
                },
                MUTED,
            ));
            if app.tools.is_empty() {
                rows.push(line("No tool events recorded yet.", MUTED));
                rows.push(line(
                    "Arguments and results appear here after tool activity.",
                    MUTED,
                ));
            } else {
                let index = app.selection.min(app.tools.len() - 1);
                let tool = &app.tools[index];
                rows.push(line(
                    format!("Tool {} / {} · {}", index + 1, app.tools.len(), tool.name),
                    BLUE,
                ));
                rows.push(line(format!("ID: {}", tool.id), MUTED));
                rows.push(line(
                    format!("Status: {}", tool.status),
                    if tool.is_error { RED } else { TEXT },
                ));
                rows.push(line("Recorded details", MUTED));
                rows.extend(tool.details.split('\n').map(|s| line(s, TEXT)));
            }
        }
        View::Commands => {
            rows.push(title("Commands"));
            rows.push(line(format!("Search: {}", app.query), BLUE));
            rows.push(line("↑ ↓ choose · Enter open · draft retained", MUTED));
            for (i, (command, detail)) in [
                ("/model", "Provider and model settings"),
                ("/sessions", "Find saved conversations"),
                ("/inspect", "Recorded execution details"),
                ("/new", "Start a fresh conversation"),
                ("/update", "Manual installation guidance"),
                ("/help", "Keys and simulator limitations"),
                ("/quit", "Close ServoLoop"),
            ]
            .iter()
            .filter(|(command, _)| command.contains(&app.query))
            .enumerate()
            {
                if i == app.selection {
                    anchor = Some(wrap(rows.clone(), area.width).len());
                }
                choice(
                    &mut rows,
                    i == app.selection,
                    &format!("{command}  {detail}"),
                    "",
                );
            }
            if rows.len() == 3 {
                rows.push(line("No matching commands. Backspace to edit.", MUTED));
            }
        }
        View::Help => {
            rows.push(title("Keyboard & safety"));
            for text in [
                "Enter              Send / select",
                "Shift+Enter        New line",
                "Ctrl+J             New line fallback",
                "Ctrl+C             Cancel active work",
                "/                  Commands (empty draft)",
                "Esc                Back; retain draft",
                "↑ ↓                Navigate choices/tools",
                "PgUp / PgDn        Scroll output",
                "Tab                Rotate setup fields",
                "Ctrl+T             Test connection",
                "Ctrl+D             Discover models",
                "Ctrl+S             Save configuration",
            ] {
                rows.push(line(text, TEXT));
            }
            rows.push(line(
                "Simulation only. No physical hardware control.",
                GREEN,
            ));
            rows.push(line("Cancellation does not undo motion. Unknown outcomes stay blocked; never replay a movement to repair storage.", AMBER));
        }
        View::Update => {
            rows.push(title("Update ServoLoop"));
            rows.push(line(
                format!("Installed version: {}", env!("CARGO_PKG_VERSION")),
                TEXT,
            ));
            rows.push(line("Automatic update unavailable", AMBER));
            rows.push(line("Available version: unknown · no network check", MUTED));
            rows.push(Line::default());
            rows.push(line(
                "Use the installation method you originally used.",
                TEXT,
            ));
            rows.push(line(
                "Source checkout: review a release, update the checkout, then rebuild with Cargo.",
                MUTED,
            ));
            rows.push(line("cargo build --release -p servoloop-cli", BLUE));
            rows.push(line("Cargo installation: use your original cargo install source and an explicitly chosen version.", MUTED));
            rows.push(line("Verify with servoloop --version afterward.", TEXT));
            rows.push(line(
                "Nothing checked, downloaded, or installed here.",
                GREEN,
            ));
        }
        View::Chat => unreachable!(),
    }
    // Service failures must not disappear below configuration fields on short terminals.
    let notice = if app.notice.is_empty() {
        Vec::new()
    } else {
        wrap(vec![line(&app.notice, AMBER)], area.width)
    };
    let notice_height = (notice.len() as u16).min(3).min(area.height / 3);
    let content = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(notice_height),
    );
    let rows = wrap(rows, area.width);
    let max = rows.len().saturating_sub(content.height as usize);
    let offset = if let Some(anchor) = anchor {
        anchor
            .saturating_sub(content.height.saturating_sub(3) as usize)
            .min(max)
    } else {
        usize::from(app.detail_scroll).min(max)
    };
    paint(frame, content, rows, offset, BG);
    paint(
        frame,
        Rect::new(area.x, content.bottom(), area.width, notice_height),
        notice,
        0,
        BG,
    );
}

fn chat(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let input_width = area.width.saturating_sub(2).max(1);
    let mut draft = wrap(
        app.draft.split('\n').map(|s| line(s, TEXT)).collect(),
        input_width,
    );
    // Cursor is a byte offset. Clamp defensively if asynchronous input is incomplete.
    let mut cursor = app.cursor.min(app.draft.len());
    while !app.draft.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let prefix = wrap(
        app.draft[..cursor]
            .split('\n')
            .map(|s| line(s, TEXT))
            .collect(),
        input_width,
    );
    let mut cursor_row = prefix.len().saturating_sub(1);
    let mut cursor_col = prefix.last().map(Line::width).unwrap_or(0);
    let suffix = Span::raw(&app.draft[cursor..]);
    let next_width = suffix
        .styled_graphemes(Style::default())
        .next()
        .filter(|g| g.symbol != "\n")
        .map_or(0, |g| Span::raw(g.symbol).width());
    if cursor_col >= input_width as usize || cursor_col + next_width > input_width as usize {
        cursor_row += 1;
        cursor_col = 0;
        if draft.len() <= cursor_row {
            draft.push(Line::default());
        }
    }
    let narrow = area.width < 78;
    let hints = if narrow {
        vec![
            line("Enter send · Shift+Enter new line", MUTED),
            line("Ctrl+J fallback · / commands", MUTED),
        ]
    } else {
        vec![line(
            "Enter send · Shift+Enter new line (Ctrl+J fallback) · / commands",
            MUTED,
        )]
    };
    let context = wrap(vec![line(app.settings.model_label(), MUTED)], area.width);
    let context_height = context.len().min(2) as u16;
    let reserved = hints.len() as u16 + if narrow { context_height + 1 } else { 1 };
    let composer_height = (draft.len() as u16)
        .saturating_add(2)
        .max(3)
        .min((area.height / 2).max(3))
        .min(area.height.saturating_sub(reserved));
    let transcript_height = area.height.saturating_sub(composer_height + reserved);
    let transcript = Rect::new(area.x, area.y, area.width, transcript_height);
    let mut rows = Vec::new();
    rows.push(line(
        if app.session_id.is_empty() {
            "New session · simulated-arm".into()
        } else {
            format!("{} · simulated-arm", app.session_id)
        },
        MUTED,
    ));
    rows.push(Line::default());
    if app.resumed {
        rows.push(line("Conversation restored · fresh simulator", AMBER));
        rows.push(line("Previous movements were not replayed.", MUTED));
    }
    if app.messages.is_empty() {
        rows.push(title("What would you like to test?"));
        if transcript.height > 5 {
            rows.push(line("No commands have been sent.", MUTED));
        }
        for (i, suggestion) in ["Inspect joint positions", "Explain execution limits"]
            .iter()
            .enumerate()
        {
            choice(
                &mut rows,
                app.selection % 2 == i && app.draft.is_empty(),
                suggestion,
                "",
            );
        }
        rows.push(line("Tab fills draft · does not run", MUTED));
    } else {
        for message in &app.messages {
            rows.push(line(
                match message.role {
                    Role::User => "› You",
                    Role::Assistant => "✳ ServoLoop",
                    Role::System => "· Session",
                },
                if message.role == Role::User {
                    BLUE
                } else {
                    MUTED
                },
            ));
            rows.extend(message.text.split('\n').map(|s| line(s, TEXT)));
            rows.push(Line::default());
        }
    }
    for tool in &app.tools {
        let recorded = tool.status.starts_with("Verified") || tool.status == "Observed";
        rows.push(line(
            format!(
                "{} {} · {}",
                if tool.is_error {
                    "×"
                } else if recorded {
                    "✓"
                } else {
                    "◌"
                },
                tool.name,
                tool.status
            ),
            if tool.is_error {
                RED
            } else if recorded {
                GREEN
            } else {
                AMBER
            },
        ));
    }
    if !app.tools.is_empty() {
        rows.push(line("/inspect for full arguments and results", BLUE));
    }
    if !app.notice.is_empty() {
        rows.push(line(&app.notice, AMBER));
    }
    let rows = wrap(rows, transcript.width);
    let badge_height = u16::from(app.scroll > 0 || app.unseen > 0).min(transcript.height);
    let history = Rect::new(
        transcript.x,
        transcript.y,
        transcript.width,
        transcript.height.saturating_sub(badge_height),
    );
    let max = rows.len().saturating_sub(history.height as usize);
    let offset = max.saturating_sub(usize::from(app.scroll));
    paint(frame, history, rows, offset, SURFACE);
    if badge_height > 0 {
        paint(
            frame,
            Rect::new(area.x, history.bottom(), area.width, 1),
            vec![line(format!("↓ {} new · PgDn to latest", app.unseen), BLUE)],
            0,
            SURFACE,
        );
    }
    let composer = Rect::new(area.x, transcript.bottom(), area.width, composer_height);
    let border = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.blocked { RED } else { BLUE }))
        .style(Style::default().bg(INPUT));
    let inside = border.inner(composer);
    frame.render_widget(border, composer);
    let offset = cursor_row.saturating_sub(inside.height.saturating_sub(1) as usize);
    if app.draft.is_empty() {
        draft = vec![line("Describe a task…", MUTED)];
    }
    paint(frame, inside, draft, offset, INPUT);
    if !app.busy && !app.blocked && inside.width > 0 && inside.height > 0 {
        frame.set_cursor_position((
            inside.x + (cursor_col as u16).min(inside.width - 1),
            inside.y + (cursor_row - offset) as u16,
        ));
    }
    let send = if app.blocked {
        "Send unavailable · execution blocked"
    } else if app.busy {
        "Running · Ctrl+C cancel"
    } else if app.service_busy {
        "Operation pending"
    } else if app.draft.trim().is_empty() {
        "Send unavailable · enter a request"
    } else {
        "Send ↵"
    };
    let send_color = if app.blocked {
        RED
    } else if app.draft.trim().is_empty() && !app.busy {
        MUTED
    } else {
        BLUE
    };
    let mut bottom = if narrow {
        let mut lines = context
            .into_iter()
            .take(context_height as usize)
            .collect::<Vec<_>>();
        lines.push(line(send, send_color));
        lines
    } else {
        let label = app.settings.model_label();
        let available = (area.width as usize).saturating_sub(Span::raw(send).width() + 2);
        let label: String = label.chars().take(available).collect();
        let gap = (area.width as usize)
            .saturating_sub(Span::raw(&label).width() + Span::raw(send).width());
        vec![Line::from(vec![
            Span::styled(label, Style::default().fg(MUTED)),
            Span::raw(" ".repeat(gap)),
            Span::styled(send, Style::default().fg(send_color)),
        ])]
    };
    bottom.extend(hints);
    paint(
        frame,
        Rect::new(area.x, composer.bottom(), area.width, reserved),
        bottom,
        0,
        BG,
    );
}

#[cfg(test)]
mod tests {
    use super::super::types::{ChatMessage, ModelItem, SessionItem, Settings, ToolEntry};
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn app() -> App {
        App::new(Settings {
            demo: true,
            provider: "openai-compatible".into(),
            model: "custom-model".into(),
            base_url: "http://localhost:8000/v1".into(),
            store_path: "sessions".into(),
            config_path: "config.json".into(),
        })
    }

    fn screen(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_view_renders_at_supported_sizes() {
        for (width, height) in [(120, 40), (80, 24), (50, 24), (40, 16)] {
            for view in [
                View::Welcome,
                View::Chat,
                View::Setup,
                View::Models,
                View::Sessions,
                View::Preflight,
                View::Inspect,
                View::Commands,
                View::Help,
                View::Update,
            ] {
                let mut app = app();
                app.view = view;
                let text = screen(&app, width, height);
                assert!(text.contains("[↻] servoloop"), "{view:?} {width}x{height}");
                assert!(text.contains("Simulation only"));
                assert!(text.contains(if view == View::Chat {
                    "/ commands · Esc back"
                } else {
                    "Ctrl+K commands · Esc back"
                }));
                let heading = match view {
                    View::Welcome => "Start with a simulated arm.",
                    View::Chat => "What would you like to test?",
                    View::Setup => "Configure a provider",
                    View::Models => "Choose a model",
                    View::Sessions => "Saved sessions",
                    View::Preflight => "Resume preflight",
                    View::Inspect => "Execution details",
                    View::Commands => "Commands",
                    View::Help => "Keyboard & safety",
                    View::Update => "Update ServoLoop",
                };
                assert!(
                    text.contains(heading),
                    "missing {heading}: {width}x{height}"
                );
                app.busy = true;
                assert!(screen(&app, width, height).contains("Ctrl+C cancel"));
            }
        }
    }

    #[test]
    fn actual_messages_tools_and_inspection_are_rendered() {
        let mut app = app();
        app.view = View::Chat;
        app.messages.push(ChatMessage {
            role: Role::User,
            text: "Observe my elbow".into(),
        });
        app.tools.push(ToolEntry {
            id: "event-17".into(),
            name: "robot.observe".into(),
            status: "recorded".into(),
            details: "actual measured elbow: 0.125\nnot a requested target".into(),
            is_error: false,
        });
        let text = screen(&app, 80, 24);
        assert!(text.contains("You"));
        assert!(text.contains("Observe my elbow"));
        assert!(text.contains("robot.observe · recorded"));
        app.view = View::Inspect;
        let text = screen(&app, 80, 24);
        assert!(text.contains("event-17"));
        assert!(text.contains("actual measured elbow: 0.125"));
        assert!(text.contains("not a requested target"));
    }

    #[test]
    fn filtering_and_preflight_use_shared_selection() {
        let mut app = app();
        app.sessions = vec![
            SessionItem {
                id: "other".into(),
                status: "old".into(),
                resumable: true,
                detail: "".into(),
            },
            SessionItem {
                id: "MATCH-42".into(),
                status: "unknown".into(),
                resumable: false,
                detail: "Unresolved intent".into(),
            },
        ];
        app.query = "match".into();
        app.view = View::Preflight;
        let text = screen(&app, 80, 24);
        assert!(text.contains("MATCH-42"));
        assert!(text.contains("Resume unavailable"));
        assert!(!text.contains("Enter resume"));
        app.models = vec![
            ModelItem {
                id: "match-model".into(),
                tools: "unknown".into(),
            },
            ModelItem {
                id: "hidden-model".into(),
                tools: "unknown".into(),
            },
        ];
        app.view = View::Models;
        let text = screen(&app, 80, 24);
        assert!(text.contains("match-model"));
        assert!(!text.contains("hidden-model"));
        assert!(text.contains("images: unknown"));
    }

    #[test]
    fn update_is_manual_and_blocked_state_stays_visible() {
        let mut app = app();
        app.view = View::Update;
        let text = screen(&app, 120, 40);
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        assert!(text.contains("no network check"));
        assert!(text.contains("cargo build --release -p servoloop-cli"));
        app.view = View::Chat;
        app.blocked = true;
        app.status = "Unresolved intent".into();
        assert!(screen(&app, 40, 16).contains("Blocked · Unresolved intent"));
    }

    #[test]
    fn narrow_choices_and_service_errors_remain_reachable() {
        let mut app = app();
        let text = screen(&app, 40, 16);
        for label in [
            "Try offline demo",
            "Configure a provider",
            "Open a saved session",
        ] {
            assert!(text.contains(label));
        }
        app.view = View::Chat;
        let text = screen(&app, 40, 16);
        assert!(text.contains("Inspect joint positions"));
        assert!(text.contains("Tab fills draft"));
        app.view = View::Commands;
        app.selection = 6;
        assert!(screen(&app, 40, 16).contains("/quit"));
        app.query = "inspect".into();
        app.selection = 0;
        let text = screen(&app, 40, 16);
        assert!(text.contains("/inspect"));
        assert!(!text.contains("/model"));
        app.view = View::Setup;
        app.notice = "Authentication rejected".into();
        assert!(screen(&app, 40, 16).contains("Authentication rejected"));
    }

    #[test]
    fn unicode_wrap_and_tail_scrolling_are_cell_based() {
        let rows = wrap(vec![line("肩e\u{301}abc", TEXT)], 4);
        assert_eq!(rows[0].width(), 4);
        assert_eq!(rows[1].width(), 2);
        let mut app = app();
        app.view = View::Chat;
        app.draft = "肩e\u{301}\nnext".into();
        app.cursor = "肩e\u{301}".len();
        let mut terminal = Terminal::new(TestBackend::new(50, 24)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        // Narrow left margin + composer border + 2-cell CJK + combining e.
        assert_eq!(terminal.get_cursor_position().unwrap().x, 5);
        app.messages.push(ChatMessage {
            role: Role::Assistant,
            text: (0..50)
                .map(|i| format!("history-{i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        assert!(screen(&app, 80, 24).contains("history-49"));
        app.scroll = 20;
        app.unseen = 3;
        let text = screen(&app, 80, 24);
        assert!(!text.contains("history-49"));
        assert!(text.contains("3 new"));
    }
}
