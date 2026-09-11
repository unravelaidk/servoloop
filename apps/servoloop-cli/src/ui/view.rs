use super::{
    state::{Phase, Report},
    Screen,
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
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
            },
            "mono" => Self {
                bg: Color::Reset,
                fg: Color::Reset,
                muted: Color::Reset,
                accent: Color::Reset,
                positive: Color::Reset,
                warning: Color::Reset,
                line: Color::Reset,
            },
            _ => Self {
                bg: Color::Rgb(20, 23, 29),
                fg: Color::Rgb(232, 236, 242),
                muted: Color::Rgb(163, 174, 192),
                accent: Color::Rgb(149, 185, 255),
                positive: Color::Rgb(134, 210, 166),
                warning: Color::Rgb(243, 193, 123),
                line: Color::Rgb(63, 73, 88),
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

pub(super) fn draw(frame: &mut Frame, view: &View<'_>) -> usize {
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
    let margin = if area.width < 72 { 1 } else { 3 };
    let content = Rect::new(
        area.x + margin,
        area.y,
        area.width - margin * 2,
        area.height,
    );
    let [header, main, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .areas(content);

    let brand = Line::from(vec![
        Span::styled("servoloop", theme.title()),
        Span::styled(
            if area.width >= 72 {
                "  /  local workspace"
            } else {
                ""
            },
            theme.subdued(),
        ),
        Span::styled("  [SIMULATION ONLY]", theme.accent()),
    ]);
    frame.render_widget(
        Paragraph::new(brand).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(theme.subdued()),
        ),
        header,
    );
    let main = Rect::new(
        main.x,
        main.y + 1,
        main.width,
        main.height.saturating_sub(1),
    );
    let rendered_scroll = match view.screen {
        Screen::Welcome => welcome(frame, main, view),
        Screen::Review => review(frame, main, view),
        Screen::Activity => activity(frame, main, view),
        Screen::Inspect => inspect(frame, main, view),
        Screen::Help => help(frame, main, view),
    };
    let actions = match view.screen {
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
            "i inspect   n new demo   Up/Down scroll   q quit"
        }
        Screen::Activity => "i inspect   Up/Down scroll   q quit",
    };
    let label = match view.screen {
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
                .border_style(theme.text().fg(theme.line)),
        ),
        footer,
    );
    rendered_scroll
}

fn welcome(frame: &mut Frame, area: Rect, view: &View<'_>) -> usize {
    let t = view.theme;
    let choices = ["Try the offline demo", "CLI reference & controls"];
    let mut lines = vec![
        Line::styled("Your first verified movement.", t.title()),
        Line::from(""),
        Line::from("Observe a simulated arm, make one bounded move,"),
        Line::from("then inspect the evidence. No account or network needed."),
        Line::from(""),
    ];
    for (index, choice) in choices.iter().enumerate() {
        lines.push(Line::styled(
            format!(
                "{} {choice}",
                if index == view.selected { ">" } else { " " }
            ),
            if index == view.selected {
                t.accent()
            } else {
                t.subdued()
            },
        ));
        lines.push(Line::from(""));
    }
    lines.push(Line::styled(
        "Fixed script / fresh simulator / local journal",
        t.subdued(),
    ));
    lines.push(Line::styled(
        "No physical robot or Isaac Sim is connected.",
        t.subdued(),
    ));
    paragraph(frame, area, lines, 0, t)
}

fn review(frame: &mut Frame, area: Rect, view: &View<'_>) -> usize {
    let t = view.theme;
    paragraph(
        frame,
        area,
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
            Line::from(""),
            Line::styled("[r] Run this demo", t.accent()),
        ],
        view.scroll,
        t,
    )
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
        paragraph(frame, facts, evidence(view.report, t), 0, t);
        scroll
    } else {
        transcript(frame, area, view)
    }
}

fn transcript(frame: &mut Frame, area: Rect, view: &View<'_>) -> usize {
    let t = view.theme;
    let report = view.report;
    let heading = if report.phase.active() {
        format!("Offline shoulder check / {}s", view.elapsed)
    } else {
        "Offline shoulder check".into()
    };
    let mut lines = vec![Line::styled(heading, t.title()), Line::from("")];
    let summary = match report.verified {
        Some(value) => format!("+ Position verified: {value:.3} rad"),
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
        Line::from("shoulder / 0.200 rad"),
        Line::from(""),
        Line::styled("Initial observation", t.subdued()),
        Line::from(position(report.initial)),
        Line::from(""),
        Line::styled("Verified post-action position", t.subdued()),
        Line::from(position(report.verified)),
        Line::from(""),
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
    let mut lines = evidence(view.report, t);
    lines.extend([
        Line::from(""),
        Line::styled("Session / durable record", t.title()),
        Line::from(
            view.report
                .session
                .clone()
                .unwrap_or_else(|| "not opened".into()),
        ),
        Line::from(format!("Store: {}", view.report.store)),
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
            Line::from("The demo does not accept arbitrary prompts. Provider setup,"),
            Line::from("resume, and updates are not implemented in this UI slice."),
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
