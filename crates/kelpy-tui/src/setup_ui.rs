//! How the setup window looks. In full screen, a window over the conversation, which stays behind it dimmed. Inline,
//! as over SSH, the footer: what the step explains goes once into the terminal's history above it, where a QR code
//! has the rows it needs, and the footer keeps what is chosen or typed.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::setup::{Control, Para, Tone, View};
use crate::theme::Theme;
use crate::transcript::wrap;

/// The window's widest, in columns: room for the explanations and a QR code.
const WINDOW_WIDTH: u16 = 84;

/// Lines and which of them to keep in sight.
struct Drawn {
    lines: Vec<Line<'static>>,
    /// The first and last line of the control chosen or typed in, if any.
    focus: Option<(usize, usize)>,
}

/// The setup window over `area`, which it dims.
pub fn window(frame: &mut Frame, view: &View, theme: Theme, area: Rect) {
    frame
        .buffer_mut()
        .set_style(area, Style::new().add_modifier(Modifier::DIM));
    let width = area.width.saturating_sub(4).min(WINDOW_WIDTH);
    // Borders, and a column of air on each side.
    let inner = usize::from(width.saturating_sub(4));
    let room = usize::from(area.height.saturating_sub(4)).saturating_sub(2);
    let mut drawn = body(view, inner, theme, true, true);
    if drawn.lines.len() > room {
        // A QR code that does not fit is left out: the address and the code stay.
        drawn = body(view, inner, theme, true, false);
    }
    let height = (drawn.lines.len().min(room) + 2) as u16;
    let window = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, window);
    let mut block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme.accent()))
        .title(Line::styled(
            format!(" {} ", view.title),
            Style::new().fg(theme.strong()).bold(),
        ))
        .title(Line::styled(" Ctrl+P ", Style::new().fg(theme.dim())).right_aligned())
        .style(Style::new().bg(theme.panel()));
    if !view.keys.is_empty() {
        block = block.title_bottom(Line::styled(
            format!(" {} ", view.keys),
            Style::new().fg(theme.dim()),
        ));
    }
    let content = block
        .inner(window)
        .inner(ratatui::layout::Margin::new(1, 0));
    frame.render_widget(block, window);
    let lines = visible(drawn, usize::from(content.height));
    frame.render_widget(Paragraph::new(lines), content);
}

/// The setup in the footer of the terminal UI inline: its title on a rule, what is chosen or typed, and its keys;
/// `keys` false where the last row says something else.
pub fn footer(frame: &mut Frame, view: &View, theme: Theme, area: Rect, keys: bool) {
    frame.render_widget(Clear, area);
    frame
        .buffer_mut()
        .set_style(area, Style::new().bg(theme.panel()));
    let width = usize::from(area.width);
    let title = format!("─ {} ", view.title);
    let rule = Line::from(vec![
        Span::styled(title.clone(), Style::new().fg(theme.strong()).bold()),
        Span::styled(
            "─".repeat(width.saturating_sub(title.width() + 8)),
            Style::new().fg(theme.accent()),
        ),
        Span::styled(" Ctrl+P", Style::new().fg(theme.dim())),
    ]);
    frame.render_widget(Paragraph::new(rule), Rect { height: 1, ..area });
    let middle = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(3),
        height: area.height.saturating_sub(2),
    };
    let drawn = body(view, usize::from(middle.width), theme, false, false);
    frame.render_widget(
        Paragraph::new(visible(drawn, usize::from(middle.height))),
        middle,
    );
    if keys {
        let row = Rect {
            y: area.bottom().saturating_sub(1),
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" {}", view.keys),
                Style::new().fg(theme.dim()),
            )),
            row,
        );
    }
}

/// What the step explains, for the terminal's history inline, `width` columns wide: its title, then the text.
pub fn history(view: &View, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled("── ", Style::new().fg(theme.accent())),
            Span::styled(view.title.clone(), Style::new().fg(theme.strong()).bold()),
        ]),
    ];
    for para in &view.text {
        lines.extend(para_lines(para, width.saturating_sub(2), theme, "  "));
    }
    lines
}

/// The lines to show, `room` of them, keeping the focus in sight: from the top, unless the focus is further down.
fn visible(drawn: Drawn, room: usize) -> Vec<Line<'static>> {
    let start = drawn.focus.map_or(0, |(first, last)| {
        (last + 1)
            .saturating_sub(room)
            .min(first)
            .min(drawn.lines.len().saturating_sub(room))
    });
    drawn.lines.into_iter().skip(start).take(room).collect()
}

/// The step's body, `width` columns wide: the text and the controls in full; `compact`, the controls only, each
/// choice's description only when it is chosen. `qr`: with the QR code.
fn body(view: &View, width: usize, theme: Theme, full: bool, qr: bool) -> Drawn {
    let mut lines = Vec::new();
    if full {
        for para in &view.text {
            if para.tone == Tone::Qr && !qr {
                continue;
            }
            lines.extend(para_lines(para, width, theme, ""));
        }
        if !view.text.is_empty() {
            lines.push(Line::raw(""));
        }
    }
    let mut focus = None;
    for control in &view.controls {
        let first = lines.len();
        let focused = control_lines(control, width, theme, full, &mut lines);
        if focused && (focus.is_none() || matches!(control, Control::Choice { .. })) {
            focus = Some((first, lines.len().saturating_sub(1)));
        }
    }
    Drawn { lines, focus }
}

