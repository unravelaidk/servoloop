use super::{
    state::{Phase, Report},
    Screen,
};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
    Frame,
};

#[derive(Clone, Copy)]
pub(super) struct Theme {
    pub(super) bg: Color,
    fg: Color,
    muted: Color,
    accent: Color,
    positive: Color,
    warning: Color,
    line: Color,
    selected: Color,
}

impl Theme {
    pub(super) fn named(name: &str) -> Self {
        match name {
            "light" => Self {
                bg: Color::Rgb(248, 249, 251),
                fg: Color::Rgb(24, 32, 44),
                muted: Color::Rgb(78, 87, 101),
                accent: Color::Rgb(29, 71, 158),
                positive: Color::Rgb(24, 105, 72),
                warning: Color::Rgb(133, 65, 11),
                line: Color::Rgb(178, 185, 196),
                selected: Color::Rgb(228, 232, 242),
            },
            "mono" => Self {
                bg: Color::Reset,
                fg: Color::Reset,
                muted: Color::Reset,
                accent: Color::Reset,
                positive: Color::Reset,
                warning: Color::Reset,
                line: Color::Reset,
                selected: Color::Reset,
            },
            _ => Self {
                bg: Color::Rgb(32, 32, 32),
                fg: Color::Rgb(235, 235, 235),
                muted: Color::Rgb(173, 173, 173),
                accent: Color::Rgb(177, 190, 255),
                positive: Color::Rgb(134, 210, 166),
                warning: Color::Rgb(243, 193, 123),
                line: Color::Rgb(69, 69, 69),
                selected: Color::Rgb(43, 43, 43),
            },
        }
    }

    fn text(self) -> Style {
        Style::default().fg(self.fg).bg(self.bg)
    }
    fn subdued(self) -> Style {
        self.text().fg(self.muted)
    }
    fn title(self) -> Style {
        self.text().add_modifier(Modifier::BOLD)
    }
    fn accent(self) -> Style {
        self.text().fg(self.accent).add_modifier(Modifier::BOLD)
    }
}

pub(super) struct View<'a> {
    pub(super) screen: Screen,
    pub(super) selected: usize,
    pub(super) report: &'a Report,
    pub(super) theme: Theme,
    pub(super) scroll: usize,
    pub(super) elapsed: u64,
}

#[cfg(test)]
pub(super) fn draw(frame: &mut Frame, view: &View<'_>) -> usize {
    draw_workspace(frame, view, None)
}

pub(super) fn draw_workspace(
    frame: &mut Frame,
    view: &View<'_>,
    workspace: Option<&super::workspace::Workspace>,
) -> usize {
    let theme = view.theme;
    let area = frame.area();
    frame.render_widget(Block::default().style(theme.text()), area);
    if area.width < 48 || area.height < 18 {
        let text = format!(
            "servoloop / SIMULATION ONLY\n\n{}\n\nEnlarge to at least 48 columns and 18 rows.\n\nCtrl+C cancels an active run.\nq quits after cleanup settles.",
            view.report.phase.label()
        );
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .style(theme.text()),
            area,
        );
        return view.scroll;
    }
    let shell = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.text().fg(theme.line));
    let content = shell.inner(area);
    frame.render_widget(shell, area);
    let compact = area.height < 24;
    let [header, main, footer] = Layout::vertical([
        Constraint::Length(if compact { 2 } else { 3 }),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .areas(content);

    let header_block = Block::default()
        .borders(Borders::BOTTOM)
        .padding(Padding::horizontal(1))
        .border_style(theme.text().fg(theme.line));
    let header_content = header_block.inner(header);
    frame.render_widget(header_block, header);
    let [brand_area, context_area, mode_area] = Layout::horizontal([
        Constraint::Length(19),
        Constraint::Min(0),
        Constraint::Length(19),
    ])
    .areas(header_content);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("[↻] ", theme.accent()),
            Span::styled("servoloop", theme.title()),
        ])),
        brand_area,
    );
    if area.width >= 72 {
        frame.render_widget(
            Paragraph::new("local workspace")
                .alignment(Alignment::Center)
                .style(theme.subdued()),
            context_area,
        );
    }
    frame.render_widget(
        Paragraph::new("● SIMULATION ONLY")
            .alignment(Alignment::Right)
            .style(theme.text().fg(theme.positive)),
        mode_area,
    );
    let margin = if area.width < 72 { 1 } else { 3 };
    let main = Rect::new(
        main.x + margin,
        main.y + u16::from(!compact),
        main.width.saturating_sub(margin * 2),
        main.height.saturating_sub(u16::from(!compact)),
    );
    let rendered_scroll = match view.screen {
        Screen::Welcome => welcome(frame, main, view),
        Screen::Review => review(frame, main, view),
        Screen::Activity => activity(frame, main, view),
        Screen::Inspect => inspect(frame, main, view),
        Screen::Help => help(frame, main, view),
        Screen::Workspace => workspace
            .map(|workspace| workspace_view(frame, main, view, workspace))
            .unwrap_or(0),
    };
    let actions = match view.screen {
        Screen::Workspace => workspace.map(workspace_keys).unwrap_or("Esc back  q quit"),
        Screen::Welcome => "Up/Down choose  Enter open  ? help  q quit",
        Screen::Review => "r run demo   Esc back   ? help   q quit",
        Screen::Help if view.report.phase.active() => {
            "Esc back   Up/Down scroll   Ctrl+C cancel run"
        }
        Screen::Help => "Esc back   Up/Down scroll   q quit",
        Screen::Inspect if view.report.phase.active() => {
            "Esc back   Up/Down scroll   Ctrl+C cancel run"
        }
        Screen::Inspect => "Esc back   Up/Down scroll   q quit",
        Screen::Activity if view.report.phase.active() => {
            "i inspect   Up/Down scroll   Ctrl+C cancel run"
        }
        Screen::Activity if view.report.phase == Phase::Complete => {
            "i inspect  n new demo  c continue  q quit"
        }
        Screen::Activity => "i inspect   Up/Down scroll   q quit",
    };
    let label = match view.screen {
        Screen::Workspace if workspace.is_some_and(|w| !w.status.is_empty()) => {
            workspace.expect("workspace status").status.as_str()
        }
        Screen::Welcome | Screen::Review if !view.report.phase.active() => {
            "Nothing runs until you confirm the demo."
        }
        _ => view.report.phase.label(),
    };
    let status_color = match view.report.phase {
        Phase::Failed | Phase::Interrupted | Phase::Cancelling => theme.warning,
        _ => theme.muted,
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(label, theme.text().fg(status_color)),
            Line::styled(actions, theme.text()),
        ])
        .block(
            Block::default()
                .borders(Borders::TOP)
                .padding(Padding::horizontal(1))
                .border_style(theme.text().fg(theme.line)),
        ),
        footer,
    );
    if let Some(workspace) = workspace.filter(|w| w.picker.is_some()) {
        picker_popup(frame, workspace, theme);
    }
    rendered_scroll
}

