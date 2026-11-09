//! How the terminal UI looks in full screen: the start screen with Kelpy's mark and the conversations to continue;
//! the open conversation with the side panel beside it where it fits, or over it on Ctrl+B; the message box with the
//! conversation's modes, and the line of keys.

use kelpy_common::unix_now;
use kelpy_core::agent::{Access, Approval};
use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::Capabilities;
use crate::app::{App, Focus, Modes};
use crate::mark::{Kelpie, Mark, Wordmark};
use crate::model::{Action, ContextUse};
use crate::theme::Theme;
use crate::transcript::{Fact, Transcript, fact_lines, facts, tail, unquoted, wrap};

/// From this many columns the side panel shows beside the conversation; below, Ctrl+B opens it over it. As in
/// OpenCode, whose users know it.
pub const WIDE: u16 = 120;
/// The side panel's width.
const PANEL_WIDTH: u16 = 33;
/// The message box grows with its text up to this many lines.
const INPUT_LINES: usize = 6;
/// The start screen's text column is at most this wide.
const TEXT_WIDTH: u16 = 64;
/// Columns between the mark and the text beside it.
const GAP: u16 = 4;
/// A column of air on each side of the conversation and the header.
const SIDES: Margin = Margin::new(1, 0);

pub fn draw(frame: &mut Frame, app: &App, transcript: &mut Transcript, terminal: Capabilities) {
    let area = frame.area();
    if app.starting() {
        start(frame, app, terminal, area);
    } else {
        work(frame, app, transcript, terminal, area);
    }
    if app.focus == Focus::Conversations {
        conversations(frame, app, terminal.theme, area);
    }
}

/// The footer of the terminal UI inline, all it draws: what is under way (`live`), a rule, the message box or the
/// decision waiting, and the status line; over them, the list of conversations.
pub fn draw_inline(frame: &mut Frame, app: &App, terminal: Capabilities, live: Vec<Line<'static>>) {
    let area = frame.area();
    let theme = terminal.theme;
    if app.focus == Focus::Conversations {
        conversations(frame, app, theme, area);
        return;
    }
    let status = Rect {
        y: area.bottom().saturating_sub(1),
        height: 1.min(area.height),
        ..area
    };
    // A connection asking to be let in is decided first, from the status line.
    let waiting = app.waiting().filter(|_| app.connections.is_empty());
    let text = input_lines(app, usize::from(area.width.saturating_sub(5)));
    let height = match waiting {
        Some(_) => approval_height(area.width),
        None => text.len() as u16 + 3,
    }
    .min(status.y.saturating_sub(area.y + 1));
    let footer = Rect {
        y: status.y - height,
        height,
        ..area
    };
    match waiting {
        Some(action) => approval(frame, theme, action, footer),
        None => message_box(frame, app, terminal, footer, text),
    }
    let rule = Rect {
        y: footer.y.saturating_sub(1),
        height: 1,
        ..area
    };
    let line = Line::styled(
        "─".repeat(usize::from(area.width)),
        Style::new().fg(theme.dim()),
    );
    frame.render_widget(Paragraph::new(line), rule);
    // The latest of what is under way, just above the rule.
    let room = usize::from(rule.y - area.y);
    let shown = live.len().min(room);
    let above = Rect::new(area.x, rule.y - shown as u16, area.width, shown as u16);
    frame.render_widget(Paragraph::new(live[live.len() - shown..].to_vec()), above);
    if app.connections.is_empty() && !app.confirming_quit && app.notice.is_none() {
        status_line(frame, app, theme, status);
    } else {
        key_line(frame, app, theme, status);
    }
}

/// The rows the decision waiting takes in the footer, `width` columns wide: its keys on one row from 80 columns,
/// one per row below, without the rows of air under 56.
fn approval_height(width: u16) -> u16 {
    match width {
        80.. => 5,
        56.. => 6,
        _ => 4,
    }
}

