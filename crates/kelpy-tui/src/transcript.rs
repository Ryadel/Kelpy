//! The open conversation laid out as lines: the administrator's messages as bands, Kelpy's answers with each action
//! folded to a line where the model asked for it, and what waits for a decision whole, with the keys that decide it.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Local, TimeZone};

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, status_note};
use crate::model::{Action, ActionState, Message, Role};
use crate::theme::Theme;

/// The open conversation laid out, kept between draws: a message is laid out again only when it or the width
/// changed, an answer being written only from its last line on, and messages far from what shows never are.
#[derive(Default)]
pub struct Transcript {
    selected: Option<i64>,
    width: usize,
    theme: Theme,
    messages: HashMap<i64, Laid>,
    /// How many lines of text were cut to the width.
    #[cfg(test)]
    wrapped: usize,
}

/// A message laid out.
#[derive(Default)]
struct Laid {
    /// Its text, as laid out in `text`.
    content: String,
    text: Vec<Line<'static>>,
    /// For each line of `text`, where in `content` the line it was cut from starts: an action shows after the text
    /// the model wrote before asking for it.
    starts: Vec<usize>,
    /// Where the text's last line starts, and how many lines of `text` it gave: a growing answer is laid out again
    /// from there.
    last_start: usize,
    last_lines: usize,
    /// Each action, as laid out.
    actions: Vec<(Action, Vec<Line<'static>>)>,
}

impl Transcript {
    /// `message` laid out `width` columns wide, as far as it changed since it last was.
    fn laid(&mut self, message: &Message, width: usize) -> &Laid {
        let theme = self.theme;
        let laid = self.messages.entry(message.id).or_default();
        if laid.content != message.content {
            // Only grown, as an answer being written: from its last line on.
            let from = if message.content.starts_with(laid.content.as_str()) {
                let kept = laid.text.len() - laid.last_lines;
                laid.text.truncate(kept);
                laid.starts.truncate(kept);
                laid.content
                    .push_str(&message.content[laid.content.len()..]);
                laid.last_start
            } else {
                laid.text.clear();
                laid.starts.clear();
                laid.content.clone_from(&message.content);
                0
            };
            let mut start = from;
            let mut lines = message.content[from..].split('\n').peekable();
            while let Some(line) = lines.next() {
                let last = lines.peek().is_none();
                // A text ending with a new line has no last line yet.
                let cut = if last && line.is_empty() {
                    Vec::new()
                } else {
                    #[cfg(test)]
                    {
                        self.wrapped += 1;
                    }
                    text_lines(message.role, line, width, theme)
                };
                if last {
                    (laid.last_start, laid.last_lines) = (start, cut.len());
                }
                laid.starts.extend(std::iter::repeat_n(start, cut.len()));
                laid.text.extend(cut);
                start += line.len() + 1;
            }
        }
        laid.actions.truncate(message.actions.len());
        for (index, action) in message.actions.iter().enumerate() {
            match laid.actions.get_mut(index) {
                Some((known, _)) if known == action => {}
                Some(changed) => {
                    *changed = (action.clone(), action_lines(action, width, theme, true));
                }
                None => laid
                    .actions
                    .push((action.clone(), action_lines(action, width, theme, true))),
            }
        }
        laid
    }
}

/// A line of a message's text, `width` columns wide: the administrator's as a band, Kelpy's indented.
pub(crate) fn text_lines(role: Role, line: &str, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let line = line.trim_end_matches('\r');
    match role {
        Role::User => band(line, width, theme),
        Role::Assistant => wrap(line, width.saturating_sub(2))
            .into_iter()
            .map(|text| Line::from(format!("  {text}")).fg(theme.text()))
            .collect(),
    }
}

/// The last row of the administrator's message: when it was written, as at `now`.
pub(crate) fn stamp(
    message: &Message,
    now: &DateTime<Local>,
    width: usize,
    theme: Theme,
) -> Line<'static> {
    let time = when(message.created_at, now);
    band_row(time, Style::new().fg(theme.dim()), width, theme)
}