fn welcome(frame: &mut Frame, area: Rect, view: &View<'_>) -> usize {
    let t = view.theme;
    // Compact terminals retain all actions, without hiding a clipped menu.
    if area.height < 18 {
        return paragraph(
            frame,
            area,
            vec![
                Line::styled("Start with a simulated arm.", t.title()),
                Line::styled(
                    "No physical hardware is connected.",
                    t.text().fg(t.positive),
                ),
                Line::from(""),
                Line::styled(
                    format!(
                        "{} Try the offline demo",
                        if view.selected == 0 { ">" } else { " " }
                    ),
                    if view.selected == 0 {
                        t.accent()
                    } else {
                        t.text()
                    },
                ),
                Line::styled("  Scripted provider / no network needed", t.subdued()),
                Line::styled(
                    format!(
                        "{} Configure a provider",
                        if view.selected == 1 { ">" } else { " " }
                    ),
                    if view.selected == 1 {
                        t.accent()
                    } else {
                        t.text()
                    },
                ),
                Line::styled(
                    format!(
                        "{} Open a saved session",
                        if view.selected == 2 { ">" } else { " " }
                    ),
                    if view.selected == 2 {
                        t.accent()
                    } else {
                        t.text()
                    },
                ),
                Line::from(""),
                Line::styled(
                    "Enter opens your selection; r confirms a demo.",
                    t.subdued(),
                ),
            ],
            0,
            t,
        );
    }
    let [intro, menu, button, note] = Layout::vertical([
        Constraint::Length(if area.height >= 17 { 6 } else { 5 }),
        Constraint::Length(10),
        Constraint::Length(if area.height >= 20 { 4 } else { 2 }),
        Constraint::Min(1),
    ])
    .areas(area);
    let lines = vec![
        Line::styled("Start with a simulated arm.", t.title()),
        Line::from(""),
        Line::styled(
            if area.width >= 70 {
                "Describe a task, inspect execution, and verify the result."
            } else {
                "Inspect execution and verify the result."
            },
            t.subdued(),
        ),
        Line::from(""),
        Line::styled(
            "No physical hardware is connected.",
            t.text().fg(t.positive),
        ),
    ];
    paragraph(frame, intro, lines, 0, t);
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .split(menu);
    for (index, (title, detail)) in [
        (
            "Try the offline demo",
            "Scripted provider · no account or network needed",
        ),
        ("Configure a provider", "Use an OpenAI-compatible endpoint"),
        (
            "Open a saved session",
            "Restore conversation history, not robot state",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let selected = index == view.selected;
        let bg = if selected { t.selected } else { t.bg };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!("{}  {title}", if selected { "›" } else { " " }),
                    if selected {
                        t.accent().bg(bg)
                    } else {
                        t.text().bg(bg)
                    },
                ),
                Line::styled(format!("   {detail}"), t.subdued().bg(bg)),
            ])
            .style(t.text().bg(bg))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(t.text().fg(t.line))
                    .padding(Padding::horizontal(1)),
            ),
            rows[index],
        );
    }
    primary_action(
        frame,
        button,
        if view.selected == 0 {
            "Review offline demo ↵"
        } else if view.selected == 1 {
            "Configure a provider ↵"
        } else {
            "Open a saved session ↵"
        },
        "↑ ↓ choose · Enter open",
        t,
    );
    paragraph(
        frame,
        note,
        vec![Line::styled(
            if area.width >= 70 {
                "Fixed offline script · no physical robot or Isaac Sim connected."
            } else {
                "Fixed script · no hardware or Isaac Sim."
            },
            t.subdued(),
        )],
        0,
        t,
    );
    0
}

