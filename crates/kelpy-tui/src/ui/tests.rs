use super::*;
use crate::Banner;
use crate::app::tests::{message, session, waiting};
use crate::model::{ContextUse, ProviderState, Role};
use crate::theme::Colours;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use serde_json::json;

/// Kelpy started on web-01, with three conversations.
pub(crate) fn started() -> App {
    let mut app = App::new(
        Banner {
            version: "0.4.0".into(),
            web_ui: "http://127.0.0.1:4321".into(),
            server: "web-01".into(),
            system: "Ubuntu 24.04".into(),
            notes: Vec::new(),
        },
        true,
    );
    app.provider = ProviderState {
        label: "Ollama (qwen3)".into(),
        problem: None,
    };
    app.sessions = ["Harden SSH", "Disk space on /var", "Nginx 502 errors"]
        .into_iter()
        .zip(1..)
        .map(|(title, id)| {
            let mut session = session(id, unix_now() - 600 * id);
            session.title = title.into();
            session
        })
        .collect();
    app
}

/// `started`, with "Harden SSH" open: a question, two reads, an answer and a plan waiting.
pub(crate) fn working() -> App {
    let mut app = started();
    app.selected = Some(1);
    app.context = Some(ContextUse {
        used: 23_000,
        window: 128_000,
    });
    app.sessions[0].access = Access::Full;
    app.sessions[0].approval = Approval::Plan;
    let mut answer = message(
        3,
        Role::Assistant,
        "Password login is on: anyone can try passwords.",
    );
    for (id, summary) in [(4, "Read /etc/ssh/sshd_config"), (5, "Ran sshd -T")] {
        let mut read = waiting(id, json!({ "state": "done" }));
        read.summary = summary.into();
        answer.actions.push(read);
    }
    answer.actions.push(waiting(
        6,
        json!({
            "tool": "propose_plan", "summary": "Plan: harden SSH", "command": null, "position": 100,
            "reason": "Password login lets anyone try passwords against web-01.",
            "plan": { "goal": "Harden SSH", "steps": [
                { "tool": "write_file", "input": "{}", "summary": "Update sshd_config",
                  "detail": "-PasswordAuthentication yes\n+PasswordAuthentication no",
                  "floor": "it changes how people log in (/etc/ssh)" },
                { "tool": "run_command", "input": "{}", "summary": "Check the configuration",
                  "detail": "sshd -t", "floor": null },
                { "tool": "run_command", "input": "{}", "summary": "Reload SSH",
                  "detail": "systemctl reload ssh", "floor": null },
            ] },
        }),
    ));
    app.messages = vec![
        message(
            2,
            Role::User,
            "Disable password login for SSH and reload it.",
        ),
        answer,
    ];
    app
}

/// `app` drawn on a terminal `width` × `height` columns and rows.
pub(crate) fn drawn(app: &App, (width, height): (u16, u16), theme: Theme) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let terminal_can = Capabilities {
        theme,
        shift_enter: true,
        inline: false,
    };
    terminal
        .draw(|frame| draw(frame, app, &mut Transcript::default(), terminal_can))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// The footer of `app` drawn inline, `width` columns wide, with `live` above it.
