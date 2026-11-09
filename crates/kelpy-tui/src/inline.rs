//! The terminal UI inline, as over SSH: what is said goes once into the terminal's own history, where it scrolls and
//! copies as the terminal's own text, and only a footer is drawn again. The history only grows, so a line goes there
//! once it cannot change: a line of an answer once the next one starts, an action once it ended, a plan as it waits.
//! Until then it shows in the footer, above the message box.

use std::collections::{HashMap, HashSet};

use chrono::Local;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::App;
use crate::mark::{Kelpie, Mark, Wordmark};
use crate::model::{ActionState, Message, Role, Status};
use crate::theme::Theme;
use crate::transcript::{action_lines, note, stamp, text_lines, wrap};
use crate::ui::kelpy_name;

/// What of the open conversation was written into the terminal's history.
#[derive(Default)]
pub struct Scrollback {
    /// Whether the start banner was written.
    banner: bool,
    /// The conversation written, and what of each of its messages.
    conversation: Option<i64>,
    printed: HashMap<i64, Printed>,
}

/// What of a message was written.
#[derive(Default)]
struct Printed {
    /// Whether anything was, after the blank line that comes before it.
    started: bool,
    /// The bytes of its text written: up to where a line starts, or all of it.
    text: usize,
    /// Its actions written as they ended.
    ended: HashSet<i64>,
    /// Its actions written whole as they waited for a decision.
    shown: HashSet<i64>,
    /// Whether it was written to its end.
    done: bool,
}

impl Scrollback {
    /// What is new to write into the history, drawn `width` columns wide; none if nothing is.
    pub fn take(&mut self, app: &App, width: u16, theme: Theme) -> Option<Buffer> {
        let mut blocks = Vec::new();
        // After the conversations are read, to list them; at the first message, if they cannot be.
        if !self.banner && (app.loaded || !app.messages.is_empty()) {
            self.banner = true;
            blocks.push(banner(app, width, theme));
        }
        if !self.banner {
            return None;
        }
        let mut lines = Vec::new();
        if app.selected != self.conversation {
            self.conversation = app.selected;
            self.printed.clear();
            if let Some(id) = app.selected {
                let title = app
                    .session(id)
                    .map_or("New conversation", |session| session.title.as_str());
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled("── ", Style::new().fg(theme.dim())),
                    Span::styled(title.to_owned(), Style::new().fg(theme.strong()).bold()),
                ]));
            }
        }
        let (width_used, now) = (usize::from(width), Local::now());
        for message in &app.messages {
            let printed = self.printed.entry(message.id).or_default();
            if printed.done {
                continue;
            }
            if !printed.started {
                printed.started = true;
                lines.push(Line::raw(""));
            }
            if !print(message, printed, &mut lines, width_used, theme) {
                break;
            }
            if message.role == Role::User {
                lines.push(stamp(message, &now, width_used, theme));
            }
        }
        if !lines.is_empty() {
            blocks.push(paragraph(lines, width));
        }
        stack(blocks, width)
    }

    /// What is under way, not yet in the history: the line of the answer being written, the action running.
    pub fn live(&self, app: &App, width: u16, theme: Theme) -> Vec<Line<'static>> {
        let width = usize::from(width);
        let mut lines = Vec::new();
        if let Some(message) = app.messages.iter().find(|message| {
            self.printed
                .get(&message.id)
                .is_none_or(|printed| !printed.done)
        }) {
            let written = self
                .printed
                .get(&message.id)
                .map_or(0, |printed| printed.text);
            let rest = message.content.get(written..).unwrap_or_default();
            for line in rest.split('\n').filter(|line| !line.is_empty()) {
                lines.extend(text_lines(message.role, line, width, theme));
            }
            for action in &message.actions {
                if action.state == ActionState::Running {
                    lines.extend(action_lines(action, width, theme, false));
                }
            }
        }
        if app.running() && lines.is_empty() {
            lines.push(Line::from("  Kelpy is answering…").fg(theme.dim()).italic());
        }
        lines
    }
}