fn review(frame: &mut Frame, area: Rect, view: &View<'_>) -> usize {
    let t = view.theme;
    let area = panel(frame, area, " Offline demo / review before running ", t);
    let [body, action] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if area.height >= 20 { 4 } else { 2 }),
    ])
    .areas(area);
    let scroll = paragraph(
        frame,
        body,
        vec![
            Line::styled("Review the offline demo", t.title()),
            Line::from(""),
            Line::from("1  Observe the shoulder in a fresh simulator."),
            Line::from("2  Request one move to 0.200 rad."),
            Line::from("3  Verify the position and save the session."),
            Line::from(""),
            Line::styled("Target      0.200 rad      Max step  0.250 rad", t.accent()),
            Line::from(""),
            Line::from("This is a fixed script, not a free-form AI conversation."),
            Line::from("Configured providers are not contacted."),
            Line::styled(format!("Local records: {}", view.report.store), t.subdued()),
        ],
        view.scroll,
        t,
    );
    primary_action(
        frame,
        action,
        "[r] Run this demo",
        "Esc back · r confirms",
        t,
    );
    scroll
}

/// One terminal-sized button treatment for primary actions. The last row is
/// spacing, not part of the button; short viewports use a one-line version.
fn primary_action(frame: &mut Frame, area: Rect, label: &str, hint: &str, theme: Theme) {
    let height = if area.height >= 4 { 3 } else { 1 };
    let width = 26.min(area.width);
    let style = if theme.bg == Color::Reset {
        theme
            .text()
            .add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.bg)
            .bg(theme.fg)
            .add_modifier(Modifier::BOLD)
    };
    let button = Rect::new(area.x, area.y, width, height);
    frame.render_widget(Block::default().style(style), button);
    if height == 3 && theme.bg != Color::Reset {
        // Half-cell caps retain a two-cell visual height while placing the
        // label between equal top and bottom padding. Monochrome uses full
        // rows because the user's foreground/background colors are unknown.
        let edge = Style::default().fg(theme.fg).bg(theme.bg);
        for (y, glyph) in [(button.y, "▄"), (button.y + 2, "▀")] {
            frame.render_widget(
                Paragraph::new(glyph.repeat(usize::from(width))).style(edge),
                Rect::new(button.x, y, width, 1),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(style),
        Rect::new(button.x, button.y + height / 2, button.width, 1),
    );
    let hint_width = area.width.saturating_sub(width + 3);
    // The persistent footer carries the same shortcuts when a full hint
    // doesn't fit. Never show a partially clipped keyboard instruction.
    if Line::from(hint).width() <= usize::from(hint_width) {
        frame.render_widget(
            Paragraph::new(hint).style(theme.subdued()),
            Rect::new(area.x + width + 3, area.y + height / 2, hint_width, 1),
        );
    }
}

fn activity(frame: &mut Frame, area: Rect, view: &View<'_>) -> usize {
    let t = view.theme;
    if area.width >= 104 {
        let [timeline, _, facts] = Layout::horizontal([
            Constraint::Percentage(65),
            Constraint::Length(3),
            Constraint::Min(28),
        ])
        .areas(area);
        let scroll = transcript(frame, timeline, view);
        let facts = panel(frame, facts, " Result / evidence ", t);
        paragraph(frame, facts, evidence(view.report, t), 0, t);
        scroll
    } else {
        transcript(frame, area, view)
    }
}

fn transcript(frame: &mut Frame, area: Rect, view: &View<'_>) -> usize {
    let t = view.theme;
    let area = panel(
        frame,
        area,
        if view.report.demo {
            " Offline demo / execution "
        } else {
            " Conversation / execution "
        },
        t,
    );
    let report = view.report;
    let name = if report.demo {
        "Offline shoulder check"
    } else {
        "Provider conversation / simulated arm"
    };
    let heading = if report.phase.active() {
        format!("{name} / {}s", view.elapsed)
    } else {
        name.into()
    };
    let mut lines = vec![Line::styled(heading, t.title()), Line::from("")];
    let summary = match report.verified {
        Some(value) => format!("+ Position verified: {value:.3} rad"),
        None if report.post_observed.is_some() => format!(
            "Post-action shoulder observed: {:.3} rad. Inspect command evidence.",
            report.post_observed.unwrap()
        ),
        None if report.command_started => {
            "! Position not verified. Inspect the recorded outcome.".into()
        }
        None => "No verified position yet.".into(),
    };
    lines.push(Line::styled(
        summary,
        t.text().fg(if report.verified.is_some() {
            t.positive
        } else {
            t.muted
        }),
    ));
    lines.push(Line::styled(
        if report.snapshot_saved {
            "+ Snapshot saved"
        } else {
            "Snapshot not saved"
        },
        t.subdued(),
    ));
    if report.phase == Phase::Cancelling {
        lines.push(Line::styled(
            "! Cleanup pending. Cancellation cannot undo motion.",
            t.text().fg(t.warning),
        ));
    }
    if let Some(error) = &report.error {
        lines.push(Line::styled(format!("! {error}"), t.text().fg(t.warning)));
    }
    lines.push(Line::from(""));
    if !report.answer.is_empty() {
        lines.push(Line::styled(
            "ServoLoop / model response (bounded preview)",
            t.subdued(),
        ));
        lines.push(Line::from(report.answer.clone()));
        lines.push(Line::from(""));
    }
    if !report.activity.is_empty() {
        lines.push(Line::styled(
            "Activity / recorded runtime events",
            t.subdued(),
        ));
    }
    if report.omitted > 0 {
        lines.push(Line::styled(
            format!(
                "{} older display entries omitted; inspect durable records.",
                report.omitted
            ),
            t.subdued(),
        ));
    }
    lines.extend(report.activity.iter().cloned().map(Line::from));
    if !report.phase.active() && report.phase != Phase::Complete {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            if report.session.is_none() {
                "No run started. Check the storage path and permissions."
            } else {
                "No retry here. Inspect the journal before continuing."
            },
            t.text().fg(t.warning),
        ));
    }
    paragraph(frame, area, lines, view.scroll, t)
}