/// A message's state, when it did not simply complete.
pub(crate) fn note(message: &Message, theme: Theme) -> Option<Line<'static>> {
    status_note(message).map(|note| Line::from(format!("  ({note})")).fg(theme.bad()))
}

/// A line of the administrator's message: on the surface, after Kelpy's bar, as wide as the conversation.
fn band(line: &str, width: usize, theme: Theme) -> Vec<Line<'static>> {
    wrap(line, width.saturating_sub(3))
        .into_iter()
        .map(|text| band_row(text, Style::new().fg(theme.strong()), width, theme))
        .collect()
}

/// A row of the band, `text` in `style`, filled to `width`.
fn band_row(text: String, style: Style, width: usize, theme: Theme) -> Line<'static> {
    let used: usize = text.chars().map(|char| char.width().unwrap_or(0)).sum();
    let fill = " ".repeat(width.saturating_sub(used + 2));
    Line::from(vec![
        Span::styled("┃ ", Style::new().fg(theme.accent())),
        Span::styled(format!("{text}{fill}"), style),
    ])
    .bg(theme.surface())
}

/// When a message was written, `seconds` in Unix time, as at `now`'s place: the time today, else the day too.
fn when<Zone: TimeZone>(seconds: i64, now: &DateTime<Zone>) -> String
where
    Zone::Offset: std::fmt::Display,
{
    let Some(at) = DateTime::from_timestamp(seconds, 0).map(|at| at.with_timezone(&now.timezone()))
    else {
        return String::new();
    };
    let format = if at.date_naive() == now.date_naive() {
        "%H:%M"
    } else if at.year() == now.year() {
        "%b %-d, %H:%M"
    } else {
        "%b %-d %Y, %H:%M"
    };
    at.format(format).to_string()
}

/// The last `needed` lines of the open conversation, `width` columns wide, or all of it if shorter: from the end, so
/// that drawing costs what shows and the scroll, not the whole conversation.
pub fn tail(
    app: &App,
    transcript: &mut Transcript,
    theme: Theme,
    width: usize,
    needed: usize,
) -> Vec<Line<'static>> {
    if (transcript.selected, transcript.width, transcript.theme) != (app.selected, width, theme) {
        transcript.messages.clear();
        (transcript.selected, transcript.width, transcript.theme) = (app.selected, width, theme);
    }
    if app.messages.is_empty() && !app.running() {
        let empty = if app.loading {
            "  Loading…"
        } else {
            "  No messages yet."
        };
        return vec![Line::from(empty).fg(theme.dim())];
    }
    // In reverse, turned the right way at the end.
    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.running() {
        lines.push(Line::from("  Kelpy is answering…").fg(theme.dim()).italic());
    }
    let now = Local::now();
    'messages: for message in app.messages.iter().rev() {
        if message.role == Role::User {
            lines.push(stamp(message, &now, width, theme));
        }
        lines.extend(note(message, theme));
        let laid = transcript.laid(message, width);
        // Each action before the line of text it comes before; the text's lines are walked from the end.
        let mut slots: Vec<(usize, &Vec<Line<'static>>)> = laid
            .actions
            .iter()
            .map(|(action, lines)| {
                (
                    laid.starts
                        .partition_point(|&start| start < action.position),
                    lines,
                )
            })
            .collect();
        slots.sort_by_key(|(slot, _)| *slot);
        for index in (0..=laid.text.len()).rev() {
            let actions = slots
                .iter()
                .rev()
                .filter(|(slot, _)| *slot == index)
                .flat_map(|(_, lines)| lines.iter().rev());
            for line in actions.chain(index.checked_sub(1).map(|index| &laid.text[index])) {
                lines.push(line.clone());
                if lines.len() >= needed {
                    break 'messages;
                }
            }
        }
        // A blank line before each message.
        lines.push(Line::raw(""));
        if lines.len() >= needed {
            break;
        }
    }
    lines.reverse();
    lines
}