/// The decision waiting, in the footer: what it is, and the keys that decide it.
fn approval(frame: &mut Frame, theme: Theme, action: &Action, area: Rect) {
    let buf = frame.buffer_mut();
    buf.set_style(area, Style::new().bg(theme.surface()));
    for y in area.top()..area.bottom() {
        buf[(area.x, y)].set_symbol("┃").set_fg(theme.warn());
    }
    let dim = Style::new().fg(theme.dim());
    let (question, about) = match &action.plan {
        Some(plan) => {
            let asks = plan
                .steps
                .iter()
                .filter(|step| step.floor.is_some())
                .count();
            let mut about = format!("  {} steps", plan.steps.len());
            if asks > 0 {
                about.push_str(&format!(
                    " · {asks} always ask{}",
                    if asks == 1 { "s" } else { "" }
                ));
            }
            ("Approve the plan?".to_owned(), about)
        }
        None => (
            "Approve this action?".to_owned(),
            format!("  {}", action.summary),
        ),
    };
    let title = Line::from(vec![
        Span::styled(question, Style::new().fg(theme.strong()).bold()),
        Span::styled(about, dim),
    ]);
    let key = |key: &str, what: &str, lit: bool| {
        let style = if lit {
            Style::new().fg(theme.on_chip()).bg(theme.accent()).bold()
        } else {
            Style::new().fg(theme.text()).bg(theme.chip())
        };
        [
            Span::styled(format!(" {key} "), style),
            Span::styled(format!(" {what}"), Style::new().fg(theme.text())),
        ]
    };
    let approve = key("Ctrl+Y", "Approve and run", true);
    let deny = key("Ctrl+N", "Deny", false);
    let stop = Span::styled("Esc stop the answer", dim);
    let lines = match area.width {
        80.. => vec![
            Line::raw(""),
            title,
            Line::raw(""),
            Line::from(
                [
                    &approve[..],
                    &[Span::raw("   ")],
                    &deny[..],
                    &[Span::raw("     "), stop],
                ]
                .concat(),
            ),
        ],
        56.. => vec![
            Line::raw(""),
            title,
            Line::raw(""),
            Line::from(approve.to_vec()),
            Line::from(deny.to_vec()),
            Line::from(stop),
        ],
        _ => vec![
            title,
            Line::from(approve.to_vec()),
            Line::from(deny.to_vec()),
            Line::from(stop),
        ],
    };
    let inner = Rect {
        x: area.x + 2,
        width: area.width.saturating_sub(3),
        ..area
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The status line inline: the server, the model, the context used and the modes on the left, what waits or the
/// main keys on the right. Where it does not fit, the model goes first, then the approval.
fn status_line(frame: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let dim = Style::new().fg(theme.dim());
    let modes = app.modes();
    let right = if app.waiting().is_some() {
        Span::styled(
            " 1 decision waits ",
            Style::new()
                .fg(theme.on_chip())
                .bg(theme.warn_chip())
                .bold(),
        )
    } else {
        let quit = if app.stops_runtime {
            "Ctrl+C stop Kelpy"
        } else {
            "Ctrl+C quit"
        };
        Span::styled(format!("Ctrl+R conversations · {quit}"), dim)
    };
    let access = Span::styled(
        access_name(modes.access),
        Style::new().fg(if modes.access == Access::Full {
            theme.warn()
        } else {
            theme.text()
        }),
    );
    let context = app
        .context
        .map(|context| format!("ctx {}%", context.used * 100 / context.window.max(1)));
    // In the order shown, each with whether it goes first where there is no room.
    let parts: Vec<(Span<'static>, bool)> = [
        Some((
            Span::styled(app.banner.server.clone(), Style::new().fg(theme.text())),
            false,
        )),
        Some((Span::styled(app.provider.label.clone(), dim), true)),
        context.map(|context| (Span::styled(context, dim), false)),
        Some((access, false)),
        Some((Span::styled(approval_name(modes.approval), dim), true)),
    ]
    .into_iter()
    .flatten()
    .filter(|(span, _)| !span.content.is_empty())
    .collect();
    let line = |parts: &[(Span<'static>, bool)], dropped: usize| {
        let mut droppable = 0;
        let mut spans = vec![Span::raw(" ")];
        for (span, goes) in parts {
            if *goes {
                droppable += 1;
                if droppable <= dropped {
                    continue;
                }
            }
            if spans.len() > 1 {
                spans.push(Span::styled(" · ", dim));
            }
            spans.push(span.clone());
        }
        Line::from(spans)
    };
    let room = usize::from(area.width).saturating_sub(right.width() + 1);
    let left = (0..=2)
        .map(|dropped| line(&parts, dropped))
        .find(|line| line.width() <= room)
        .unwrap_or_else(|| line(&parts, 2));
    // The keys only where they fit; what waits always.
    let fits = left.width() <= room;
    frame.render_widget(Paragraph::new(left), area);
    if fits || app.waiting().is_some() {
        frame.render_widget(Paragraph::new(Line::from(right)).right_aligned(), area);
    }
}

/// The start screen: the mark beside the wordmark, what runs where, the conversations to continue.
fn start(frame: &mut Frame, app: &App, terminal: Capabilities, area: Rect) {
    let theme = terminal.theme;
    let width = area.width.min(100);
    let footer = Rect {
        x: area.x + (area.width - width) / 2,
        width,
        ..area
    };
    let above = bottom(frame, app, terminal, footer);
    // Left out when a long message leaves it no room.
    let mark = start_mark(theme, area).filter(|mark| mark.size().1 <= above.height);
    let (mark_width, mark_height) = mark.map_or((0, 0), |mark| mark.size());
    let gap = if mark.is_some() { GAP } else { 0 };
    let text_width = TEXT_WIDTH.min(area.width.saturating_sub(4 + mark_width + gap));
    let wordmark = above.height >= Wordmark::SIZE.1 + 4 && text_width >= Wordmark::SIZE.0;
    let lines = start_text(app, theme, text_width.into(), !wordmark);
    let text_height = lines.len() as u16 + if wordmark { Wordmark::SIZE.1 + 1 } else { 0 };
    let height = mark_height.max(text_height).min(above.height);
    let x = area.x + (area.width - (mark_width + gap + text_width)) / 2;
    let y = above.y + (above.height - height) / 2;
    if let Some(mark) = mark {
        let top = y + (height - mark_height) / 2;
        frame.render_widget(mark, Rect::new(x, top, mark_width, mark_height));
    }
    let mut text = Rect::new(
        x + mark_width + gap,
        y + height.saturating_sub(text_height) / 2,
        text_width,
        0,
    );
    text.height = above.bottom().saturating_sub(text.y);
    if wordmark {
        let (ink, cursor) = (theme.strong(), theme.accent());
        frame.render_widget(Wordmark { ink, cursor }, text);
        text.y += Wordmark::SIZE.1 + 1;
        text.height = text.height.saturating_sub(Wordmark::SIZE.1 + 1);
    }
    frame.render_widget(Paragraph::new(lines), text);
}

/// The kelpie beside the wordmark: the largest that fits the terminal's rows with the text beside it. None on a
/// terminal of 16 colours or too small.
fn start_mark(theme: Theme, area: Rect) -> Option<Mark> {
    if !theme.marks() {
        return None;
    }
    [
        (36, Kelpie::Large, 4),
        (28, Kelpie::Medium, 4),
        (20, Kelpie::Small, 2),
    ]
    .into_iter()
    .filter(|&(rows, ..)| area.height >= rows)
    .map(|(_, kelpie, margin)| {
        let mark = Mark::new(kelpie, margin);
        theme.paint(mark, None)
    })
    .find(|mark| mark.size().0 + GAP + 40 + 4 <= area.width)
}

/// The start screen's text: what runs where, the Web UI, the notes, the conversations to continue. With
/// `name`, Kelpy's name first, where the wordmark does not fit.
fn start_text(app: &App, theme: Theme, width: usize, name: bool) -> Vec<Line<'static>> {
    let (dim, text) = (Style::new().fg(theme.dim()), Style::new().fg(theme.text()));
    let banner = &app.banner;
    let mut lines = Vec::new();
    if name {
        lines.push(kelpy_name(theme));
    }
    let mut about = vec![Span::styled(format!("v{}", banner.version), text)];
    for part in [&banner.server, &banner.system] {
        if !part.is_empty() {
            about.push(Span::styled("  ·  ", dim));
            about.push(Span::styled(part.clone(), text));
        }
    }
    let warn = Style::new().fg(theme.warn());
    if !app.provider.label.is_empty() {
        let dot = match &app.provider.problem {
            None => Span::styled(" ●", Style::new().fg(theme.good())),
            Some(_) => Span::styled(" ●", warn),
        };
        let provider = vec![Span::styled(app.provider.label.clone(), text), dot];
        // On the same line where it fits, else on its own.
        let used = Line::from(about.clone()).width() + 5 + Line::from(provider.clone()).width();
        if used <= width {
            about.push(Span::styled("  ·  ", dim));
            about.extend(provider);
        } else {
            lines.push(Line::from(std::mem::take(&mut about)));
            about = provider;
        }
    }
    lines.push(Line::from(about));
    // What the provider still needs, on lines of its own.
    if let Some(problem) = &app.provider.problem {
        lines.extend(
            wrap(&unquoted(problem), width)
                .into_iter()
                .map(|line| Line::styled(line, warn)),
        );
    }
    lines.push(Line::raw(""));
    lines.extend(fact_lines(&start_facts(app, theme), width, dim));
    if !app.sessions.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Continue",
            Style::new().fg(theme.strong()).bold(),
        ));
        let now = unix_now();
        for (index, session) in app.sessions.iter().take(3).enumerate() {
            let note = if app.answering.contains(&session.id) {
                Span::styled("answering", Style::new().fg(theme.accent()))
            } else {
                Span::styled(ago(now - session.updated_at), dim)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{}  ", index + 1), Style::new().fg(theme.accent())),
                Span::styled(fit(&session.title, 24), text),
                Span::raw("  "),
                note,
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("Ctrl+R", Style::new().fg(theme.accent())),
            Span::styled(" all conversations", dim),
        ]));
    }
    lines
}