fn evidence(report: &Report, t: Theme) -> Vec<Line<'static>> {
    let position = |value: Option<f64>| {
        value
            .map(|v| format!("{v:.3} rad"))
            .unwrap_or_else(|| "not observed".into())
    };
    vec![
        Line::styled("Execution evidence", t.title()),
        Line::from(""),
        Line::styled("Requested target", t.subdued()),
        Line::from(if report.demo {
            "shoulder / 0.200 rad"
        } else {
            "See submitted request / tool records"
        }),
        Line::from(""),
        Line::styled("Initial observation", t.subdued()),
        Line::from(position(report.initial)),
        Line::styled(
            if report.demo {
                "Verified post-action position"
            } else {
                "Observed post-action position"
            },
            t.subdued(),
        ),
        Line::from(position(if report.demo {
            report.verified
        } else {
            report.post_observed
        })),
        Line::styled("Persistence", t.subdued()),
        Line::from(if report.journal_recorded {
            "Verified result journaled"
        } else {
            "Verified result not confirmed"
        }),
        Line::from(if report.snapshot_saved {
            "Terminal snapshot saved"
        } else {
            "Terminal snapshot not saved"
        }),
    ]
}

fn inspect(frame: &mut Frame, area: Rect, view: &View<'_>) -> usize {
    let t = view.theme;
    let area = panel(frame, area, " Run inspector ", t);
    let mut lines = vec![
        Line::styled("Session / durable record", t.title()),
        Line::from(
            view.report
                .session
                .clone()
                .unwrap_or_else(|| "not opened".into()),
        ),
        Line::from(format!("Store: {}", view.report.store)),
        Line::from(""),
    ];
    lines.extend(evidence(view.report, t));
    lines.extend([
        Line::from(""),
        Line::from("Inspect from another terminal with the same store:"),
        Line::styled(
            "servoloop sessions show SESSION --store DIRECTORY",
            t.accent(),
        ),
        Line::from("Add --snapshot to inspect the terminal conversation."),
        Line::from(""),
        Line::styled("Safety & recovery", t.title()),
        Line::from("Observations above belong to this run, not future robot state."),
        Line::from("Unknown outcomes are not retried. Software cancellation is"),
        Line::from("not a safety-rated stop and does not reverse motion."),
    ]);
    if let Some(error) = &view.report.error {
        lines.push(Line::styled(format!("! {error}"), t.text().fg(t.warning)));
    }
    paragraph(frame, area, lines, view.scroll, t)
}

fn help(frame: &mut Frame, area: Rect, view: &View<'_>) -> usize {
    let t = view.theme;
    let area = panel(frame, area, " CLI reference & controls ", t);
    paragraph(
        frame,
        area,
        vec![
            Line::styled("A terminal workspace, not a second runtime", t.title()),
            Line::from(""),
            Line::from("Enter  Open the selected view. Does not start motion."),
            Line::from("r      Run the fixed demo from its review screen."),
            Line::from("i      Inspect evidence. Esc returns to activity."),
            Line::from("Ctrl+C Request cancellation; wait for cleanup to settle."),
            Line::from("q      Quit. During a run, cancels first; press again later."),
            Line::from("Up/Down, PgUp/PgDn  Scroll. Home returns to the top."),
            Line::from(""),
            Line::styled("Existing CLI commands stay available", t.title()),
            Line::from("servoloop run --demo"),
            Line::from("servoloop run --provider ID --model ID --prompt TEXT"),
            Line::from("servoloop sessions list"),
            Line::from("servoloop config --help"),
            Line::from(""),
            Line::from("The offline demo is fixed. Configure a provider for free-form prompts."),
            Line::from("/ opens commands. m opens model selection from a conversation."),
            Line::from("Pickers: type to search, arrows to choose, Enter select, Esc cancel."),
            Line::from("Conversations: e edits, Enter finishes editing, s sends."),
            Line::from("Saved-session preflight never replays commands. Updates are manual."),
            Line::from("Existing run/resume commands retain their NDJSON output."),
            Line::from(""),
            Line::styled("Appearance", t.title()),
            Line::from("--theme dark | light | mono. NO_COLOR selects monochrome."),
            Line::from("For screen-reader or piped use, prefer the line-oriented CLI."),
        ],
        view.scroll,
        t,
    )
}