/// An action as lines: folded to one when done, as it went; whole while it waits for a decision, with everything the
/// decision rests on, as the Web UI shows it: why, its commands, its change with its notes, and each step of a plan
/// with its detail and why it always asks, then, with `keys`, the keys that decide. Long lines are cut into more,
/// never cut short.
pub(crate) fn action_lines(
    action: &Action,
    width: usize,
    theme: Theme,
    keys: bool,
) -> Vec<Line<'static>> {
    let (mark, colour, state) = match action.state {
        ActionState::Waiting => ("◆", theme.warn(), ""),
        ActionState::Running => ("●", theme.accent(), "  running"),
        ActionState::Done => ("✓", theme.good(), ""),
        ActionState::Failed => ("✗", theme.bad(), "  failed"),
        ActionState::Denied => ("–", theme.dim(), "  denied"),
        ActionState::Uncertain => ("?", theme.bad(), "  outcome uncertain"),
    };
    let dim = Style::new().fg(theme.dim());
    let mut lines = Vec::new();
    if action.state != ActionState::Waiting {
        lines.push(Line::from(vec![
            Span::styled("  ▸ ", dim),
            Span::styled(format!("{mark} "), Style::new().fg(colour)),
            Span::styled(action.summary.clone(), Style::new().fg(theme.text())),
            Span::styled(state, Style::new().fg(colour)),
        ]));
        if let Some(error) = &action.error {
            push(&mut lines, error, 6, width, Style::new().fg(theme.bad()));
        }
        return lines;
    }
    let heading = match &action.plan {
        Some(plan) => {
            let asks = plan
                .steps
                .iter()
                .filter(|step| step.floor.is_some())
                .count();
            let steps = plan.steps.len();
            let mut note = format!(
                "  {} · {steps} step{}",
                plan.goal,
                if steps == 1 { "" } else { "s" }
            );
            if asks > 0 {
                note.push_str(&format!(
                    ", {asks} always ask{}",
                    if asks == 1 { "s" } else { "" }
                ));
            }
            vec![
                Span::styled("Plan", Style::new().fg(theme.strong()).bold()),
                Span::styled(note, dim),
            ]
        }
        None => vec![Span::styled(
            action.summary.clone(),
            Style::new().fg(theme.strong()).bold(),
        )],
    };
    lines.push(Line::from(
        [Span::styled(
            format!("  {mark} "),
            Style::new().fg(colour).bold(),
        )]
        .into_iter()
        .chain(heading)
        .collect::<Vec<_>>(),
    ));
    let floor = Style::new().fg(theme.warn());
    push(
        &mut lines,
        &format!("Why: {}", action.reason),
        4,
        width,
        dim,
    );
    if let Some(command) = &action.command {
        for line in command.lines() {
            push(
                &mut lines,
                &format!("$ {line}"),
                4,
                width,
                Style::new().fg(theme.strong()),
            );
        }
    }
    if let Some(why) = &action.risk {
        push(
            &mut lines,
            &format!("! Asks in Auto too: {why}"),
            4,
            width,
            floor,
        );
    }
    if let Some(change) = &action.change {
        if let Some(why) = &change.floor {
            push(
                &mut lines,
                &format!("! Always asks: {why}"),
                4,
                width,
                floor,
            );
        }
        for note in &change.notes {
            push(
                &mut lines,
                &format!("Note: {}", unquoted(note)),
                4,
                width,
                dim,
            );
        }
        push_detail(&mut lines, &change.diff, 4, width, theme);
    }
    if let Some(plan) = &action.plan {
        for (index, step) in plan.steps.iter().enumerate() {
            lines.push(Line::from(vec![
                Span::styled(format!("    {}  ", index + 1), dim),
                Span::styled(step.summary.clone(), Style::new().fg(theme.text())),
            ]));
            if let Some(detail) = &step.detail {
                push_detail(&mut lines, detail, 7, width, theme);
            }
            if let Some(why) = &step.floor {
                push(
                    &mut lines,
                    &format!("! Always asks: {why}"),
                    7,
                    width,
                    floor,
                );
            }
        }
    }
    if !keys {
        return lines;
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            " Approve ",
            Style::new().fg(theme.on_chip()).bg(theme.accent()).bold(),
        ),
        Span::raw("  "),
        Span::styled(" Deny ", Style::new().fg(theme.text()).bg(theme.chip())),
        Span::styled("   Ctrl+Y approve · Ctrl+N deny", dim),
    ]));
    lines
}