/// The Web UI's link, then the banner's notes: `Code`, `Home`, `Account`, what Kelpy is reachable by.
pub(crate) fn start_facts(app: &App, theme: Theme) -> Vec<Fact> {
    let mut list = vec![Fact {
        label: Some("Web UI".into()),
        value: app.banner.web_ui.clone(),
        style: Style::new().fg(theme.accent()).underlined(),
    }];
    list.extend(facts(&app.banner.notes, Style::new().fg(theme.text())));
    list
}

/// How long ago, in `seconds`, as a person would say it.
fn ago(seconds: i64) -> String {
    match seconds {
        ..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

/// `text` cut or filled with spaces to `width` columns.
fn fit(text: &str, width: usize) -> String {
    let mut fitted = String::new();
    for char in text.chars() {
        if fitted.width() + char.to_string().width() > width {
            fitted.pop();
            fitted.push('…');
            break;
        }
        fitted.push(char);
    }
    let used = fitted.width();
    fitted + &" ".repeat(width.saturating_sub(used))
}

/// `kelpy_` in text: the name, its cursor in turquoise.
pub(crate) fn kelpy_name(theme: Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("kelpy", Style::new().fg(theme.strong()).bold()),
        Span::styled("_", Style::new().fg(theme.accent()).bold()),
    ])
}