fn panel(frame: &mut Frame, area: Rect, title: &'static str, theme: Theme) -> Rect {
    // Borders cost precious rows on small terminals. Keep the same hierarchy
    // there using text rather than reducing the readable viewport further.
    if area.height < 16 {
        return area;
    }
    let block = Block::default()
        .title(Line::styled(title, theme.subdued()))
        .borders(Borders::ALL)
        .border_style(theme.text().fg(theme.line))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn picker_popup(frame: &mut Frame, workspace: &super::workspace::Workspace, theme: Theme) {
    use super::{state::safe_text, workspace::PickerKind};
    let picker = workspace.picker.as_ref().expect("picker is open");
    let screen = frame.area();
    let width = screen.width.saturating_sub(4).min(88);
    let height = screen
        .height
        .saturating_sub(2)
        .min(if picker.kind == PickerKind::Provider {
            15
        } else {
            21
        });
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    let title = if picker.kind == PickerKind::Provider {
        " Choose a provider "
    } else {
        " Choose a model "
    };
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(Line::styled(title, theme.title()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.text().fg(theme.accent))
        .style(theme.text())
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [search, list, details, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(2),
        Constraint::Length(4),
        Constraint::Length(2),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(format!("› {}▏", safe_text(&picker.query)), theme.accent()),
            Line::styled(
                if picker.kind == PickerKind::Provider {
                    "Search catalog providers · unsupported adapters are labeled".into()
                } else {
                    format!(
                        "{} · Models.dev + endpoint discovery",
                        safe_text(&workspace.provider)
                    )
                },
                theme.subdued(),
            ),
        ])
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(theme.text().fg(theme.line)),
        ),
        search,
    );
    let choices = workspace.choices(picker);
    let start = picker
        .selected
        .saturating_sub(usize::from(list.height.saturating_sub(1)));
    for (index, choice) in choices
        .iter()
        .enumerate()
        .skip(start)
        .take(usize::from(list.height))
    {
        let selected = index == picker.selected;
        let label = format!(
            "{} {}",
            if selected { "›" } else { " " },
            safe_text(&choice.title)
        );
        frame.render_widget(
            Paragraph::new(label).style(if selected {
                theme.accent().bg(theme.selected)
            } else {
                theme.text()
            }),
            Rect::new(list.x, list.y + (index - start) as u16, list.width, 1),
        );
    }
    if choices.is_empty() {
        frame.render_widget(
            Paragraph::new(if workspace.busy() {
                if picker.kind == PickerKind::Provider {
                    "Loading provider catalog… Escape cancels."
                } else {
                    "Loading model catalog… You can type a custom ID now."
                }
            } else if picker.kind == PickerKind::Model {
                "No models found. Type an exact ID to add a custom model."
            } else {
                "No matching providers. Change search or press F5 to reload."
            })
            .wrap(Wrap { trim: false })
            .style(theme.subdued()),
            list,
        );
    }
    let mut detail = vec![];
    if let Some(choice) = choices.get(picker.selected) {
        detail.push(Line::styled(safe_text(&choice.detail), theme.subdued()));
    }
    if picker.kind == PickerKind::Model && workspace.busy() {
        detail.push(Line::styled(
            "Loading… Selection does not send a prompt.",
            theme.subdued(),
        ));
    } else if !workspace.status.is_empty() {
        detail.push(Line::styled(safe_text(&workspace.status), theme.subdued()));
    }
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(theme.text().fg(theme.line)),
        ),
        details,
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                if width >= 70 {
                    "↑ ↓ navigate   Enter select   Esc cancel   F5 refresh"
                } else {
                    "↑ ↓ choose  Enter select  Esc cancel"
                },
                theme.text(),
            ),
            Line::styled(
                format!("{} matches · pending until Save", choices.len()),
                theme.subdued(),
            ),
        ]),
        footer,
    );
}

fn workspace_keys(workspace: &super::workspace::Workspace) -> &'static str {
    use super::workspace::Page;
    if workspace.picker.is_some() {
        return "Type to search  ↑ ↓ choose  Enter select  Esc cancel";
    }
    if workspace.editing {
        return "Type to edit  Enter done  Esc done  Ctrl+C cancel";
    }
    match workspace.page {
        Page::Setup => "Tab choose  Enter edit/open  Esc back  q quit",
        Page::Sessions => "e search  r refresh  Enter review  Esc back  q quit",
        Page::Preflight => "Enter resume  Esc sessions  PgDn scroll  q quit",
        Page::Conversation => "e compose  s send  m models  / commands  q quit",
        Page::Palette => "e search  Up/Down choose  Enter open  Esc back",
        Page::Updates => "Esc back  / commands  q quit",
    }
}

