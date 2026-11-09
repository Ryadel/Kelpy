//! Kelpy's terminal UI: the conversations, their answers and actions, and what the banner would show.
//!
//! It is a client of the runtime's local API, like the Web UI, and follows the runtime's event channel, so that both
//! stay in step. It draws on any ratatui backend and reads keys from any stream of terminal events: the local
//! terminal ([`run_local`]) or an SSH session ([`run_remote`]). It takes the whole screen, or draws inline: the
//! conversation into the terminal's own history and only a footer redrawn, as over SSH. The state and the keys are in
//! [`app`], pure; this module runs what they ask of the runtime and draws.

mod api;
mod app;
mod inline;
mod mark;
mod model;
mod remote;
mod theme;
mod transcript;
mod ui;

use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement};
use futures_util::{Stream, StreamExt};
use kelpy_common::lock;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Size;
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};

pub use api::Api;
pub use app::Exit;
use app::{App, Effect, Update};
pub use kelpy_client::ClientError;
use mark::Rgb;
use model::{ChatEvent, RuntimeEvent};
use remote::{AnsiBackend, KeyParser};
use terminal_colorsaurus::QueryOptions;
use theme::{Colours, Theme};

/// How many updates from the runtime may wait for the screen: they are drawn as fast as they come.
const UPDATES: usize = 256;
/// How long the event channel waits before connecting again.
const RECONNECT: Duration = Duration::from_secs(2);
/// The rows the footer takes inline: the message box or a decision, the status line, and what is under way.
const INLINE_HEIGHT: u16 = 10;
/// How long the terminal of an SSH session has to tell its background. Terminals that do not tell say so at once.
const ASK_TIME: Duration = Duration::from_secs(1);

pub struct Options {
    pub banner: Banner,
    /// Whether quitting stops Kelpy: this interface started it.
    pub stops_runtime: bool,
    pub screen: Screen,
    pub background: Background,
}

/// The terminal's background, as it says, or as the user does where it cannot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Background {
    #[default]
    Ask,
    Dark,
    Light,
}

impl Background {
    /// The theme for a terminal showing `colours`, whose background is `told` if it said.
    fn theme(self, colours: Colours, told: Option<Rgb>) -> Theme {
        match (self, told) {
            (Self::Dark, _) | (Self::Ask, None) => Theme::new(colours, true),
            (Self::Light, _) => Theme::new(colours, false),
            (Self::Ask, Some(background)) => Theme {
                background,
                ..Theme::new(colours, theme::is_dark(background))
            },
        }
    }
}

/// Where the terminal UI draws.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Screen {
    /// Inline over SSH, where a full screen is slow to redraw and its history is the only one; else the whole screen.
    #[default]
    Auto,
    /// In the terminal's own history, with a footer: what is said stays there after the terminal UI ends.
    Inline,
    /// The whole screen, given back as it was at the end.
    Full,
}

/// What the start screen tells of the Kelpy running.
#[derive(Debug, Clone, Default)]
pub struct Banner {
    /// Kelpy's version, as `0.4.0`.
    pub version: String,
    /// The address that opens the Web UI signed in.
    pub web_ui: String,
    /// The server's name.
    pub server: String,
    /// Its operating system, as `Ubuntu 24.04`.
    pub system: String,
    /// What else to know, a line each: the code, the home, notices.
    pub notes: Vec<String>,
}