/// A note without the backticks around its code, as in "Checked with `nginx -t`"; an unpaired backtick stays.
pub(crate) fn unquoted(note: &str) -> String {
    if note.matches('`').count().is_multiple_of(2) {
        note.replace('`', "")
    } else {
        note.to_owned()
    }
}

/// `text` after `indent` columns, cut into lines that fit `width`, in `style`.
fn push(lines: &mut Vec<Line<'static>>, text: &str, indent: usize, width: usize, style: Style) {
    let margin = " ".repeat(indent);
    for line in wrap(text, width.saturating_sub(indent)) {
        lines.push(Line::styled(format!("{margin}{line}"), style));
    }
}

/// A change or the commands of a step, line by line, with a diff's colours.
fn push_detail(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    indent: usize,
    width: usize,
    theme: Theme,
) {
    for line in text.lines() {
        let colour = match line.chars().next() {
            Some('+') => theme.good(),
            Some('-') => theme.bad(),
            _ => theme.dim(),
        };
        push(lines, line, indent, width, Style::new().fg(colour));
    }
}

/// One line of the start screen's facts: `Home` and its folder, or a sentence without a label.
pub(crate) struct Fact {
    pub label: Option<String>,
    pub value: String,
    pub style: Style,
    /// A line under the value, in the labels' style, such as how to open a link.
    pub note: Option<String>,
}

/// The facts in a start banner's notes, such as `Code: K7QM-4ZP2 (…) · Home: /root/Kelpy`: each part with a short
/// label (`Home`, `Over SSH`) becomes a fact of its own; a sentence stays whole. Backticks around code are dropped.
pub(crate) fn facts(notes: &[String], style: Style) -> Vec<Fact> {
    let mut facts = Vec::new();
    for note in notes {
        for part in unquoted(note).split(" · ") {
            let labelled = part
                .split_once(": ")
                .filter(|(label, value)| label.chars().count() <= 10 && !value.is_empty());
            facts.push(match labelled {
                Some((label, value)) => Fact {
                    label: Some(label.to_owned()),
                    value: value.to_owned(),
                    style,
                    note: None,
                },
                None => Fact {
                    label: None,
                    value: part.to_owned(),
                    style,
                    note: None,
                },
            });
        }
    }
    facts
}

/// The facts in `width` columns: the labels in a column as wide as the longest, in `label_style`; a value that
/// does not fit goes on under itself, never under its label, and so does its note. A fact without a label takes the whole width.
pub(crate) fn fact_lines(facts: &[Fact], width: usize, label_style: Style) -> Vec<Line<'static>> {
    let longest = facts
        .iter()
        .filter_map(|fact| fact.label.as_ref())
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    // The values keep at least half the width.
    let column = (longest + 2).min(width / 2);
    let mut lines = Vec::new();
    for fact in facts {
        let Some(label) = &fact.label else {
            for line in wrap(&fact.value, width) {
                lines.push(Line::styled(line, fact.style));
            }
            continue;
        };
        for (index, line) in wrap(&fact.value, width.saturating_sub(column))
            .into_iter()
            .enumerate()
        {
            let head = if index == 0 {
                Span::styled(format!("{label:<column$}"), label_style)
            } else {
                Span::raw(" ".repeat(column))
            };
            lines.push(Line::from(vec![head, Span::styled(line, fact.style)]));
        }
        for line in fact
            .note
            .iter()
            .flat_map(|note| wrap(note, width.saturating_sub(column)))
        {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(column)),
                Span::styled(line, label_style),
            ]));
        }
    }
    lines
}