fn workspace_view(
    frame: &mut Frame,
    area: Rect,
    view: &View<'_>,
    workspace: &super::workspace::Workspace,
) -> usize {
    use super::{state::safe_text, workspace::Page};
    let t = view.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let title = match workspace.page {
        Page::Setup => "Configure a provider",
        Page::Sessions => "Saved sessions",
        Page::Preflight => "Resume preflight",
        Page::Conversation => {
            if workspace.session.is_some() {
                "Resumed conversation"
            } else {
                "What would you like to test?"
            }
        }
        Page::Palette => "Commands",
        Page::Updates => "Update ServoLoop",
    };
    lines.extend([Line::styled(title, t.title()), Line::from("")]);
    let selected = |index: usize, label: String| {
        Line::styled(
            format!(
                "{} {label}",
                if index == workspace.selected {
                    "›"
                } else {
                    " "
                }
            ),
            if index == workspace.selected {
                t.accent().bg(t.selected)
            } else {
                t.text()
            },
        )
    };
    match workspace.page {
        Page::Setup => {
            lines.push(Line::styled(
                "Pending changes apply to the next run only.",
                t.subdued(),
            ));
            lines.push(Line::from(""));
            let source = workspace.credential_source();
            for (index, (label, value)) in [
                ("Provider  /  Enter to choose", workspace.provider.as_str()),
                (
                    "Endpoint",
                    if workspace.endpoint.is_empty() {
                        "Provider default"
                    } else {
                        &workspace.endpoint
                    },
                ),
                (
                    "Model ID",
                    if workspace.model.is_empty() {
                        "Enter exact model ID"
                    } else {
                        &workspace.model
                    },
                ),
            ]
            .into_iter()
            .enumerate()
            {
                lines.push(Line::styled(label, t.subdued()));
                lines.push(Line::styled(
                    format!(
                        "{} {}{}",
                        if workspace.field == index { "›" } else { " " },
                        safe_text(value),
                        if workspace.editing && workspace.field == index {
                            "▏"
                        } else {
                            ""
                        }
                    ),
                    if workspace.field == index {
                        t.accent().bg(t.selected)
                    } else {
                        t.text()
                    },
                ));
                lines.push(Line::from(""));
            }
            lines.push(Line::styled(
                format!("Credential source: {source} · value hidden"),
                t.subdued(),
            ));
            lines.push(Line::from(""));
            for (index, label) in [
                (3, "Test connection / choose a model"),
                (4, "Save and continue"),
            ] {
                lines.push(Line::styled(
                    format!(
                        "{} {label}",
                        if workspace.field == index { "›" } else { " " }
                    ),
                    if workspace.field == index {
                        t.accent().bg(t.selected)
                    } else {
                        t.text()
                    },
                ));
            }
            lines.push(Line::styled(
                "Save applies settings. Escape discards unapplied edits.",
                t.subdued(),
            ));
        }
        Page::Sessions => {
            lines.push(Line::from(
                "Local history · review a session before resuming.",
            ));
            lines.push(Line::styled(
                format!(
                    "Search: {}{}",
                    safe_text(&workspace.query),
                    if workspace.editing { "▏" } else { "" }
                ),
                t.accent(),
            ));
            lines.push(Line::from(""));
            let sessions = workspace.filtered_sessions();
            if sessions.is_empty() && !workspace.busy() {
                lines.push(Line::from(
                    "No matching saved sessions. An offline demo creates local history.",
                ));
            }
            for (index, id) in sessions
                .into_iter()
                .enumerate()
                .skip(workspace.selected.saturating_sub(4))
                .take(9)
            {
                lines.push(selected(index, safe_text(id)));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled("History is not robot state", t.title()));
            lines.push(Line::from(
                "Opening a saved session does not replay its commands.",
            ));
            lines.push(Line::styled("Review selected session ↵", t.accent()));
        }
        Page::Preflight => {
            lines.push(Line::from(
                "Restore the conversation. Observe the environment again.",
            ));
            lines.push(Line::from(""));
            lines.push(Line::styled("Saved conversation", t.title()));
            lines.push(Line::from(
                workspace
                    .session
                    .clone()
                    .unwrap_or_else(|| "Not cleared for resume".into()),
            ));
            lines.push(Line::from(""));
            lines.push(Line::styled("Fresh simulator", t.title()));
            lines.push(Line::from(
                "Driver: simulated-arm · shoulder now: not observed",
            ));
            lines.push(Line::from(
                "Motion replay: disabled · no tools executed by preflight",
            ));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                if workspace.preflight_ok {
                    "Resume conversation ↵"
                } else {
                    "Resume blocked until checks pass"
                },
                t.accent(),
            ));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Historical messages / not current robot state",
                t.subdued(),
            ));
            lines.extend(workspace.history.iter().cloned().map(Line::from));
        }
        Page::Conversation => {
            lines.push(Line::styled(
                format!(
                    "{} / {} · simulated-arm",
                    safe_text(
                        workspace
                            .config
                            .model
                            .as_deref()
                            .unwrap_or("No model selected")
                    ),
                    safe_text(
                        workspace
                            .config
                            .provider
                            .as_deref()
                            .unwrap_or("No provider selected")
                    )
                ),
                t.subdued(),
            ));
            if workspace.session.is_some() {
                lines.push(Line::from(
                    "The simulator is fresh. Previous movements have not been replayed.",
                ));
            } else {
                lines.push(Line::from("A fresh session. No commands have been sent."));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled("Current execution context", t.title()));
            lines.push(Line::from(
                "Maximum shoulder step: 0.250 rad. Active policy checks motion.",
            ));
            lines.push(Line::from(""));
            lines.push(Line::from("1  Inspect the current joint positions"));
            lines.push(Line::from("2  Move the shoulder to 0.2 radians"));
            lines.push(Line::from("3  Explain the execution limits"));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                format!(
                    "› {}{}",
                    if workspace.draft.is_empty() {
                        "Describe a task…".into()
                    } else {
                        safe_text(&workspace.draft)
                    },
                    if workspace.editing { "▏" } else { "" }
                ),
                t.accent(),
            ));
            lines.push(Line::styled(
                "[s] Send · suggestions only fill the draft",
                t.accent(),
            ));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Earlier conversation / historical",
                t.subdued(),
            ));
            lines.extend(workspace.history.iter().cloned().map(Line::from));
        }
        Page::Palette => {
            lines.push(Line::styled(
                format!(
                    "Search commands: {}{}",
                    safe_text(&workspace.query),
                    if workspace.editing { "▏" } else { "" }
                ),
                t.subdued(),
            ));
            lines.push(Line::from(""));
            for (index, (name, detail)) in workspace.palette().into_iter().enumerate() {
                lines.push(selected(index, name.into()));
                lines.push(Line::styled(format!("  {detail}"), t.subdued()));
                lines.push(Line::from(""));
            }
            lines.push(Line::from(
                "Escape closes the palette and keeps your draft.",
            ));
        }
        Page::Updates => {
            lines.push(Line::from(format!(
                "Installed version: {}",
                env!("CARGO_PKG_VERSION")
            )));
            lines.push(Line::from(""));
            lines.push(Line::styled("Automatic update unavailable", t.title()));
            lines.push(Line::from("Installation provenance has not been verified."));
            lines.push(Line::from(
                "Use the installation method you originally used.",
            ));
            lines.push(Line::from(
                "https://github.com/unravelaidk/servoloop/releases",
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Nothing checked or installed. No sudo, force, or silent updates.",
            ));
        }
    }
    if !workspace.status.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            safe_text(&workspace.status),
            t.text().fg(t.warning),
        ));
    }
    let focus_line: usize = match workspace.page {
        Page::Setup => match workspace.field {
            0 => 5,
            1 => 8,
            2 => 11,
            3 => 16,
            _ => 17,
        },
        _ => 0,
    };
    let scroll = view.scroll.max(
        focus_line
            .saturating_add(2)
            .saturating_sub(usize::from(area.height)),
    );
    paragraph(frame, area, lines, scroll, t)
}