/// Writes into `lines` what of `message` cannot change any more and was not written yet, as `printed` records.
/// Returns whether all of it is written: the messages after it wait until then.
fn print(
    message: &Message,
    printed: &mut Printed,
    lines: &mut Vec<Line<'static>>,
    width: usize,
    theme: Theme,
) -> bool {
    let content = &message.content;
    let text = |printed: &mut Printed, lines: &mut Vec<Line<'static>>, to: usize| {
        if to > printed.text {
            for line in content[printed.text..to]
                .split('\n')
                .filter(|line| !line.is_empty())
            {
                lines.extend(text_lines(message.role, line, width, theme));
            }
            printed.text = to;
        }
    };
    // Each action after the text written before it.
    let mut actions: Vec<_> = message.actions.iter().collect();
    actions.sort_by_key(|action| action.position);
    for action in actions {
        if printed.ended.contains(&action.id) {
            continue;
        }
        let at = (0..=action.position.min(content.len()))
            .rev()
            .find(|&at| content.is_char_boundary(at))
            .unwrap_or(0);
        text(printed, lines, at);
        match action.state {
            ActionState::Waiting => {
                if printed.shown.insert(action.id) {
                    lines.extend(action_lines(action, width, theme, false));
                }
                return false;
            }
            ActionState::Running => return false,
            _ => {
                lines.extend(action_lines(action, width, theme, false));
                printed.ended.insert(action.id);
            }
        }
    }
    if message.status == Status::Streaming {
        // Up to the line being written.
        let end = content[printed.text..]
            .rfind('\n')
            .map_or(printed.text, |at| printed.text + at + 1);
        text(printed, lines, end);
        return false;
    }
    text(printed, lines, content.len());
    lines.extend(note(message, theme));
    printed.done = true;
    true
}

/// `lines` drawn into a buffer `width` columns wide and as high as they are.
fn paragraph(lines: Vec<Line<'static>>, width: u16) -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, lines.len() as u16));
    Paragraph::new(lines).render(buffer.area, &mut buffer);
    buffer
}

/// `blocks` one under the other, in one buffer; none if there are none.
fn stack(blocks: Vec<Buffer>, width: u16) -> Option<Buffer> {
    let height = blocks.iter().map(|block| block.area.height).sum();
    if height == 0 {
        return None;
    }
    let mut stacked = Buffer::empty(Rect::new(0, 0, width, height));
    let mut y = 0;
    for mut block in blocks {
        block.area.y = y;
        y += block.area.height;
        stacked.merge(&block);
    }
    Some(stacked)
}