/// An open conversation: its title, the conversation, the message box and the keys; the side panel beside them
/// where it fits, and over them when opened where it does not.
fn work(
    frame: &mut Frame,
    app: &App,
    transcript: &mut Transcript,
    terminal: Capabilities,
    area: Rect,
) {
    let theme = terminal.theme;
    let wide = area.width >= WIDE;
    let beside = wide && !app.panel_hidden;
    let main = if beside {
        Rect {
            width: area.width - PANEL_WIDTH - 1,
            ..area
        }
    } else {
        area
    };
    let above = bottom(frame, app, terminal, main);
    let title = app
        .selected
        .and_then(|id| app.session(id))
        .map_or("New conversation", |session| session.title.as_str());
    let header = Rect { height: 1, ..main };
    if wide {
        let line = Line::from(vec![
            Span::styled("# ", Style::new().fg(theme.dim())),
            Span::styled(title.to_owned(), Style::new().fg(theme.strong()).bold()),
        ]);
        frame.render_widget(Paragraph::new(line), header.inner(SIDES));
    } else {
        // No room for the panel: its main facts go in a header.
        frame
            .buffer_mut()
            .set_style(header, Style::new().bg(theme.panel()));
        let mut left = kelpy_name(theme);
        left.spans.push(Span::raw("  "));
        left.spans.push(Span::styled(
            title.to_owned(),
            Style::new().fg(theme.strong()).bold(),
        ));
        left.spans.push(Span::styled(
            format!("  {}", app.banner.server),
            Style::new().fg(theme.dim()),
        ));
        frame.render_widget(Paragraph::new(left), header.inner(SIDES));
        let mut right = Vec::new();
        if let Some(context) = app.context {
            right.push(Span::styled("ctx ", Style::new().fg(theme.dim())));
            right.extend(context_bar(context, 8, theme));
            right.push(Span::raw("  "));
        }
        right.push(Span::styled(
            app.provider.label.clone(),
            Style::new().fg(theme.dim()),
        ));
        frame.render_widget(
            Paragraph::new(Line::from(right)).right_aligned(),
            header.inner(SIDES),
        );
    }
    let conversation = Rect {
        y: above.y + 2,
        height: above.height.saturating_sub(3),
        ..above.inner(SIDES)
    };
    let width = usize::from(conversation.width);
    let visible = usize::from(conversation.height);
    let lines = tail(app, transcript, theme, width, visible + app.scroll);
    // The end shows, unless scrolled up; never past the first line.
    let end = lines
        .len()
        .saturating_sub(app.scroll.min(lines.len().saturating_sub(visible)));
    let start = end.saturating_sub(visible);
    frame.render_widget(Paragraph::new(lines[start..end].to_vec()), conversation);
    if beside {
        let panel = Rect {
            x: main.right() + 1,
            width: PANEL_WIDTH,
            ..area
        };
        side_panel(frame, app, theme, panel, false);
    } else if !wide && app.overlay {
        frame
            .buffer_mut()
            .set_style(area, Style::new().add_modifier(Modifier::DIM));
        let width = (PANEL_WIDTH + 1).min(area.width);
        let panel = Rect {
            x: area.right() - width,
            width,
            ..area
        };
        side_panel(frame, app, theme, panel, true);
    }
}