/// Runs the terminal UI in this process's terminal, until the user quits or Kelpy stops. The terminal is given back
/// as it was when this ends, fails or is dropped.
pub async fn run_local(api: Api, options: Options) -> io::Result<Exit> {
    // Asked before the terminal UI takes the terminal, which answers on its input.
    let colours = theme::colours_from(|name| std::env::var(name).ok(), cfg!(windows));
    let told = (options.background == Background::Ask)
        .then(|| terminal_colorsaurus::background_color(QueryOptions::default()).ok())
        .flatten()
        .map(|colour| {
            let (red, green, blue) = colour.scale_to_8bit();
            [red, green, blue]
        });
    let theme = options.background.theme(colours, told);
    let inline = match options.screen {
        Screen::Auto => over_ssh(),
        Screen::Inline => true,
        Screen::Full => false,
    };
    let mut local = LocalTerminal {
        inline,
        shift_enter: false,
    };
    let mut terminal = if inline {
        enable_raw_mode()?;
        let options = TerminalOptions {
            viewport: Viewport::Inline(INLINE_HEIGHT),
        };
        Terminal::with_options(CrosstermBackend::new(io::stdout()), options)?
    } else {
        ratatui::init()
    };
    // Asked once the terminal is in raw mode, as it answers on its input.
    local.shift_enter = cfg!(windows) || supports_keyboard_enhancement().unwrap_or(false);
    crossterm::execute!(io::stdout(), EnableBracketedPaste)?;
    if local.shift_enter && !cfg!(windows) {
        // Keys told apart, so that Shift+Enter is not Enter.
        crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let input = EventStream::new().filter_map(|event| std::future::ready(event.ok()));
    let terminal_can = Capabilities {
        theme,
        shift_enter: local.shift_enter,
        inline,
    };
    run_on(&mut terminal, input, api, options, terminal_can).await
}

/// Whether this process runs in an SSH session: OpenSSH sets these, on Unix and on Windows.
fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_TTY"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// The local terminal in the terminal UI's modes, until dropped.
struct LocalTerminal {
    inline: bool,
    shift_enter: bool,
}

impl Drop for LocalTerminal {
    fn drop(&mut self) {
        // Best effort: nothing more can be done for a terminal that does not take it.
        if self.shift_enter && !cfg!(windows) {
            let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        if self.inline {
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(io::stdout(), crossterm::cursor::Show);
        } else {
            ratatui::restore();
        }
    }
}

/// What the terminal can show and tell apart, and how the terminal UI draws on it.
#[derive(Debug, Clone, Copy, Default)]
struct Capabilities {
    theme: Theme,
    /// Whether it tells Shift+Enter from Enter; Alt+Enter is told apart everywhere.
    shift_enter: bool,
    /// Drawn inline, rather than on the whole screen.
    inline: bool,
}

impl Capabilities {
    /// The key that starts a new line in the message.
    fn newline_key(self) -> &'static str {
        if self.shift_enter {
            "Shift+Enter"
        } else {
            "Alt+Enter"
        }
    }
}

/// What a remote terminal sends: the bytes of its keys, or its new size in columns and rows.
pub enum RemoteInput {
    Bytes(Vec<u8>),
    Resize(u16, u16),
}

/// A terminal reached through a byte stream, as its SSH session describes it.
pub struct RemoteTerminal {
    pub columns: u16,
    pub rows: u16,
    /// Its `TERM`, such as `xterm-256color`.
    pub name: String,
}

/// Runs the terminal UI on a terminal reached through a byte stream, such as an SSH session: it draws to `out` with
/// ANSI sequences and reads `input`, until the user quits, `input` ends or Kelpy stops. The terminal is given back as
/// it was, if `out` still takes it.
pub async fn run_remote<W: Write>(
    mut out: W,
    mut input: impl Stream<Item = RemoteInput> + Unpin,
    remote: RemoteTerminal,
    api: Api,
    options: Options,
) -> io::Result<Exit> {
    let inline = options.screen != Screen::Full;
    let colours = theme::colours_from(|name| (name == "TERM").then(|| remote.name.clone()), false);
    // The background asked through the stream: what comes before the answer is kept for the keys.
    let mut early = Vec::new();
    let mut size = (remote.columns, remote.rows);
    let mut told = None;
    if options.background == Background::Ask {
        out.write_all(remote::ASK_BACKGROUND.as_bytes())?;
        out.flush()?;
        let answer = tokio::time::timeout(ASK_TIME, async {
            while let Some(next) = input.next().await {
                match next {
                    RemoteInput::Bytes(bytes) => {
                        early.extend(bytes);
                        if let Some(answer) = remote::background_answer(&early) {
                            return Some(answer);
                        }
                    }
                    RemoteInput::Resize(columns, rows) => size = (columns, rows),
                }
            }
            None
        });
        if let Ok(Some((background, rest))) = answer.await {
            (told, early) = (background, rest);
        }
    }
    let theme = options.background.theme(colours, told);
    let input = futures_util::stream::iter([RemoteInput::Bytes(early)]).chain(input);
    let rows = size.1;
    let size = Arc::new(Mutex::new(Size::new(size.0, size.1)));
    out.write_all(
        if inline {
            remote::INLINE_ENTER
        } else {
            remote::ENTER
        }
        .as_bytes(),
    )?;
    let mut backend = AnsiBackend::new(out, Arc::clone(&size));
    let mut terminal = if inline {
        // From the last row, where the terminal's history goes on.
        backend.set_cursor_position((0, rows.saturating_sub(1)))?;
        let options = TerminalOptions {
            viewport: Viewport::Inline(INLINE_HEIGHT),
        };
        Terminal::with_options(backend, options)?
    } else {
        backend.flush()?;
        Terminal::new(backend)?
    };
    let mut parser = KeyParser::default();
    let events = input.flat_map(move |input| {
        futures_util::stream::iter(match input {
            RemoteInput::Bytes(bytes) => parser.feed(&bytes),
            RemoteInput::Resize(columns, rows) => {
                *lock(&size) = Size::new(columns, rows);
                vec![Event::Resize(columns, rows)]
            }
        })
    });
    let terminal_can = Capabilities {
        theme,
        inline,
        ..Capabilities::default()
    };
    let exit = run_on(&mut terminal, events, api, options, terminal_can).await;
    // Best effort: the other end may be gone already.
    let leave = if inline {
        remote::INLINE_LEAVE
    } else {
        remote::LEAVE
    };
    let _ = terminal.backend_mut().write_raw(leave.as_bytes());
    exit
}

/// Runs the terminal UI on `terminal` with the keys of `input`, until the user quits, `input` ends or Kelpy stops.
/// With [`Screen::Inline`], `terminal` has an inline viewport.
pub async fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    input: impl Stream<Item = Event> + Unpin,
    api: Api,
    options: Options,
) -> io::Result<Exit>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let terminal_can = Capabilities {
        inline: options.screen == Screen::Inline,
        ..Capabilities::default()
    };
    run_on(terminal, input, api, options, terminal_can).await
}