/// The start banner, written once: the small mark beside the wordmark, what runs where, the conversations to
/// continue, and what else to know.
fn banner(app: &App, width: u16, theme: Theme) -> Buffer {
    let mark = Mark::new(Kelpie::Small, 2);
    let mark = (theme.marks() && width >= 18 + Wordmark::SIZE.0).then(|| theme.paint(mark, None));
    let x = if mark.is_some() { 18 } else { 1 };
    let text_width = usize::from(width.saturating_sub(x + 1));
    let (dim, text) = (Style::new().fg(theme.dim()), Style::new().fg(theme.text()));
    let banner = &app.banner;
    let mut lines = Vec::new();
    let wordmark = usize::from(Wordmark::SIZE.0) <= text_width;
    if !wordmark {
        lines.push(kelpy_name(theme));
    }
    let mut about = format!("v{}", banner.version);
    for part in [&banner.server, &banner.system, &app.provider.label] {
        if !part.is_empty() {
            about.push_str(&format!(" · {part}"));
        }
    }
    lines.extend(
        wrap(&about, text_width)
            .into_iter()
            .map(|line| Line::styled(line, text)),
    );
    lines.push(Line::from(vec![
        Span::styled("Web UI  ", dim),
        Span::styled(
            banner.web_ui.clone(),
            Style::new().fg(theme.accent()).underlined(),
        ),
    ]));
    if !app.sessions.is_empty() {
        let mut recent: Vec<String> = app
            .sessions
            .iter()
            .take(3)
            .enumerate()
            .map(|(index, session)| format!("{} {}", index + 1, session.title))
            .collect();
        recent.push("Ctrl+R all".to_owned());
        let line = format!("Continue: {}", recent.join(" · "));
        lines.extend(
            wrap(&line, text_width)
                .into_iter()
                .map(|line| Line::styled(line, dim)),
        );
    }
    for note in &banner.notes {
        lines.extend(
            wrap(note, text_width)
                .into_iter()
                .map(|line| Line::styled(line, dim)),
        );
    }
    let top = if wordmark { Wordmark::SIZE.1 + 2 } else { 0 };
    let (mark_width, mark_height) = mark.map_or((0, 0), |mark| mark.size());
    let height = mark_height.max(top + lines.len() as u16 + 1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    if let Some(mark) = mark {
        mark.render(Rect::new(0, 0, mark_width, mark_height), &mut buffer);
    }
    let column = Rect::new(
        x,
        u16::from(wordmark),
        width - x,
        height - u16::from(wordmark),
    );
    if wordmark {
        let (ink, cursor) = (theme.strong(), theme.accent());
        Wordmark { ink, cursor }.render(column, &mut buffer);
    }
    let rest = Rect::new(x, top, width - x, height - top);
    Paragraph::new(lines).render(rest, &mut buffer);
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{message, waiting};
    use crate::ui::tests::started;
    use serde_json::json;

    fn text(buffer: &Buffer) -> Vec<String> {
        buffer
            .content()
            .chunks(usize::from(buffer.area.width))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn the_banner_is_written_once_with_the_conversations_to_continue() {
        let mut scrollback = Scrollback::default();
        let mut app = started();
        app.loaded = false;
        assert!(scrollback.take(&app, 80, Theme::default()).is_none());
        app.loaded = true;
        let banner = text(&scrollback.take(&app, 80, Theme::default()).unwrap());
        let all = banner.join("\n");
        for expected in [
            "v0.4.0 · web-01 · Ubuntu 24.04 · Ollama (qwen3)",
            "Web UI  http://127.0.0.1:4321",
            "Continue: 1 Harden SSH · 2 Disk space on /var",
        ] {
            assert!(all.contains(expected), "{expected}\n{all}");
        }
        assert!(scrollback.take(&app, 80, Theme::default()).is_none());
    }

    #[test]
    fn an_answer_goes_into_the_history_a_finished_line_and_a_finished_action_at_a_time() {
        let mut scrollback = Scrollback::default();
        let mut app = started();
        app.loaded = true;
        scrollback.take(&app, 60, Theme::default());
        let printed = |scrollback: &mut Scrollback, app: &App| {
            scrollback
                .take(app, 60, Theme::default())
                .map(|buffer| text(&buffer))
                .unwrap_or_default()
        };
        app.selected = Some(1);
        let mut answer = message(3, Role::Assistant, "Checking the");
        answer.status = Status::Streaming;
        app.messages = vec![message(2, Role::User, "Is SSH safe?"), answer];
        let first = printed(&mut scrollback, &app);
        assert_eq!(first[1], "── Harden SSH");
        assert_eq!(first[3], "┃ Is SSH safe?");
        // Then the time it was written, and the blank line before the answer.
        assert!(first[4].starts_with("┃ "), "{first:#?}");
        assert_eq!(first.len(), 6, "{first:#?}");
        // The line being written stays in the footer.
        assert_eq!(
            scrollback.live(&app, 60, Theme::default())[0].to_string(),
            "  Checking the"
        );
        // An action: the text before it is complete; done, it is written as one line.
        let mut read = waiting(4, json!({ "state": "running", "position": 21 }));
        read.summary = "Read sshd_config".into();
        app.messages[1].content.push_str(" config.\n");
        app.messages[1].actions.push(read);
        assert_eq!(printed(&mut scrollback, &app), ["  Checking the config."]);
        assert!(
            scrollback.live(&app, 60, Theme::default())[0]
                .to_string()
                .contains("Read sshd_config  running")
        );
        app.messages[1].actions[0].state = ActionState::Done;
        app.messages[1].content.push_str("Password login is on.");
        assert_eq!(printed(&mut scrollback, &app), ["  ▸ ✓ Read sshd_config"]);
        // Ended, the rest; nothing twice.
        app.messages[1].status = Status::Complete;
        assert_eq!(printed(&mut scrollback, &app), ["  Password login is on."]);
        assert!(scrollback.take(&app, 60, Theme::default()).is_none());
        assert!(scrollback.live(&app, 60, Theme::default()).is_empty());
    }

    #[test]
    fn a_plan_waiting_is_written_whole_once_and_what_follows_it_waits() {
        let mut scrollback = Scrollback::default();
        let mut app = ui_working();
        // The plan once, without the keys: the footer has them.
        let lines = text(&scrollback.take(&app, 80, Theme::default()).unwrap()).join("\n");
        assert_eq!(lines.matches("◆ Plan  Harden SSH").count(), 1, "{lines}");
        assert!(lines.contains("▸ ✓ Ran sshd -T"), "{lines}");
        assert!(!lines.contains("Ctrl+Y"), "{lines}");
        assert!(scrollback.take(&app, 80, Theme::default()).is_none());
        app.messages.push(message(9, Role::User, "Later"));
        assert!(scrollback.take(&app, 80, Theme::default()).is_none());
    }

    /// The working conversation of the screens' tests, already loaded.
    fn ui_working() -> App {
        let mut app = crate::ui::tests::working();
        app.loaded = true;
        app
    }
}