/// The side panel: the mark, then the conversation's state.
fn side_panel(frame: &mut Frame, app: &App, theme: Theme, area: Rect, over: bool) {
    frame.render_widget(Clear, area);
    frame
        .buffer_mut()
        .set_style(area, Style::new().bg(theme.panel()));
    let mut y = area.y + 1;
    if theme.marks() {
        let mark = if area.height >= 40 {
            Mark::new(Kelpie::Medium, 1)
        } else {
            Mark::new(Kelpie::Small, 2)
        };
        let mark = theme.paint(mark, theme.panel_under());
        let (width, height) = mark.size();
        let x = area.x + area.width.saturating_sub(width) / 2;
        frame.render_widget(mark, Rect::new(x, area.y, width, height).intersection(area));
        y = area.y + height + 1;
    }
    let (dim, text) = (Style::new().fg(theme.dim()), Style::new().fg(theme.text()));
    let width = usize::from(area.width.saturating_sub(4));
    let session = app.selected.and_then(|id| app.session(id));
    let mut lines = vec![
        Line::styled(
            session
                .map_or("New conversation", |session| session.title.as_str())
                .to_owned(),
            Style::new().fg(theme.strong()).bold(),
        ),
        Line::styled(
            format!("{} · {}", app.banner.server, app.banner.system),
            dim,
        ),
        Line::raw(""),
    ];
    if let Some(context) = app.context {
        lines.push(Line::styled("Context", dim));
        lines.push(Line::from(context_bar(
            context,
            width.saturating_sub(5),
            theme,
        )));
        lines.push(Line::styled(
            format!(
                "{} of {} tokens",
                tokens(context.used),
                tokens(context.window)
            ),
            dim,
        ));
        lines.push(Line::raw(""));
    }
    let modes = app.modes();
    let row = |name: &str, value: Span<'static>| {
        Line::from(vec![Span::styled(format!("{name:<10}"), dim), value])
    };
    lines.push(row("Model", Span::styled(app.provider.label.clone(), text)));
    if let Some(problem) = &app.provider.problem {
        lines.extend(
            wrap(&unquoted(problem), width)
                .into_iter()
                .map(|line| Line::styled(line, Style::new().fg(theme.warn()))),
        );
    }
    let access = Span::styled(
        access_name(modes.access),
        Style::new().fg(if modes.access == Access::Full {
            theme.warn()
        } else {
            theme.text()
        }),
    );
    lines.push(row("Access", access));
    lines.push(row(
        "Approval",
        Span::styled(approval_name(modes.approval), text),
    ));
    lines.push(row(
        "Explain",
        Span::styled(if modes.explanations { "on" } else { "off" }, text),
    ));
    if let Some(action) = app.waiting() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("Waits for you", dim));
        let summary = action
            .plan
            .as_ref()
            .map_or(&action.summary, |plan| &plan.goal);
        for (index, line) in wrap(summary, width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let lead = if index == 0 { "◆ " } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(lead, Style::new().fg(theme.warn())),
                Span::styled(line, text),
            ]));
        }
        lines.push(Line::styled("Ctrl+Y approve · Ctrl+N deny", dim));
    }
    let foot = area.bottom().saturating_sub(2);
    let body = Rect::new(
        area.x + 2,
        y,
        area.width.saturating_sub(4),
        foot.saturating_sub(y + 1),
    );
    frame.render_widget(Paragraph::new(lines), body);
    let foot = Rect::new(area.x + 2, foot, area.width.saturating_sub(4), 1);
    let mut name = kelpy_name(theme);
    name.spans
        .push(Span::styled(format!("  v{}", app.banner.version), dim));
    frame.render_widget(Paragraph::new(name), foot);
    let hint = if over { "Esc closes" } else { "Ctrl+B hides" };
    frame.render_widget(
        Paragraph::new(Line::styled(hint, dim)).right_aligned(),
        foot,
    );
}