async fn run_on<B: Backend>(
    terminal: &mut Terminal<B>,
    mut input: impl Stream<Item = Event> + Unpin,
    api: Api,
    options: Options,
    terminal_can: Capabilities,
) -> io::Result<Exit>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let mut app = App::new(options.banner, options.stops_runtime);
    app.inline = terminal_can.inline;
    app.resize(terminal.size().map_err(io::Error::other)?.width);
    let (updates, mut received) = mpsc::channel(UPDATES);
    let mut runner = Runner {
        api,
        updates,
        tasks: JoinSet::new(),
        following: Arc::default(),
    };
    runner.subscribe();
    let mut transcript = transcript::Transcript::default();
    let mut scrollback = inline::Scrollback::default();
    for effect in app.start() {
        runner.run(effect);
    }
    let exit = loop {
        if terminal_can.inline {
            let width = terminal.size().map_err(io::Error::other)?.width;
            let theme = terminal_can.theme;
            if let Some(written) = scrollback.take(&app, width, theme) {
                terminal
                    .insert_before(written.area.height, |buffer| buffer.merge(&written))
                    .map_err(io::Error::other)?;
            }
            let live = scrollback.live(&app, width, theme);
            terminal
                .draw(|frame| ui::draw_inline(frame, &app, terminal_can, live))
                .map_err(io::Error::other)?;
        } else {
            terminal
                .draw(|frame| ui::draw(frame, &app, &mut transcript, terminal_can))
                .map_err(io::Error::other)?;
        }
        let effects = tokio::select! {
            event = input.next() => match event {
                Some(Event::Key(key)) => app.key(key),
                Some(Event::Paste(text)) => {
                    app.paste(&text);
                    Vec::new()
                }
                Some(Event::Resize(columns, _)) => {
                    app.resize(columns);
                    Vec::new()
                }
                Some(_) => Vec::new(),
                None => break Exit::Quit,
            },
            Some(update) = received.recv() => {
                let mut effects = app.apply(update);
                // What else arrived meanwhile, such as the pieces of an answer, is drawn once.
                while let Ok(update) = received.try_recv() {
                    effects.extend(app.apply(update));
                }
                effects
            }
            // Each task is collected when it ends, so that none is kept once done.
            Some(ended) = runner.tasks.join_next() => match failure(ended) {
                Some(update) => app.apply(update),
                None => Vec::new(),
            },
        };
        if let Some(exit) = effects.iter().find_map(|effect| match effect {
            Effect::Exit(exit) => Some(*exit),
            _ => None,
        }) {
            break exit;
        }
        for effect in effects {
            runner.run(effect);
        }
    };
    if terminal_can.inline {
        leave_inline(terminal).map_err(io::Error::other)?;
    }
    Ok(exit)
}