/// `text` cut into lines of at most `width` columns, at spaces where it can.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split_inclusive(' ') {
        let size: usize = word.chars().map(|char| char.width().unwrap_or(0)).sum();
        // The space after a word may fall at the end of the line.
        let fits = used + size - usize::from(word.ends_with(' ')) <= width;
        if !fits && !line.is_empty() {
            lines.push(std::mem::take(&mut line).trim_end().to_owned());
            used = 0;
        }
        if size > width {
            // A word longer than the line is cut.
            for char in word.chars() {
                let char_width = char.width().unwrap_or(0);
                if used + char_width > width {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(char);
                used += char_width;
            }
        } else {
            line.push_str(word);
            used += size;
        }
    }
    lines.push(line.trim_end().to_owned());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_to_approve_shows_every_step_with_its_change_why_it_always_asks_and_the_keys() {
        let plan = crate::app::tests::waiting(
            4,
            serde_json::json!({
                "tool": "propose_plan", "summary": "Plan: harden SSH", "command": null,
                "plan": { "goal": "Harden SSH", "steps": [
                    { "tool": "write_file", "input": "{}", "summary": "Update configuration",
                      "detail": "-%sudo ALL=(ALL) ALL\n+%sudo ALL=(ALL) NOPASSWD: ALL",
                      "floor": "it changes who may act as root (/etc/sudoers)" },
                    { "tool": "run_command", "input": "{}", "summary": "Reload",
                      "detail": "systemctl reload ssh", "floor": null },
                ] },
            }),
        );
        let lines: Vec<String> = action_lines(&plan, 60, Theme::default(), true)
            .iter()
            .map(|line| line.to_string())
            .collect();
        let text = lines.join("\n");
        for expected in [
            "◆ Plan  Harden SSH · 2 steps, 1 always asks",
            "1  Update configuration",
            "! Always asks: it changes who may act as root",
            "(/etc/sudoers)",
            "+%sudo ALL=(ALL) NOPASSWD: ALL",
            "2  Reload",
            "systemctl reload ssh",
            " Approve    Deny    Ctrl+Y approve · Ctrl+N deny",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
        assert!(
            lines.iter().all(|line| line.chars().count() <= 60),
            "{text}"
        );
    }

    #[test]
    fn an_action_done_is_folded_to_one_line_after_the_text_written_before_it() {
        let mut answer = crate::app::tests::message(1, Role::Assistant, "Looking.\nIt is fine.");
        let mut read =
            crate::app::tests::waiting(2, serde_json::json!({ "state": "done", "position": 9 }));
        read.summary = "Read /etc/ssh/sshd_config".into();
        answer.actions.push(read);
        let mut app = open(vec![
            crate::app::tests::message(0, Role::User, "Is SSH safe?"),
            answer,
        ]);
        app.answering.clear();
        let lines: Vec<String> = tail(&app, &mut Transcript::default(), Theme::default(), 40, 100)
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect();
        // The question's band ends with the time it was written.
        assert!(lines[2].starts_with("┃ "), "{lines:?}");
        assert_eq!(
            [&lines[..2], &lines[3..]].concat(),
            [
                "",
                "┃ Is SSH safe?",
                "",
                "  Looking.",
                "  ▸ ✓ Read /etc/ssh/sshd_config",
                "  It is fine."
            ]
        );
    }

    #[test]
    fn an_empty_conversation_says_loading_only_while_it_is_read() {
        let mut app = open(Vec::new());
        let text = |app: &App| {
            tail(app, &mut Transcript::default(), Theme::default(), 40, 10)
                .iter()
                .map(|line| line.to_string().trim().to_owned())
                .collect::<Vec<_>>()
        };
        app.loading = true;
        assert_eq!(text(&app), ["Loading…"]);
        app.loading = false;
        assert_eq!(text(&app), ["No messages yet."]);
        app.answering.insert(1);
        assert_eq!(text(&app), ["Kelpy is answering…"]);
    }

    /// A conversation open with `messages`.
    fn open(messages: Vec<Message>) -> App {
        let mut app = App::new(crate::Banner::default(), false);
        app.selected = Some(1);
        app.messages = messages;
        app
    }

    #[test]
    fn only_the_lines_that_show_are_laid_out() {
        let app = open(
            (0..1000)
                .map(|id| crate::app::tests::message(id, Role::User, &format!("line {id}")))
                .collect(),
        );
        let mut transcript = Transcript::default();
        let lines = tail(&app, &mut transcript, Theme::default(), 40, 6);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[4].to_string().trim_end(), "┃ line 999");
        assert_eq!(transcript.wrapped, 2);
        // Scrolled past the beginning, all of it.
        assert_eq!(
            tail(&app, &mut transcript, Theme::default(), 40, 10_000).len(),
            3000
        );
    }

    #[test]
    fn a_long_answer_is_laid_out_once_and_then_only_where_it_grows() {
        let long = "word ".repeat(100_000);
        let mut answer = crate::app::tests::message(1, Role::Assistant, &format!("{long}\nend"));
        answer.actions.push(crate::app::tests::waiting(
            3,
            serde_json::json!({ "command": "echo x\n".repeat(20_000), "position": long.len() + 4 }),
        ));
        let mut app = open(vec![answer]);
        let mut transcript = Transcript::default();
        let theme = Theme::default();
        assert_eq!(tail(&app, &mut transcript, theme, 40, 20).len(), 20);
        let laid = transcript.wrapped;
        assert_eq!(laid, 2);
        // Drawn again: nothing laid out again, the action included.
        let action = transcript.messages[&1].actions[0].1.len();
        tail(&app, &mut transcript, theme, 40, 20);
        assert_eq!(transcript.wrapped, laid);
        // The answer grows: only its last line is laid out again.
        app.messages[0].content.push_str("ing\nmore");
        let lines = tail(&app, &mut transcript, theme, 40, usize::MAX);
        assert_eq!(transcript.wrapped, laid + 2);
        assert!(lines.iter().any(|line| line.to_string() == "  ending"));
        assert_eq!(transcript.messages[&1].actions[0].1.len(), action);
        // Narrower, all of it again.
        tail(&app, &mut transcript, theme, 30, 20);
        assert_eq!(transcript.wrapped, laid + 2 + 3);
    }

    #[test]
    fn a_message_shows_the_time_it_was_written_and_the_day_if_not_today() {
        use chrono::Utc;
        let now = Utc.with_ymd_and_hms(2026, 11, 8, 15, 0, 0).unwrap();
        let at = |y, m, d, h, min| {
            Utc.with_ymd_and_hms(y, m, d, h, min, 0)
                .unwrap()
                .timestamp()
        };
        assert_eq!(when(at(2026, 11, 8, 10, 58), &now), "10:58");
        assert_eq!(when(at(2026, 11, 3, 9, 5), &now), "Nov 3, 09:05");
        assert_eq!(when(at(2025, 12, 31, 23, 0), &now), "Dec 31 2025, 23:00");
    }

    #[test]
    fn a_note_shows_its_code_without_the_backticks_around_it() {
        assert_eq!(
            unquoted("Checked with `nginx -t` before and after."),
            "Checked with nginx -t before and after."
        );
        assert_eq!(unquoted("A single ` stays."), "A single ` stays.");
    }

    #[test]
    fn each_label_of_the_start_screen_stays_with_its_value_and_a_long_value_goes_on_under_itself() {
        let notes = [
            "Code: K7QM-4ZP2 (to sign in from another browser) · Home: /root/Kelpy".to_owned(),
            "Over SSH: run there `ssh -N -L 4321:127.0.0.1:4321 root@web-01`".to_owned(),
            "Attached to the Kelpy running: closing this terminal UI leaves it running.".to_owned(),
        ];
        let lines: Vec<String> = fact_lines(&facts(&notes, Style::new()), 40, Style::new())
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(
            lines,
            [
                "Code      K7QM-4ZP2 (to sign in from",
                "          another browser)",
                "Home      /root/Kelpy",
                "Over SSH  run there ssh -N -L",
                "          4321:127.0.0.1:4321",
                "          root@web-01",
                "Attached to the Kelpy running: closing",
                "this terminal UI leaves it running.",
            ]
        );
    }

    #[test]
    fn lines_are_cut_at_spaces_and_long_words_where_they_must() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
    }
}