fn para_lines(para: &Para, width: usize, theme: Theme, margin: &str) -> Vec<Line<'static>> {
    let style = Style::new().fg(match para.tone {
        Tone::Text => theme.text(),
        Tone::Dim => theme.dim(),
        Tone::Strong | Tone::Qr => theme.strong(),
        Tone::Link => theme.accent(),
        Tone::Good => theme.good(),
        Tone::Warn => theme.warn(),
        Tone::Bad => theme.bad(),
    });
    let style = match para.tone {
        Tone::Strong => style.bold(),
        Tone::Link => style.underlined(),
        _ => style,
    };
    if para.tone == Tone::Qr {
        return para
            .text
            .lines()
            .map(|line| Line::styled(format!("{margin}{line}"), style))
            .collect();
    }
    let lead = para.lead.width();
    let mut lines = Vec::new();
    for (index, line) in para
        .text
        .split('\n')
        .flat_map(|part| wrap(part, width.saturating_sub(lead).max(1)))
        .enumerate()
    {
        let before = if index == 0 {
            Span::styled(
                format!("{margin}{}", para.lead),
                Style::new().fg(theme.dim()),
            )
        } else {
            Span::raw(format!("{margin}{}", " ".repeat(lead)))
        };
        lines.push(Line::from(vec![before, Span::styled(line, style)]));
    }
    lines
}

/// Adds the lines of `control`; returns whether it is the one chosen or typed in.
fn control_lines(
    control: &Control,
    width: usize,
    theme: Theme,
    full: bool,
    lines: &mut Vec<Line<'static>>,
) -> bool {
    let dim = Style::new().fg(theme.dim());
    match control {
        Control::Choice {
            name,
            detail,
            description,
            selected,
        } => {
            let (lead, style) = if *selected {
                (
                    Span::styled("› ", Style::new().fg(theme.accent()).bold()),
                    Style::new().fg(theme.strong()).bold(),
                )
            } else {
                (Span::raw("  "), Style::new().fg(theme.text()))
            };
            let mut spans = vec![lead, Span::styled(name.clone(), style)];
            if let Some(detail) = detail {
                spans.push(Span::styled(format!("  {detail}"), dim));
            }
            let mut line = Line::from(spans);
            if *selected {
                line = line.bg(theme.chip());
            }
            lines.push(line);
            if let Some(description) = description.as_ref().filter(|_| full || *selected) {
                for part in description.split('\n') {
                    for wrapped in wrap(part, width.saturating_sub(4)) {
                        lines.push(Line::styled(format!("    {wrapped}"), dim));
                    }
                }
            }
            *selected
        }
        Control::Field {
            label,
            note,
            shown,
            search,
        } => {
            if !label.is_empty() {
                let mut spans = vec![Span::styled(
                    label.clone(),
                    Style::new().fg(theme.strong()).bold(),
                )];
                if let Some(note) = note {
                    spans.push(Span::styled(note.clone(), dim));
                }
                lines.push(Line::from(spans));
            }
            let prompt = if *search { "/ " } else { "› " };
            // The end of a long value, as typed last.
            let room = width.saturating_sub(4);
            let skip = shown.chars().count().saturating_sub(room);
            let value: String = shown.chars().skip(skip).collect();
            lines.push(
                Line::from(vec![
                    Span::styled(prompt, Style::new().fg(theme.accent()).bold()),
                    Span::styled(value, Style::new().fg(theme.strong())),
                    Span::styled("█", Style::new().fg(theme.accent())),
                ])
                .bg(theme.surface()),
            );
            true
        }
        Control::Buttons(buttons) => {
            let mut spans = Vec::new();
            for (name, lit) in buttons {
                if !spans.is_empty() {
                    spans.push(Span::raw("  "));
                }
                let style = if *lit {
                    Style::new().fg(theme.on_chip()).bg(theme.accent()).bold()
                } else {
                    Style::new().fg(theme.text()).bg(theme.chip())
                };
                spans.push(Span::styled(format!(" {name} "), style));
            }
            lines.push(Line::from(spans));
            true
        }
        Control::Bar(share) => {
            let share = share.clamp(0.0, 1.0);
            let room = width.saturating_sub(6).min(40);
            let filled = (share * room as f64).round() as usize;
            lines.push(Line::from(vec![
                Span::styled("━".repeat(filled), Style::new().fg(theme.accent())),
                Span::styled("━".repeat(room - filled), dim),
                Span::styled(
                    format!(" {:>3.0}%", share * 100.0),
                    Style::new().fg(theme.text()),
                ),
            ]));
            false
        }
        Control::Para(para) => {
            lines.extend(para_lines(para, width, theme, ""));
            false
        }
    }
}
