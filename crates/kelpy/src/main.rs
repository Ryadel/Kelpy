//! The `kelpy` executable: command line, first run and the local runtime.

mod cleanup;
mod duration;
mod elevate;
mod handover;
mod platform;
mod prompt;
mod runtime;
mod setup;
mod update;
mod wizard;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "kelpy",
    version,
    about = "AI-powered system administration. One binary. No installation required."
)]
struct Cli {
    /// Use this folder as the Kelpy home; starting Kelpy creates it if needed. Nothing is installed.
    #[arg(long, global = true, env = "KELPY_HOME", value_name = "PATH")]
    home: Option<PathBuf>,

    /// Accept every first-run default without asking.
    #[arg(long, short = 'y', global = true)]
    yes: bool,

    /// Do not open the Web UI in the browser: in a terminal, the terminal UI only.
    #[arg(long, global = true)]
    no_browser: bool,

    /// Draw the terminal UI inline: the conversation in the terminal's own history, only a footer redrawn. The
    /// default over SSH.
    #[arg(long, global = true, conflicts_with = "fullscreen")]
    inline: bool,

    /// Draw the terminal UI on the whole screen, also over SSH.
    #[arg(long, global = true)]
    fullscreen: bool,

    /// The terminal's background, for the terminal UI's colours, where the terminal does not tell it.
    #[arg(long, global = true, value_enum, value_name = "BACKGROUND")]
    theme: Option<Theme>,

    /// Also start Kelpy's own SSH server, on this port or one the system chooses: its terminal UI, SFTP to the
    /// uploaded files and forwarding to the Web UI, each connection allowed by you first. Without another limit, Kelpy
    /// stops after 1h without activity.
    #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "0")]
    ssh: Option<u16>,

    /// With --ssh, let the key with this fingerprint (SHA256:...) connect without asking, for unattended use.
    #[arg(long, value_name = "FINGERPRINT", requires = "ssh")]
    allow_key: Vec<String>,

    /// Stop after this time, e.g. 30m, 2h or 1h30m. Work in progress is allowed to finish.
    #[arg(long, value_name = "DURATION", value_parser = duration::parse)]
    shutdown_after: Option<Duration>,

    /// Stop after this long without activity, e.g. 15m. Work in progress always counts as activity.
    #[arg(long, value_name = "DURATION", value_parser = duration::parse)]
    shutdown_idle: Option<Duration>,

    #[command(subcommand)]
    command: Option<Command>,
}

/// A terminal's background, as `--theme` gives it.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Theme {
    Dark,
    Light,
}

#[derive(Debug, Clone, Subcommand)]
enum Command {
    /// Start or attach the terminal UI.
    Tui,
    /// Stop the Kelpy running for this home, for example from another terminal.
    Stop,
    /// Show whether Kelpy is running, with its PID, port and URL.
    Status,
    /// Open the Web UI in the browser.
    Open,
    /// Choose how Kelpy connects to AI: a ChatGPT plan, a free service, an API key or a local model.
    Provider(ProviderArgs),
    /// Remove Kelpy from this computer: its home with conversations and memory, the PATH entry, the user record.
    Cleanup,
    /// Install the latest version of Kelpy, after checking its signature.
    Update,
    /// Takes over from the Kelpy restarting as administrator, through its channel: started by it, not by hand.
    #[command(hide = true)]
    Handover { channel: String, from: u32 },
}

