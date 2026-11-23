//! The provider setup in the terminal: the questions of the Web UI's setup, asked of the running Kelpy, which keeps
//! the API key or the sign-in in memory. The questions, their order and the requests are shared with the terminal
//! UI's setup window ([`kelpy_client::setup`]); this module asks them as lines in a terminal.
//!
//! The questions block on the terminal, so the setup runs on a thread of its own, with a small runtime for its
//! requests.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
pub(crate) use kelpy_client::setup::lifetime;
use kelpy_client::setup::{
    self, Chosen, CodeEvent, Download, Installation, KEEP_WARNING, KindInfo, OLLAMA_DOWNLOAD,
    OllamaProblem, ProviderInfo, SHOWN_MODELS, SIGN_IN_MINUTES, Service, SignInMethod, gigabytes,
    key_question, matching, model_options, ollama_options, qr_code,
};
use kelpy_client::{Client, Method};
use serde::{Deserialize, Serialize};

use crate::prompt;

/// A QR code in white and black where the terminal shows colours: in its own colours, a light terminal would
/// invert it, and some scanners would not read it. A line of upper halves only is the margin's last half row, with
/// the terminal under it.
fn in_black_and_white(code: &str) -> String {
    if !crate::platform::colors() {
        return code.to_owned();
    }
    code.lines()
        .map(|line| {
            let background = if line.chars().all(|char| char == '▀') {
                ""
            } else {
                ";40"
            };
            format!("\x1b[97{background}m{line}\x1b[0m")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How long a sign-in as an agent may take in the browser, as the runtime allows.
const SIGN_IN_TIME: Duration = Duration::from_secs(SIGN_IN_MINUTES * 60);

/// An answer to "Model:".
enum ModelOption {
    Model(String),
    /// Search the service's list, of this many models.
    Search(usize),
    Another,
}

/// Asks whether to set up the provider now, at a start without one, and runs the setup if so.
pub fn offer(client: Client) -> Result<()> {
    println!();
    if prompt::confirm(
        "No AI provider is set up. Set it up here instead of in the Web UI?",
        true,
    )? {
        run(client)?;
    }
    Ok(())
}

/// Runs the whole setup.
pub fn run(client: Client) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the setup")?;
    Setup { client, runtime }.run()
}

/// Sets the provider without questions, for scripts. The API key comes from the environment variable `key_from`,
/// never from the command line, where other users and the shell history could see it.
pub async fn configure(
    client: &Client,
    service: &str,
    model: &str,
    base_url: Option<String>,
    key_from: Option<String>,
) -> Result<()> {
    let info = client.provider_info().await?;
    let Some(service) = info.service(service) else {
        let known: Vec<&str> = info
            .services
            .iter()
            .map(|known| known.id.as_str())
            .collect();
        bail!("unknown service {service}; known: {}", known.join(", "));
    };
    let api_key = match key_from {
        Some(variable) => {
            Some(std::env::var(&variable).with_context(|| format!("{variable} is not set"))?)
        }
        None => None,
    };
    let chosen = Chosen {
        model: model.to_owned(),
        base_url,
        api_key,
    };
    save(client, service, &chosen).await
}

/// Prints the provider in use.
pub async fn show(client: &Client) -> Result<()> {
    match client.provider_info().await?.in_use() {
        Some(in_use) => println!("{in_use}"),
        None => {
            println!("No AI provider is set up. Run `kelpy provider` in a terminal to set one up.")
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct AgentFile {
    text: String,
}

/// Writes the ChatGPT agent registration to `path`, for another Kelpy.
pub async fn export_agent(client: &Client, path: &Path) -> Result<()> {
    let file: AgentFile = client.get("/api/chatgpt/agent").await?;
    fs::write(path, file.text).with_context(|| format!("cannot write {}", path.display()))?;
    println!("Agent registration written to {}.", path.display());
    Ok(())
}

/// Uses the ChatGPT agent registration of another Kelpy, read from `path`.
pub async fn import_agent(client: &Client, path: &Path) -> Result<()> {
    #[derive(Deserialize)]
    struct Registered {
        registered: Option<String>,
    }

    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let used: Registered = client
        .send(Method::PUT, "/api/chatgpt/agent", &AgentFile { text })
        .await?;
    println!(
        "Agent registration of {} in use. Signing in as an agent asks only for approval.",
        used.registered.as_deref().unwrap_or("the account")
    );
    Ok(())
}

async fn save(client: &Client, service: &Service, chosen: &Chosen) -> Result<()> {
    client.save_provider(&chosen.request(service)).await?;
    println!("Provider saved: {} ({}).", chosen.model, service.name);
    Ok(())
}

struct Setup {
    client: Client,
    runtime: tokio::runtime::Runtime,
}

impl Setup {
    fn wait<T>(&self, request: impl Future<Output = T>) -> T {
        self.runtime.block_on(request)
    }

    fn run(&self) -> Result<()> {
        let info: ProviderInfo = self.wait(self.client.provider_info())?;
        if let Some(in_use) = info.in_use() {
            println!("In use: {in_use}.");
        }
        println!();
        println!("How does Kelpy connect to AI?");
        let way = pick(&info.ways, |way| {
            (way.name.clone(), Some(way.description.clone()))
        })?;
        let services = info.services_of(&way.id);
        let service = if services.len() == 1 {
            services[0]
        } else {
            println!();
            println!("Service:");
            *pick(&services, |service| {
                (service.name.clone(), Some(service.description.clone()))
            })?
        };
        let kind = info
            .kind_of(service)
            .context("the runtime did not describe this kind of provider")?;
        println!();
        let chosen = match service.kind.as_str() {
            "chatgpt" => {
                let current = info
                    .configured
                    .as_ref()
                    .filter(|configured| configured.kind == "chatgpt")
                    .map(|configured| configured.model.as_str());
                self.chatgpt(&info.sign_in_methods, current)?
            }
            "ollama" => self.ollama(service)?,
            _ => self.keyed(service, kind)?,
        };
        self.wait(save(&self.client, service, &chosen))
    }

    /// A service with an API key: the steps to get it, the address if the user gives it, the key and the model.
    fn keyed(&self, service: &Service, kind: &KindInfo) -> Result<Chosen> {
        if !service.steps.is_empty() {
            println!("To get a key:");
            for (number, step) in service.steps.iter().enumerate() {
                println!("  {}. {step}", number + 1);
            }
        }
        if let Some(page) = &service.key_page {
            println!("Key page: {page}");
        }
        println!();

        let base_url = if setup::asks_address(service, kind) {
            loop {
                let address = prompt::ask("API address, such as http://10.0.0.5:8000/v1", "")?;
                if setup::is_address(&address) {
                    break Some(address);
                }
                println!("The address starts with http:// or https://.");
            }
        } else {
            None
        };

        let api_key = match key_question(service, kind) {
            Some(question) => {
                let asked = match &question.note {
                    Some(note) => format!("API key, hidden; {note}"),
                    None => "API key, hidden".to_owned(),
                };
                loop {
                    let key = prompt::secret(&asked)?;
                    if !key.is_empty() || !question.needed {
                        break Some(key).filter(|key| !key.is_empty());
                    }
                    println!("This service needs an API key.");
                }
            }
            None => None,
        };

        let model = self.choose_model(service, base_url.as_deref(), api_key.as_deref())?;
        Ok(Chosen {
            model,
            base_url,
            api_key,
        })
    }

    /// The model: the recommended ones first, then the others the service lists, searched by part of the name when
    /// they are many; any name can be typed.
    fn choose_model(
        &self,
        service: &Service,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<String> {
        let list = self.wait(self.client.models(service, base_url, api_key));
        if let Some(error) = &list.error {
            println!("The service's list is unavailable: {error}. Any model name can be typed.");
        }
        let (recommended, others) = model_options(list);
        let mut options: Vec<ModelOption> =
            recommended.into_iter().map(ModelOption::Model).collect();
        if others.len() <= SHOWN_MODELS {
            options.extend(others.iter().cloned().map(ModelOption::Model));
        } else {
            options.push(ModelOption::Search(others.len()));
        }
        options.push(ModelOption::Another);
        if options.len() == 1 {
            return type_model();
        }
        println!("Model:");
        let index = pick_index(&options, 0, |option| {
            let name = match option {
                ModelOption::Model(id) => id.clone(),
                ModelOption::Search(count) => {
                    format!("Search the {count} models the service lists")
                }
                ModelOption::Another => "Another model".to_owned(),
            };
            (name, None)
        })?;
        match &options[index] {
            ModelOption::Model(id) => Ok(id.clone()),
            ModelOption::Search(_) => search_model(&others),
            ModelOption::Another => type_model(),
        }
    }

    /// A ChatGPT plan: the sign-in, unless a session is open, and the model, `current` proposed first.
    fn chatgpt(&self, methods: &[SignInMethod], current: Option<&str>) -> Result<Chosen> {
        let status = self.wait(self.client.chatgpt_status())?;
        if let Some(account) = &status.signed_in {
            println!("Signed in as {} {}.", account.name(), lifetime(status.kept));
        } else {
            println!("Sign in:");
            let method = pick(methods, |method| {
                (method.name.clone(), Some(method.details().join("\n     ")))
            })?;
            println!();
            let keep = ask_keep()?;
            println!();
            if method.id == "codex" {
                self.sign_in_with_code(keep)?;
            } else {
                if let Some(email) = &status.registered
                    && !prompt::confirm(
                        &format!("Use the agent registered in {email}'s ChatGPT account?"),
                        true,
                    )?
                {
                    self.wait(self.client.forget_registration())?;
                }
                self.sign_in(keep)?;
            }
        }
        let models = self.wait(self.client.chatgpt_models())?;
        if models.is_empty() {
            bail!("the ChatGPT account offers no models");
        }
        println!();
        println!("Model:");
        let proposed = models
            .iter()
            .position(|model| Some(model.slug.as_str()) == current)
            .unwrap_or(0);
        let model = &models[pick_index(&models, proposed, |model| {
            (model.display_name.clone(), None)
        })?];
        Ok(Chosen {
            model: model.slug.clone(),
            base_url: None,
            api_key: None,
        })
    }

    /// Signs in as the Codex CLI: shows the address, a QR code of it and the code, and waits until the code is
    /// approved on any device.
    fn sign_in_with_code(&self, keep: bool) -> Result<()> {
        let account = self.wait(async {
            let events = self.client.sign_in_with_code(keep).await?;
            let mut events = std::pin::pin!(events);
            while let Some(data) = events.next().await {
                match serde_json::from_str::<CodeEvent>(&data?)? {
                    CodeEvent::Code {
                        verification_url,
                        user_code,
                    } => {
                        println!(
                            "On any device, open this address or scan the QR code, sign in to ChatGPT and enter \
                             the code."
                        );
                        println!();
                        println!("{}", in_black_and_white(&qr_code(&verification_url)));
                        println!("  {verification_url}");
                        println!();
                        println!("  Code: {user_code}");
                        println!();
                        println!("The code expires in 15 minutes. Waiting for approval…");
                    }
                    CodeEvent::SignedIn { account } => return Ok(account),
                    CodeEvent::Failed { error } => bail!("{error}"),
                }
            }
            bail!("the sign-in ended without an outcome")
        })?;
        println!("Signed in as {}.", account.name());
        Ok(())
    }

    /// Signs in as an agent: the browser may be on another computer, so the user can also paste the address OpenAI
    /// sends it back to.
    fn sign_in(&self, keep: bool) -> Result<()> {
        let url = self.wait(self.client.start_sign_in(keep))?;
        println!("Open this address in a browser and sign in:");
        println!();
        println!("  {url}");
        println!();
        println!(
            "OpenAI then sends the browser to a 127.0.0.1 address. If Kelpy's page opens, sign in there if asked; the \
             sign-in completes by itself. If the page cannot be reached, copy its address from the address bar and \
             paste it here."
        );

        // Lines are read one at a time, on request, so none is taken from the questions that follow.
        let (wanted, want) = mpsc::channel::<()>();
        let (send_line, lines) = mpsc::channel::<io::Result<String>>();
        thread::spawn(move || {
            while want.recv().is_ok() {
                if send_line.send(prompt::read_line("Address: ")).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + SIGN_IN_TIME;
        let _ = wanted.send(());
        loop {
            match lines.recv_timeout(Duration::from_secs(2)) {
                Ok(line) => {
                    let line = line?;
                    if !line.is_empty() {
                        match self.wait(self.client.finish_sign_in(&line)) {
                            Ok(account) => {
                                println!("Signed in as {}.", account.name());
                                return Ok(());
                            }
                            Err(error) => println!("{error}"),
                        }
                    }
                    let _ = wanted.send(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let status = self.wait(self.client.chatgpt_status())?;
                    if let Some(account) = status.signed_in {
                        println!();
                        println!("Signed in as {}. Press Enter to continue.", account.name());
                        // The line the reader is waiting for.
                        let _ = lines.recv();
                        return Ok(());
                    }
                    if Instant::now() > deadline {
                        bail!(
                            "the sign-in took longer than {SIGN_IN_MINUTES} minutes; run the setup again"
                        );
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the terminal closed"),
            }
        }
    }

    /// Ollama: where it runs, its models, and a download if the model chosen is not there yet.
    fn ollama(&self, service: &Service) -> Result<Chosen> {
        let mut address = prompt::ask("Ollama address, empty for this computer", "")?;
        let status = loop {
            let status = self.wait(self.client.ollama(&address))?;
            let Some(problem) = status.problem() else {
                break status;
            };
            println!("{}", status.explain().unwrap_or_default());
            if problem == OllamaProblem::NotInstalled {
                if self.install_ollama()? {
                    continue;
                }
                println!(
                    "Download it from {OLLAMA_DOWNLOAD}, or enter the address of a computer running it."
                );
            }
            if !prompt::confirm("Check again?", true)? {
                bail!("no model chosen");
            }
            address = prompt::ask("Ollama address, empty for this computer", &address)?;
        };
        for step in &service.steps {
            println!("{step}");
        }

        let options = ollama_options(&status);
        if options.is_empty() {
            bail!("no models installed, and none suggested for this computer's memory");
        }
        println!();
        match status.memory {
            Some(memory) => println!("Model (this computer has {} of RAM):", gigabytes(memory)),
            None => println!("Model:"),
        }
        let index = pick_index(&options, 0, |option| {
            (
                format!("{}  {}", option.name, option.detail),
                option.description.clone(),
            )
        })?;
        let option = &options[index];
        if option.download {
            self.download(&status.base_url, &option.name)?;
        }
        Ok(Chosen {
            model: option.name.clone(),
            base_url: Some(address).filter(|address| !address.is_empty()),
            api_key: None,
        })
    }

    /// Offers to install Ollama on this computer, showing the command first. Returns whether it was installed.
    fn install_ollama(&self) -> Result<bool> {
        let Ok(installer) = self.wait(self.client.ollama_installer()) else {
            return Ok(false);
        };
        println!("Kelpy can install it: {}", installer.command);
        for note in &installer.notes {
            println!("  {note}");
        }
        if !prompt::confirm("Install Ollama now?", false)? {
            return Ok(false);
        }
        let failed = self.wait(async {
            let events = self.client.install_ollama().await?;
            let mut events = std::pin::pin!(events);
            let mut shown = 0;
            while let Some(data) = events.next().await {
                let installation: Installation = serde_json::from_str(&data?)?;
                // The output so far comes each time: only what is new is printed.
                if let Some(new) = installation.output.get(shown..) {
                    print!("{new}");
                    shown = installation.output.len();
                }
                if installation.done {
                    return anyhow::Ok(installation.error);
                }
            }
            Ok(Some("the installation ended without an outcome".to_owned()))
        })?;
        match failed {
            None => {
                println!("Ollama is installed and answers.");
                Ok(true)
            }
            Some(error) => {
                println!("The installation did not complete: {error}");
                Ok(false)
            }
        }
    }

    fn download(&self, base_url: &str, model: &str) -> Result<()> {
        let failed = self.wait(async {
            let events = self.client.download_model(base_url, model).await?;
            let mut events = std::pin::pin!(events);
            while let Some(data) = events.next().await {
                let download: Download = serde_json::from_str(&data?)?;
                print!("\r{:<72}", format!("{model}: {}", download.progress()));
                let _ = io::stdout().flush();
                if download.done {
                    println!();
                    return anyhow::Ok(download.error);
                }
            }
            Ok(Some("the download ended without an outcome".to_owned()))
        })?;
        match failed {
            Some(error) => bail!("the download failed: {error}"),
            None => Ok(()),
        }
    }
}

/// Whether the sign-in stays across restarts: off unless the user answers yes after the warning.
fn ask_keep() -> Result<bool> {
    println!("{KEEP_WARNING}");
    Ok(prompt::confirm(
        "Save the sign-in on this computer?",
        false,
    )?)
}

/// A model name, typed.
fn type_model() -> Result<String> {
    loop {
        let model = prompt::ask("Model", "")?;
        if !model.is_empty() {
            return Ok(model);
        }
    }
}

/// A model of `models`, found by part of its name.
fn search_model(models: &[String]) -> Result<String> {
    loop {
        let term = prompt::ask("Part of the model's name", "")?;
        let found = matching(models, &term);
        if found.is_empty() {
            println!("No model name contains {term:?}.");
            continue;
        }
        if found.len() > SHOWN_MODELS {
            println!(
                "{} models match: showing the first {SHOWN_MODELS}.",
                found.len()
            );
        }
        let found = &found[..found.len().min(SHOWN_MODELS)];
        let mut options: Vec<Option<&String>> = found.iter().copied().map(Some).collect();
        options.push(None);
        let index = pick_index(&options, 0, |option| match option {
            Some(model) => ((*model).clone(), None),
            None => ("Search again".to_owned(), None),
        })?;
        if let Some(model) = options[index] {
            return Ok(model.clone());
        }
    }
}

/// Lists `items` numbered, with a description under each if any, and returns the one chosen.
fn pick<T>(items: &[T], show: impl Fn(&T) -> (String, Option<String>)) -> io::Result<&T> {
    Ok(&items[pick_index(items, 0, show)?])
}

/// Lists `items` and returns the index chosen; Enter alone chooses `proposed`.
fn pick_index<T>(
    items: &[T],
    proposed: usize,
    show: impl Fn(&T) -> (String, Option<String>),
) -> io::Result<usize> {
    for (number, item) in items.iter().enumerate() {
        let (name, description) = show(item);
        println!("  {}. {name}", number + 1);
        if let Some(description) = description {
            println!("     {description}");
        }
    }
    Ok(prompt::choose(items.len(), proposed + 1)? - 1)
}