/// Clears the footer drawn inline and leaves the cursor where it began, under what was written into the history.
fn leave_inline<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let top = terminal.get_frame().area().y;
    terminal.clear()?;
    terminal.set_cursor_position((0, top))?;
    terminal.show_cursor()
}

/// What to show of a task that ended: a failure, rather than losing it.
fn failure(ended: Result<(), JoinError>) -> Option<Update> {
    match ended {
        Err(error) if error.is_panic() => Some(Update::Failed(format!(
            "Part of the terminal UI failed: {error}"
        ))),
        _ => None,
    }
}

/// Runs the effects in the background, sending their results back as updates; the loop collects them as they end.
/// Dropped, it stops them all.
struct Runner {
    api: Api,
    updates: mpsc::Sender<Update>,
    tasks: JoinSet<()>,
    /// The conversations whose answer is being followed, so that none is followed twice.
    following: Arc<Mutex<HashSet<i64>>>,
}

impl Runner {
    fn run(&mut self, effect: Effect) {
        let (api, updates) = (self.api.clone(), self.updates.clone());
        match effect {
            Effect::LoadProvider => {
                self.spawn(async move { Ok(Update::Provider(api.provider().await?.state())) })
            }
            Effect::LoadSessions(listing) => self.spawn(async move {
                Ok(Update::Sessions {
                    listing,
                    sessions: api.sessions().await?,
                })
            }),
            Effect::LoadConnections => {
                self.spawn(async move { Ok(Update::Connections(api.connections().await?.waiting)) })
            }
            Effect::DecideConnection { id, allow } => {
                self.spawn_quiet(async move { api.decide_connection(id, allow).await })
            }
            Effect::Open { id, read, since } => self.spawn(async move {
                Ok(Update::Conversation {
                    id,
                    read,
                    since,
                    conversation: api.conversation(id).await?,
                })
            }),
            Effect::Follow(id) => {
                let Some(followed) = Followed::start(&self.following, id) else {
                    return;
                };
                self.tasks.spawn(async move {
                    let answer = api.follow(id).await;
                    forward(answer, id, &updates).await;
                    drop(followed);
                });
            }
            Effect::Send {
                session,
                content,
                modes,
            } => {
                let following = Arc::clone(&self.following);
                self.tasks.spawn(async move {
                    let id = match session {
                        Some(id) => id,
                        None => match api.create_session(&modes).await {
                            Ok(session) => {
                                let id = session.id;
                                let _ = updates.send(Update::Created(session)).await;
                                id
                            }
                            Err(error) => {
                                let _ = updates.send(Update::Failed(error.to_string())).await;
                                return;
                            }
                        },
                    };
                    // Followed by this stream: the notice of the answer's start must not open a second one.
                    let followed = Followed::start(&following, id);
                    let answer = api.send(id, &content).await;
                    forward(answer, id, &updates).await;
                    drop(followed);
                });
            }
            Effect::Decide {
                session,
                action,
                approve,
            } => self.spawn_quiet(async move { api.decide(session, action, approve).await }),
            Effect::Cancel(id) => self.spawn_quiet(async move { api.cancel(id).await }),
            Effect::Change { session, change } => self.spawn(async move {
                Ok(Update::Session(api.update_session(session, &change).await?))
            }),
            Effect::Exit(_) => {}
        }
    }

    /// Runs `work`, sending its update, or its error as [`Update::Failed`].
    fn spawn(&mut self, work: impl Future<Output = Result<Update, ClientError>> + Send + 'static) {
        let updates = self.updates.clone();
        self.tasks.spawn(async move {
            let update = work
                .await
                .unwrap_or_else(|error| Update::Failed(error.to_string()));
            let _ = updates.send(update).await;
        });
    }