/// Without options, `kelpy provider` asks; with `--service`, it sets the provider without questions.
#[derive(Debug, Clone, clap::Args)]
struct ProviderArgs {
    /// The service, such as anthropic, groq or ollama.
    #[arg(long, requires = "model")]
    service: Option<String>,
    /// The model, such as claude-opus-5-5.
    #[arg(long, requires = "service")]
    model: Option<String>,
    /// The API address, for services without one.
    #[arg(long, requires = "service", value_name = "URL")]
    base_url: Option<String>,
    /// The environment variable holding the API key. Keys are never given on the command line.
    #[arg(long, requires = "service", value_name = "VARIABLE")]
    key_from: Option<String>,
    /// Write the ChatGPT agent registration to FILE, for another Kelpy. It holds no secrets.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["service", "import_agent"])]
    export_agent: Option<PathBuf>,
    /// Use the ChatGPT agent registration of another Kelpy from FILE: signing in as an agent then asks only for
    /// approval.
    #[arg(long, value_name = "FILE", conflicts_with = "service")]
    import_agent: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let options = setup::Options {
        explicit_home: cli.home.as_deref(),
        assume_yes: cli.yes,
        allow_first_run: matches!(cli.command, None | Some(Command::Tui)),
    };
    let (home, open_browser) = match setup::resolve(&options).await? {
        setup::Resolved::Ready { home, open_browser } => (home, open_browser),
        setup::Resolved::Delegated(code) => return Ok(code),
    };

    let screen = if cli.inline {
        kelpy_tui::Screen::Inline
    } else if cli.fullscreen {
        kelpy_tui::Screen::Full
    } else {
        kelpy_tui::Screen::Auto
    };
    let background = match cli.theme {
        None => kelpy_tui::Background::Ask,
        Some(Theme::Dark) => kelpy_tui::Background::Dark,
        Some(Theme::Light) => kelpy_tui::Background::Light,
    };
    let terminal = runtime::TerminalUi { screen, background };
    match cli.command {
        None => {
            if let Some(code) = update::at_start(&home, cli.yes).await {
                return Ok(code);
            }
            let mut limits = runtime::Limits {
                after: cli.shutdown_after,
                idle: cli.shutdown_idle,
            };
            // Reachable from outside, Kelpy never stays up unattended without an end.
            if cli.ssh.is_some() && limits.after.is_none() && limits.idle.is_none() {
                limits.idle = Some(runtime::EXPOSED_IDLE);
            }
            let launch = runtime::Launch {
                open_browser: open_browser && !cli.no_browser,
                terminal_ui: prompt::interactive(),
                terminal,
                ssh: cli.ssh.map(|port| runtime::Ssh {
                    port,
                    allowed_keys: cli.allow_key.clone(),
                }),
            };
            runtime::run_foreground(&home, launch, limits).await
        }
        Some(Command::Status) => runtime::status(&home),
        Some(Command::Open) => runtime::open(&home),
        Some(Command::Tui) => {
            runtime::terminal_ui(&home, open_browser && !cli.no_browser, terminal).await
        }
        Some(Command::Stop) => runtime::stop(&home).await,
        Some(Command::Cleanup) => cleanup::run(&home, cli.yes),
        Some(Command::Update) => update::command(&home, cli.yes).await,
        Some(Command::Provider(args)) => provider(&home, args).await,
        Some(Command::Handover { channel, from }) => {
            runtime::take_over(&home, &channel, from).await
        }
    }
}

async fn provider(home: &kelpy_core::home::Home, args: ProviderArgs) -> anyhow::Result<ExitCode> {
    let Some(client) = runtime::client(home)? else {
        println!("Kelpy is not running. Start it with `kelpy`, then run `kelpy provider`.");
        return Ok(ExitCode::from(1));
    };
    if let Some(path) = args.export_agent {
        wizard::export_agent(&client, &path).await?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(path) = args.import_agent {
        wizard::import_agent(&client, &path).await?;
        return Ok(ExitCode::SUCCESS);
    }
    match (args.service, args.model) {
        (Some(service), Some(model)) => {
            wizard::configure(&client, &service, &model, args.base_url, args.key_from).await?;
        }
        _ if prompt::interactive() => {
            tokio::task::spawn_blocking(move || wizard::run(client)).await??
        }
        _ => wizard::show(&client).await?,
    }
    Ok(ExitCode::SUCCESS)
}