/// How full the model's context window is: a bar `width` columns wide and the share used.
fn context_bar(context: ContextUse, width: usize, theme: Theme) -> Vec<Span<'static>> {
    let share = (context.used as f64 / context.window.max(1) as f64).min(1.0);
    // Never empty once something is used.
    let filled = ((share * width as f64).round() as usize)
        .max(usize::from(context.used > 0))
        .min(width);
    vec![
        Span::styled("█".repeat(filled), Style::new().fg(theme.accent())),
        Span::styled("░".repeat(width - filled), Style::new().fg(theme.dim())),
        Span::styled(
            format!(" {:>3.0}%", share * 100.0),
            Style::new().fg(theme.text()),
        ),
    ]
}

/// A number of tokens, short: `850`, `23k`, `1.2M`.
fn tokens(count: u64) -> String {
    match count {
        ..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{}k", (count + 500) / 1_000),
        _ => format!("{:.1}M", count as f64 / 1e6).replace(".0M", "M"),
    }
}

/// The message box and, under it, the line of keys, at the bottom of `area`; returns what is left above them.
fn bottom(frame: &mut Frame, app: &App, terminal: Capabilities, area: Rect) -> Rect {
    let theme = terminal.theme;
    let text = input_lines(app, usize::from(area.width.saturating_sub(5)));
    let height = (text.len() as u16 + 3).min(area.height.saturating_sub(1));
    let keys = Rect {
        y: area.bottom().saturating_sub(1),
        height: 1,
        ..area
    };
    let input = Rect {
        y: keys.y.saturating_sub(height),
        height,
        ..area
    };
    message_box(frame, app, terminal, input, text);
    key_line(frame, app, theme, keys);
    Rect {
        height: input.y - area.y,
        ..area
    }
}

