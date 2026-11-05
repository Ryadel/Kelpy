//! The `kelpy` executable: command line, first run and the local runtime.

mod cleanup;
mod client;
mod duration;
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

    /// Stop after this time, e.g. 30m, 2h or 1h30m. Work in progress is allowed to finish.
    #[arg(long, value_name = "DURATION", value_parser = duration::parse)]
    shutdown_after: Option<Duration>,

    /// Stop after this long without activity, e.g. 15m. Work in progress always counts as activity.
    #[arg(long, value_name = "DURATION", value_parser = duration::parse)]
    shutdown_idle: Option<Duration>,

    #[command(subcommand)]
    command: Option<Command>,
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
    /// Manage the configuration.
    Config,
    /// Send messages from the command line.
    Chat,
    /// Remove Kelpy from this computer: its home with conversations and memory, the PATH entry, the user record.
    Cleanup,
    /// Install the latest version of Kelpy, after checking its signature.
    Update,
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

    match cli.command {
        None => {
            if let Some(code) = update::at_start(&home, cli.yes).await {
                return Ok(code);
            }
            let limits = runtime::Limits {
                after: cli.shutdown_after,
                idle: cli.shutdown_idle,
            };
            runtime::run_foreground(&home, open_browser, limits).await
        }
        Some(Command::Status) => runtime::status(&home),
        Some(Command::Open) => runtime::open(&home),
        Some(Command::Tui) => not_available("The terminal UI"),
        Some(Command::Stop) => runtime::stop(&home).await,
        Some(Command::Cleanup) => cleanup::run(&home, cli.yes),
        Some(Command::Update) => update::command(&home, cli.yes).await,
        Some(Command::Provider(args)) => provider(&home, args).await,
        Some(Command::Config) => not_available("Configuration from the command line"),
        Some(Command::Chat) => not_available("Chat from the command line"),
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

fn not_available(feature: &str) -> anyhow::Result<ExitCode> {
    println!("{feature} is not available yet in this version of Kelpy.");
    Ok(ExitCode::from(2))
}