fn drawn_inline(app: &App, width: u16, live: Vec<Line<'static>>) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
    let terminal_can = Capabilities {
        theme: DARK,
        shift_enter: false,
        inline: true,
    };
    terminal
        .draw(|frame| draw_inline(frame, app, terminal_can, live))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    buffer
        .content()
        .chunks(usize::from(buffer.area.width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>() + "\n")
        .collect()
}

fn shows(buffer: &Buffer, expected: &[&str]) {
    let screen = text(buffer);
    for expected in expected {
        assert!(screen.contains(expected), "{expected}\n{screen}");
    }
}

/// Whether `buffer` shows a kelpie: its black body, a colour of the mark only.
fn has_kelpie(buffer: &Buffer) -> bool {
    let body = Color::Rgb(0x11, 0x13, 0x12);
    buffer
        .content()
        .iter()
        .any(|cell| cell.fg == body || cell.bg == body)
}

const DARK: Theme = Theme {
    colours: Colours::True,
    dark: true,
    background: crate::theme::TERMINAL,
};
const BASIC: Theme = Theme {
    colours: Colours::Basic,
    ..DARK
};

#[test]
fn the_start_screen_has_the_largest_kelpie_the_terminal_fits_beside_the_text() {
    let size = |width, height, theme| {
        start_mark(theme, Rect::new(0, 0, width, height)).map(|mark| mark.size())
    };
    assert_eq!(size(140, 40, DARK), Some((50, 29)));
    assert_eq!(size(100, 32, DARK), Some((38, 22)));
    assert_eq!(size(80, 24, DARK), Some((16, 11)));
    // Too narrow or too short for any beside the text, or only 16 colours.
    assert_eq!(size(54, 20, DARK), None);
    assert_eq!(size(140, 19, DARK), None);
    assert_eq!(size(140, 40, BASIC), None);
}

#[test]
fn the_start_screen_tells_what_runs_where_and_the_conversations_to_continue() {
    for size in [(140, 40), (100, 32), (80, 24)] {
        let screen = drawn(&started(), size, DARK);
        assert!(has_kelpie(&screen), "{size:?}");
        shows(
            &screen,
            &[
                "v0.4.0  ·  web-01  ·  Ubuntu 24.04",
                "Ollama (qwen3) ●",
                "Web UI  http://127.0.0.1:4321",
                "Continue",
                "1  Harden SSH",
                "3  Nginx 502 errors",
                "Ctrl+R all conversations",
                "› █ Write, or press a number to continue",
                " Read-only   Approval: Ask   Shift+Tab",
                "Ctrl+C stop Kelpy",
            ],
        );
    }
    shows(
        &drawn(&started(), (140, 40), DARK),
        &["Enter send · Shift+Enter new line"],
    );
    // With 16 colours, the wordmark alone.
    let screen = drawn(&started(), (140, 40), BASIC);
    assert!(!has_kelpie(&screen));
    shows(&screen, &["██    ", "Continue"]);
}

#[test]
fn the_start_screen_shows_what_the_provider_needs_whole_and_without_backticks() {
    let mut app = started();
    app.provider.problem = Some("choose one in the Web UI or run `kelpy provider`".into());
    for size in [(140, 40), (80, 24)] {
        let screen = text(&drawn(&app, size, DARK));
        assert!(
            !screen.contains('`'),
            "{size:?}
{screen}"
        );
        let words: Vec<&str> = screen.split_whitespace().collect();
        let wanted = [
            "choose", "one", "in", "the", "Web", "UI", "or", "run", "kelpy", "provider",
        ];
        assert!(
            words.windows(wanted.len()).any(|run| run == wanted)
                || screen.contains("run kelpy provider"),
            "{size:?}
{screen}"
        );
    }
}

#[test]
fn a_wide_terminal_has_the_side_panel_beside_the_conversation() {
    let screen = drawn(&working(), (140, 40), DARK);
    assert!(has_kelpie(&screen));
    shows(
        &screen,
        &[
            "# Harden SSH",
            "┃ Disable password login for SSH and reload it.",
            "▸ ✓ Read /etc/ssh/sshd_config",
            "Password login is on: anyone can try passwords.",
            "◆ Plan  Harden SSH · 3 steps, 1 always asks",
            "-PasswordAuthentication yes",
            "! Always asks: it changes how people log in (/etc/ssh)",
            " Approve    Deny ",
            " Full access   Approval: Plan   Shift+Tab",
            "A decision waits",
            "Context",
            "18%",
            "23k of 128k tokens",
            "Model     Ollama (qwen3)",
            "Access    Full",
            "Approval  Plan",
            "◆ Harden SSH",
            "kelpy_  v0.4.0",
            "Ctrl+B hides",
            "Ctrl+Y approve · Ctrl+N deny",
        ],
    );
    // Hidden with Ctrl+B, it leaves the conversation the whole width.
    let mut app = working();
    app.panel_hidden = true;
    let screen = drawn(&app, (140, 40), DARK);
    assert!(!text(&screen).contains("Ctrl+B hides"));
}

#[test]
fn a_narrower_terminal_has_a_header_and_the_panel_over_the_conversation_on_ctrl_b() {
    let mut app = working();
    let screen = drawn(&app, (100, 32), DARK);
    assert!(!has_kelpie(&screen));
    shows(
        &screen,
        &[
            "kelpy_  Harden SSH  web-01",
            "ctx █░░░░░░░  18%  Ollama (qwen3)",
            "◆ Plan",
        ],
    );
    app.overlay = true;
    let screen = drawn(&app, (100, 32), DARK);
    assert!(has_kelpie(&screen));
    shows(&screen, &["Access    Full", "Esc closes"]);
    // The conversation under it is dimmed.
    assert!(screen[(2, 2)].modifier.contains(Modifier::DIM));
}

#[test]
fn a_small_pane_still_shows_the_plan_the_modes_and_the_keys() {
    let screen = drawn(&working(), (54, 20), DARK);
    shows(&screen, &[" Approve ", " Full access ", "Ctrl+Y approve"]);
    let screen = drawn(&started(), (54, 20), DARK);
    assert!(!has_kelpie(&screen));
    shows(&screen, &["Continue", "› █"]);
    // A message of many lines leaves the kelpie no room: it is left out.
    let mut app = started();
    app.input = "line\n".repeat(6);
    assert!(!has_kelpie(&drawn(&app, (80, 20), DARK)));
    assert!(has_kelpie(&drawn(&started(), (80, 20), DARK)));
}

#[test]
fn inline_the_footer_has_the_message_box_and_the_status_line() {
    let mut app = started();
    app.selected = Some(2);
    app.messages = vec![message(5, Role::User, "Disk?")];
    app.answering.insert(2);
    let live = vec![Line::raw("  Checking the")];
    let screen = drawn_inline(&app, 80, live);
    let rows: Vec<String> = text(&screen).lines().map(str::to_owned).collect();
    assert_eq!(
        rows[9].trim_end(),
        " web-01 · Read-only · Ask               Ctrl+R conversations · Ctrl+C stop Kelpy"
    );
    assert!(rows[4].starts_with("─"), "{rows:#?}");
    assert_eq!(rows[3].trim_end(), "  Checking the");
    shows(&screen, &["› █ Write a message", "Esc stop the answer"]);
}

#[test]
fn inline_a_decision_takes_the_footer_its_keys_in_one_row_or_one_per_row() {
    let screen = drawn_inline(&working(), 80, Vec::new());
    shows(
        &screen,
        &[
            "Approve the plan?  3 steps · 1 always asks",
            " Ctrl+Y  Approve and run    Ctrl+N  Deny     Esc stop the answer",
            "web-01 · Ollama (qwen3) · ctx 17% · Full · Plan",
            " 1 decision waits ",
        ],
    );
    // Narrow: one key per row, and the model leaves the status line first.
    let screen = drawn_inline(&working(), 54, Vec::new());
    let all = text(&screen);
    shows(
        &screen,
        &[
            " Ctrl+Y  Approve and run",
            " Ctrl+N  Deny",
            " web-01 · ctx 17% · Full",
        ],
    );
    assert!(!all.contains("Ollama"), "{all}");
}

#[test]
fn ctrl_r_lists_the_conversations_over_the_screen() {
    let mut app = working();
    app.focus = Focus::Conversations;
    shows(
        &drawn(&app, (100, 32), DARK),
        &[
            " Conversations ",
            "+ New conversation",
            "Disk space on /var",
            "Esc back",
        ],
    );
}

#[test]
fn any_size_of_terminal_draws_without_panicking() {
    let mut overlay = working();
    overlay.overlay = true;
    let mut listing = working();
    listing.focus = Focus::Conversations;
    let mut typing = working();
    typing.input = "word ".repeat(80);
    let apps = [started(), working(), overlay, listing, typing];
    for app in &apps {
        for width in [1, 2, 3, 4, 6, 8, 12, 20, 33, 40, 56, 80, 121] {
            for height in (1..=12).chain([24, 40]) {
                drawn(app, (width, height), DARK);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let terminal_can = Capabilities {
                    theme: DARK,
                    shift_enter: false,
                    inline: true,
                };
                terminal
                    .draw(|frame| draw_inline(frame, app, terminal_can, vec![Line::raw("live")]))
                    .unwrap();
            }
        }
    }
}