/// The message as typed, cut into the lines of the box `width` columns wide, at most [`INPUT_LINES`]: the last.
fn input_lines(app: &App, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = app
        .input
        .split('\n')
        .flat_map(|line| wrap(line, width.saturating_sub(1)))
        .collect();
    let extra = lines.len().saturating_sub(INPUT_LINES);
    lines.drain(..extra);
    lines
}

/// The message box, as in OpenCode: on the surface, Kelpy's bar on the left; a row of air, the text, a row of air,
/// then the conversation's modes, and on their right what Enter does or what waits.
fn message_box(
    frame: &mut Frame,
    app: &App,
    terminal: Capabilities,
    area: Rect,
    text: Vec<String>,
) {
    if area.is_empty() {
        return;
    }
    let theme = terminal.theme;
    let buf = frame.buffer_mut();
    buf.set_style(area, Style::new().bg(theme.surface()));
    for y in area.top()..area.bottom() {
        buf[(area.x, y)].set_symbol("┃").set_fg(theme.accent());
    }
    let inner = Rect {
        x: area.x + 2,
        width: area.width.saturating_sub(3),
        ..area
    };
    let cursor = Span::styled("█", Style::new().fg(theme.accent()));
    let focused = app.focus == Focus::Message;
    let prompt = Span::styled("› ", Style::new().fg(theme.accent()).bold());
    let mut lines = vec![Line::raw("")];
    if app.input.is_empty() {
        let hint = if app.starting() && !app.sessions.is_empty() {
            "Write, or press a number to continue"
        } else if app.starting() {
            "What do you need to do?"
        } else {
            "Write a message"
        };
        let mut line = vec![prompt];
        if focused {
            line.push(cursor);
        }
        line.push(Span::styled(
            format!(" {hint}"),
            Style::new().fg(theme.placeholder()),
        ));
        lines.push(Line::from(line));
    } else {
        let last = text.len() - 1;
        for (index, line) in text.into_iter().enumerate() {
            let lead = if index == 0 {
                prompt.clone()
            } else {
                Span::raw("  ")
            };
            let mut spans = vec![lead, Span::styled(line, Style::new().fg(theme.strong()))];
            if index == last && focused {
                spans.push(cursor.clone());
            }
            lines.push(Line::from(spans));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
    let chips = Line::from(chips(app.modes(), theme));
    let row = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..inner
    };
    let right = if app.running() {
        "Esc stop the answer".to_owned()
    } else if app.waiting().is_some() {
        "A decision waits".to_owned()
    } else {
        format!("Enter send · {} new line", terminal.newline_key())
    };
    let used = chips.width() + right.width() + 2;
    frame.render_widget(Paragraph::new(chips), row);
    if used <= usize::from(row.width) {
        let right = Line::styled(right, Style::new().fg(theme.dim()));
        frame.render_widget(Paragraph::new(right).right_aligned(), row);
    }
}

/// The conversation's modes as chips: access, lit when full; approval, with the key that changes it; explanations,
/// when on.
fn chips(modes: Modes, theme: Theme) -> Vec<Span<'static>> {
    let plain = Style::new().fg(theme.text()).bg(theme.chip());
    let access = match modes.access {
        Access::Full => Span::styled(
            " Full access ",
            Style::new()
                .fg(theme.on_chip())
                .bg(theme.warn_chip())
                .bold(),
        ),
        Access::ReadOnly => Span::styled(" Read-only ", plain),
        Access::None => Span::styled(" No access ", plain.fg(theme.dim())),
    };
    let mut spans = vec![
        access,
        Span::raw(" "),
        Span::styled(
            format!(" Approval: {} ", approval_name(modes.approval)),
            plain,
        ),
    ];
    if modes.explanations {
        spans.extend([Span::raw(" "), Span::styled(" Explain ", plain)]);
    }
    spans.push(Span::styled("  Shift+Tab", Style::new().fg(theme.dim())));
    spans
}