fn paragraph(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'_>>,
    scroll: usize,
    theme: Theme,
) -> usize {
    let paragraph = Paragraph::new(lines)
        .style(theme.text())
        .wrap(Wrap { trim: false });
    let maximum = paragraph
        .line_count(area.width)
        .saturating_sub(area.height as usize);
    let scroll = scroll.min(maximum).min(u16::MAX as usize);
    frame.render_widget(paragraph.scroll((scroll as u16, 0)), area);
    scroll
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn picker_popup_renders_at_supported_sizes_and_all_themes() {
        use super::super::workspace::{Page, Picker, PickerKind, Workspace};
        for (width, height) in [(120, 34), (80, 24), (48, 18)] {
            for theme in ["dark", "light", "mono"] {
                for kind in [PickerKind::Provider, PickerKind::Model] {
                    let mut w = Workspace::new(&crate::config::Config::default());
                    w.page = Page::Setup;
                    w.picker = Some(Picker {
                        kind,
                        query: String::new(),
                        selected: 0,
                    });
                    let report = Report::default();
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| {
                            draw_workspace(
                                frame,
                                &View {
                                    screen: Screen::Workspace,
                                    selected: 0,
                                    report: &report,
                                    theme: Theme::named(theme),
                                    scroll: 0,
                                    elapsed: 0,
                                },
                                Some(&w),
                            );
                        })
                        .unwrap();
                    let text: String = terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .map(|c| c.symbol())
                        .collect();
                    assert!(text.contains(if kind == PickerKind::Provider {
                        "Choose a provider"
                    } else {
                        "Choose a model"
                    }));
                    assert!(text.contains("Enter select"));
                    assert!(text.contains("Esc cancel"));
                }
            }
        }
    }

    #[test]
    fn primary_buttons_share_dimensions_and_center_labels() {
        for height in [2, 4] {
            for label in [
                "Review offline demo ↵",
                "Open CLI reference ↵",
                "[r] Run this demo",
            ] {
                let theme = Theme::named("dark");
                let mut terminal = Terminal::new(TestBackend::new(60, height)).unwrap();
                terminal
                    .draw(|frame| primary_action(frame, frame.area(), label, "Esc back", theme))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let button_height = if height == 4 { 3 } else { 1 };
                for y in 0..button_height {
                    for x in 0..26 {
                        if button_height == 3 && y != 1 {
                            assert_eq!(buffer[(x, y)].symbol(), if y == 0 { "▄" } else { "▀" });
                            assert_eq!(buffer[(x, y)].fg, theme.fg);
                            assert_eq!(buffer[(x, y)].bg, theme.bg);
                        } else {
                            assert_eq!(buffer[(x, y)].bg, theme.fg);
                        }
                    }
                    assert_ne!(buffer[(26, y)].bg, theme.fg);
                }
                let text: String = (0..26)
                    .map(|x| buffer[(x, button_height / 2)].symbol())
                    .collect();
                assert!(text.contains(label));
                assert!(text.starts_with(' ') && text.ends_with(' '));
            }
        }
    }

    #[test]
    fn text_roles_meet_contrast_target_in_both_explicit_palettes() {
        fn luminance(color: Color) -> f64 {
            let Color::Rgb(r, g, b) = color else {
                panic!("expected RGB palette");
            };
            let channel = |n: u8| {
                let s = f64::from(n) / 255.0;
                if s <= 0.04045 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        }
        for name in ["dark", "light"] {
            let theme = Theme::named(name);
            let background = luminance(theme.bg);
            for role in [
                theme.fg,
                theme.muted,
                theme.accent,
                theme.positive,
                theme.warning,
            ] {
                let foreground = luminance(role);
                let ratio =
                    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05);
                assert!(ratio >= 4.5, "{name} {role:?}: {ratio}");
            }
            let background = luminance(theme.selected);
            for role in [theme.fg, theme.muted, theme.accent] {
                let foreground = luminance(role);
                let ratio =
                    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05);
                assert!(ratio >= 4.5, "{name} selected {role:?}: {ratio}");
            }
        }
    }

    #[test]
    fn paper_welcome_retains_selected_row_and_action_at_standard_size() {
        for selected in [0, 1, 2] {
            let theme = Theme::named("dark");
            let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        &View {
                            screen: Screen::Welcome,
                            selected,
                            report: &Report::default(),
                            theme,
                            scroll: 0,
                            elapsed: 0,
                        },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("Start with a simulated arm."));
            assert!(text.contains("Try the offline demo"));
            assert!(text.contains("Configure a provider"));
            assert!(text.contains("Open a saved session"));
            assert!(text.contains(if selected == 0 {
                "Review offline demo"
            } else if selected == 1 {
                "Configure a provider"
            } else {
                "Open a saved session"
            }));
            assert!(buffer.content.iter().any(|cell| cell.bg == theme.selected));
        }
    }

    #[test]
    fn every_view_renders_at_wide_narrow_and_tiny_sizes() {
        for (width, height) in [(120, 34), (80, 24), (60, 24), (48, 18), (30, 10)] {
            for screen in [
                Screen::Welcome,
                Screen::Review,
                Screen::Activity,
                Screen::Inspect,
                Screen::Help,
            ] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let report = Report::default();
                terminal
                    .draw(|f| {
                        draw(
                            f,
                            &View {
                                screen,
                                selected: 0,
                                report: &report,
                                theme: Theme::named("dark"),
                                scroll: 0,
                                elapsed: 0,
                            },
                        );
                    })
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(
                    text.contains("SIMULATION ONLY"),
                    "{screen:?} at {width}x{height}"
                );
                assert!(text.contains("q quit") || text.contains("q quits"));
            }
        }
    }

    #[test]
    fn cancelling_retains_visible_controls_in_inspector() {
        let report = Report {
            phase: Phase::Cancelling,
            ..Report::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(
                    f,
                    &View {
                        screen: Screen::Inspect,
                        selected: 0,
                        report: &report,
                        theme: Theme::named("mono"),
                        scroll: 100,
                        elapsed: 0,
                    },
                );
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Cancelling / awaiting cleanup"));
        assert!(text.contains("Ctrl+C cancel run"));
    }

    #[test]
    fn scrolling_stops_at_content_end_without_accumulating_hidden_offset() {
        let report = Report::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        let mut scroll = 1000;
        terminal
            .draw(|frame| {
                scroll = draw(
                    frame,
                    &View {
                        screen: Screen::Help,
                        selected: 0,
                        report: &report,
                        theme: Theme::named("dark"),
                        scroll,
                        elapsed: 0,
                    },
                );
            })
            .unwrap();
        assert!(scroll > 0 && scroll < 100);
        let end = scroll;
        scroll -= 1;
        terminal
            .draw(|frame| {
                scroll = draw(
                    frame,
                    &View {
                        screen: Screen::Help,
                        selected: 0,
                        report: &report,
                        theme: Theme::named("dark"),
                        scroll,
                        elapsed: 0,
                    },
                );
            })
            .unwrap();
        assert_eq!(scroll, end - 1);
    }
}
