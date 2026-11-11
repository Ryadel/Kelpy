//! The terminal UI against a real runtime: the local server and the agent, with a model answering on loopback.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::{Stream, StreamExt, stream};
use kelpy_core::home::Home;
use kelpy_core::providers::chatgpt::Endpoints;
use kelpy_core::storage::Store;
use kelpy_gateway::{Activity, Agent, Auth, RuntimeStatus, StopRequest};
use kelpy_test_support::chat_completions;
use kelpy_tui::{Api, Background, Banner, Exit, Options, Screen};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::{Terminal, TerminalOptions, Viewport};

const WIDTH: u16 = 110;

/// A runtime on a loopback port, answering with `pieces`; returns the API for the terminal UI and the agent.
async fn runtime(pieces: &'static [&'static str]) -> (Api, Arc<Agent>) {
    let home = Home::init(tempfile::tempdir().unwrap().keep()).unwrap();
    let agent = Arc::new(
        Agent::new(
            &home,
            Store::in_memory().unwrap(),
            None,
            Endpoints::default(),
        )
        .unwrap(),
    );
    let provider = serde_json::json!({
        "kind": "ollama", "model": "test", "base_url": chat_completions(pieces, Duration::ZERO).await,
    });
    agent
        .configure_provider(serde_json::from_value(provider).unwrap())
        .unwrap();
    let listener = kelpy_gateway::bind(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, None)
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let auth = Arc::new(Auth::new().unwrap());
    let status = RuntimeStatus {
        version: "0.0.0-test".into(),
        pid: 1,
        home: PathBuf::from("/tmp/kelpy"),
        started_at: 0,
        account: "admin".into(),
        account_limits: None,
    };
    let router = kelpy_gateway::router(
        status,
        port,
        Arc::new(Activity::new()),
        Arc::clone(&auth),
        Arc::clone(&agent),
        Arc::new(StopRequest::new()),
        Arc::new(kelpy_gateway::Elevation::new(
            false,
            None,
            Arc::clone(&agent),
        )),
    );
    tokio::spawn(kelpy_gateway::serve(
        listener,
        router,
        std::future::pending(),
    ));
    let api = Api::new(format!("http://127.0.0.1:{port}"), auth.token().to_owned()).unwrap();
    (api, agent)
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Types `text` and presses Enter, then waits `wait` and ends: the terminal UI then ends too.
fn typing(text: &str, wait: Duration) -> impl Stream<Item = Event> + Unpin {
    let keys: Vec<Event> = text
        .chars()
        .map(|char| key(KeyCode::Char(char)))
        .chain([key(KeyCode::Enter)])
        .collect();
    let pause = stream::once(tokio::time::sleep(wait)).filter_map(|()| std::future::ready(None));
    Box::pin(stream::iter(keys).chain(pause))
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    text(terminal.backend().buffer())
}

fn text(buffer: &Buffer) -> String {
    buffer
        .content()
        .chunks(WIDTH as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>() + "\n")
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_the_conversation_goes_into_the_terminals_history_and_the_footer_goes_at_the_end() {
    // Put into JSON as they are: the new line escaped.
    let (api, _agent) = runtime(&["The disks ", "have room.\\nNothing to do."]).await;
    let options = TerminalOptions {
        viewport: Viewport::Inline(10),
    };
    let mut terminal = Terminal::with_options(TestBackend::new(WIDTH, 16), options).unwrap();
    let options = Options {
        banner: Banner {
            version: "0.0.0-test".into(),
            web_ui: "http://127.0.0.1".into(),
            ..Banner::default()
        },
        stops_runtime: false,
        screen: Screen::Inline,
        background: Background::Dark,
        open: None,
    };
    let exit = kelpy_tui::run(
        &mut terminal,
        typing("Is there disk space?", Duration::from_secs(2)),
        api,
        options,
    )
    .await
    .unwrap();
    assert_eq!(exit, Exit::Quit);
    // What scrolled away and what is still on the screen, in order: written once each.
    let written = text(terminal.backend().scrollback()) + &screen(&terminal);
    let order: Vec<usize> = [
        "v0.0.0-test",
        "Web UI  http://127.0.0.1",
        "┃ Is there disk space?",
        "  The disks have room.",
        "  Nothing to do.",
    ]
    .iter()
    .map(|expected| {
        assert_eq!(
            written.matches(expected).count(),
            1,
            "{expected}\n{written}"
        );
        written.find(expected).unwrap()
    })
    .collect();
    assert!(order.is_sorted(), "{written}");
    // The footer is gone.
    assert!(!written.contains("Write a message"), "{written}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_typed_in_the_terminal_ui_is_answered_there_and_saved() {
    let (api, agent) = runtime(&["The disks ", "have room."]).await;
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 24)).unwrap();
    let options = Options {
        banner: Banner {
            version: "0.0.0-test".into(),
            web_ui: "http://127.0.0.1".into(),
            ..Banner::default()
        },
        stops_runtime: true,
        screen: Screen::Full,
        background: Background::Dark,
        open: None,
    };
    let exit = kelpy_tui::run(
        &mut terminal,
        typing("Is there disk space?", Duration::from_secs(2)),
        api,
        options,
    )
    .await
    .unwrap();
    assert_eq!(exit, Exit::Quit);
    let shown = screen(&terminal);
    for expected in [
        "Ollama (test)",
        "Is there disk space?",
        "The disks have room.",
        " Read-only ",
    ] {
        assert!(shown.contains(expected), "{expected}\n{shown}");
    }
    // The conversation is Kelpy's, as from any interface.
    let sessions = agent.sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    let messages = agent.messages(sessions[0].id).await.unwrap();
    assert_eq!(messages[1].content, "The disks have room.");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_from_another_interface_shows_and_kelpy_stopping_ends_the_terminal_ui() {
    let (api, agent) = runtime(&["Fine."]).await;
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 24)).unwrap();
    let options = Options {
        banner: Banner::default(),
        stops_runtime: false,
        screen: Screen::Full,
        background: Background::Dark,
        open: None,
    };
    let other = Arc::clone(&agent);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        other
            .create_session(Some("From the Web UI"), Default::default())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        other.stop_all(Duration::from_secs(1)).await;
    });
    let exit = kelpy_tui::run(&mut terminal, stream::pending(), api, options)
        .await
        .unwrap();
    assert_eq!(exit, Exit::Stopped);
    assert!(screen(&terminal).contains("From the Web UI"));
}