/// The last row: a connection asking to be let in, a question, a problem, or the keys that matter now, first.
fn key_line(frame: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let line = if let Some(connection) = app.connections.first() {
        Line::styled(
            format!(
                " SSH: {} from {} with {} asks to connect. Allow only if it is you or someone you work with now: \
                 Ctrl+Y allow · Ctrl+N deny",
                connection.user, connection.address, connection.key
            ),
            Style::new()
                .fg(theme.on_chip())
                .bg(theme.warn_chip())
                .bold(),
        )
    } else if app.confirming_quit {
        Line::styled(
            " Stop Kelpy? Answers being written are stopped and kept as interrupted. y: stop · any other key: cancel",
            Style::new().fg(theme.warn()).bold(),
        )
    } else if let Some(notice) = &app.notice {
        Line::styled(format!(" {notice}"), Style::new().fg(theme.bad()))
    } else {
        // The keys that matter now first, then as many others as fit whole, and how to quit.
        let mut keys = Vec::new();
        if app.running() {
            keys.push("Esc stop the answer");
        }
        if app.waiting().is_some() {
            keys.push("Ctrl+Y approve · Ctrl+N deny");
        }
        keys.push("Ctrl+R conversations");
        if !app.starting() {
            keys.push("Ctrl+B panel");
        }
        keys.extend(["F2 access", "F4 explanations"]);
        if !app.starting() {
            keys.push("PgUp/PgDn scroll");
        }
        let quit = if app.stops_runtime {
            "Ctrl+C stop Kelpy"
        } else {
            "Ctrl+C quit"
        };
        let room = usize::from(area.width).saturating_sub(2);
        let mut used = quit.width();
        let mut shown: Vec<&str> = keys
            .into_iter()
            .take_while(|key| {
                used += key.width() + 3;
                used <= room
            })
            .collect();
        shown.push(quit);
        let line = Line::styled(shown.join(" · "), Style::new().fg(theme.dim()));
        if app.starting() {
            line.centered()
        } else {
            Line::from(format!(" {line}")).fg(theme.dim())
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// The conversations, over the screen: Ctrl+R opens them, Enter opens one, Esc goes back.
fn conversations(frame: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let width = area.width.saturating_sub(4).min(64);
    let height = area
        .height
        .saturating_sub(4)
        .min(app.sessions.len() as u16 + 3);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let mut items = vec![ListItem::new(Line::styled(
        "+ New conversation",
        Style::new().fg(theme.accent()),
    ))];
    items.extend(app.sessions.iter().map(|session| {
        let marker = if app.answering.contains(&session.id) {
            "• "
        } else {
            "  "
        };
        ListItem::new(Line::styled(
            format!("{marker}{}", session.title),
            Style::new().fg(theme.text()),
        ))
    }));
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme.accent()))
        .title(" Conversations ")
        .title_bottom(Line::styled(
            " Enter open · n new · Esc back ",
            Style::new().fg(theme.dim()),
        ))
        .style(Style::new().bg(theme.panel()));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        list,
        popup,
        &mut ListState::default().with_selected(Some(app.cursor)),
    );
}

pub fn access_name(access: Access) -> &'static str {
    match access {
        Access::None => "None",
        Access::ReadOnly => "Read-only",
        Access::Full => "Full",
    }
}

pub fn approval_name(approval: Approval) -> &'static str {
    match approval {
        Approval::Ask => "Ask",
        Approval::Plan => "Plan",
        Approval::Auto => "Auto",
    }
}

#[cfg(test)]
pub(crate) mod tests;