    /// Runs `work`, whose success shows through the event channel: only an error is sent.
    fn spawn_quiet(
        &mut self,
        work: impl Future<Output = Result<(), ClientError>> + Send + 'static,
    ) {
        let updates = self.updates.clone();
        self.tasks.spawn(async move {
            if let Err(error) = work.await {
                let _ = updates.send(Update::Failed(error.to_string())).await;
            }
        });
    }

    /// Follows the runtime's event channel, connecting again when it drops, until Kelpy stops.
    fn subscribe(&mut self) {
        let (api, updates) = (self.api.clone(), self.updates.clone());
        self.tasks.spawn(async move {
            let mut first = true;
            loop {
                if let Ok(events) = api.subscribe().await {
                    if !first {
                        // Missed meanwhile: everything shown is read again.
                        let _ = updates.send(Update::Runtime(RuntimeEvent::Resync)).await;
                    }
                    first = false;
                    let mut events = std::pin::pin!(events);
                    while let Some(Ok(data)) = events.next().await {
                        let Some(event) = parse::<RuntimeEvent>(&data) else {
                            continue;
                        };
                        let stopping = matches!(event, RuntimeEvent::Stopping);
                        if updates.send(Update::Runtime(event)).await.is_err() || stopping {
                            return;
                        }
                    }
                }
                tokio::time::sleep(RECONNECT).await;
            }
        });
    }
}

/// A conversation whose answer is being followed, until dropped.
struct Followed {
    following: Arc<Mutex<HashSet<i64>>>,
    id: i64,
}

impl Followed {
    /// Marks `id` as followed, unless it is already.
    fn start(following: &Arc<Mutex<HashSet<i64>>>, id: i64) -> Option<Self> {
        lock(following).insert(id).then(|| Self {
            following: Arc::clone(following),
            id,
        })
    }
}

impl Drop for Followed {
    fn drop(&mut self) {
        lock(&self.following).remove(&self.id);
    }
}

/// Sends each event of an answer's stream as an update of conversation `id`.
async fn forward(
    answer: Result<impl Stream<Item = Result<String, ClientError>>, ClientError>,
    id: i64,
    updates: &mpsc::Sender<Update>,
) {
    let answer = match answer {
        Ok(answer) => answer,
        Err(error) => {
            let _ = updates.send(Update::Failed(error.to_string())).await;
            return;
        }
    };
    let mut answer = std::pin::pin!(answer);
    while let Some(data) = answer.next().await {
        let update = match data {
            Ok(data) => match parse::<ChatEvent>(&data) {
                Some(event) => Update::Chat { session: id, event },
                None => continue,
            },
            Err(error) => Update::Failed(error.to_string()),
        };
        if updates.send(update).await.is_err() {
            return;
        }
    }
}

fn parse<T: DeserializeOwned>(data: &str) -> Option<T> {
    serde_json::from_str(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_follows_the_background_the_terminal_tells_unless_the_user_says() {
        let solarized = [0xFD, 0xF6, 0xE3];
        let told = Background::Ask.theme(Colours::True, Some(solarized));
        assert!(!told.dark);
        assert_eq!(told.background, solarized);
        assert!(Background::Ask.theme(Colours::True, None).dark);
        assert!(Background::Dark.theme(Colours::True, Some(solarized)).dark);
        assert!(!Background::Light.theme(Colours::Indexed, None).dark);
    }

    #[tokio::test]
    async fn every_task_is_collected_and_one_that_failed_is_shown() {
        let mut tasks = JoinSet::new();
        for _ in 0..3 {
            tasks.spawn(async {});
        }
        tasks.spawn(async { panic!("the stream broke") });
        tasks.spawn(std::future::pending()).abort();
        let mut shown = Vec::new();
        while let Some(ended) = tasks.join_next().await {
            shown.extend(failure(ended));
        }
        assert!(tasks.is_empty());
        let [Update::Failed(message)] = &shown[..] else {
            panic!("{} updates", shown.len());
        };
        assert!(
            message.starts_with("Part of the terminal UI failed"),
            "{message}"
        );
        assert!(message.contains("the stream broke"), "{message}");
    }
}
