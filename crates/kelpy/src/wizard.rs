//! The provider setup in the terminal: the questions of the Web UI's setup, asked of the running Kelpy, which keeps
//! the API key or the sign-in in memory.
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
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::runtime::Runtime;

use crate::prompt;
use kelpy_client::Client;

/// How long a sign-in with ChatGPT may take in the browser, as the runtime allows.
const SIGN_IN_TIME: Duration = Duration::from_secs(10 * 60);
/// The most models listed at once: a longer list is searched by part of the name instead.
const SHOWN_MODELS: usize = 20;

#[derive(Deserialize)]
struct ProviderInfo {
    configured: Option<Configured>,
    kinds: Vec<KindInfo>,
    ways: Vec<WayInfo>,
    services: Vec<Service>,
    sign_in_methods: Vec<SignInMethod>,
}

#[derive(Deserialize)]
struct SignInMethod {
    id: String,
    name: String,
    pros: String,
    cons: String,
    recommended: String,
}

#[derive(Deserialize)]
struct Configured {
    kind: String,
    model: String,
    service: Option<String>,
}

#[derive(Deserialize)]
struct KindInfo {
    id: String,
    default_base_url: Option<String>,
    key_variable: Option<String>,
    accepts_key: bool,
    requires_key: bool,
    key_in_environment: bool,
}

#[derive(Deserialize)]
struct WayInfo {
    id: String,
    name: String,
    description: String,
}

#[derive(Deserialize)]
struct Service {
    id: String,
    name: String,
    way: String,
    kind: String,
    base_url: Option<String>,
    description: String,
    key_page: Option<String>,
    steps: Vec<String>,
    models: Vec<String>,
}

/// The models a setup offers, as the runtime lists them.
#[derive(Deserialize)]
struct ModelList {
    models: Vec<ModelChoice>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ModelChoice {
    id: String,
    recommended: bool,
}

/// An answer to "Model:".
enum ModelOption {
    Model(String),
    /// Search the service's list, of this many models.
    Search(usize),
    Another,
}

#[derive(Deserialize)]
struct OllamaStatus {
    base_url: String,
    running: bool,
    error: Option<String>,
    installed: bool,
    models: Vec<LocalModel>,
    memory: Option<u64>,
    suggestions: Vec<Suggestion>,
}

/// How Ollama would be installed, as `POST /api/ollama/install` shows it.
#[derive(Deserialize)]
struct Installer {
    command: String,
    notes: Vec<String>,
}

/// An installation's progress.
#[derive(Deserialize)]
struct Installation {
    output: String,
    done: bool,
    error: Option<String>,
}

#[derive(Deserialize)]
struct LocalModel {
    name: String,
    size: u64,
    #[serde(default)]
    cloud: bool,
}

#[derive(Deserialize)]
struct Suggestion {
    name: String,
    size: u64,
    description: String,
}

#[derive(Deserialize)]
struct Download {
    status: String,
    completed: u64,
    total: u64,
    done: bool,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ChatGptStatus {
    signed_in: Option<Account>,
    registered: Option<String>,
    kept: bool,
}

#[derive(Deserialize)]
struct Account {
    email: Option<String>,
}

#[derive(Deserialize)]
struct ChatGptModel {
    slug: String,
    display_name: String,
}

/// What the setup chose.
struct Chosen {
    model: String,
    base_url: Option<String>,
    api_key: Option<String>,
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
    let info: ProviderInfo = client.get("/api/provider").await?;
    let Some(service) = info
        .services
        .iter()
        .find(|candidate| candidate.id == service)
    else {
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
    let info: ProviderInfo = client.get("/api/provider").await?;
    match info.configured {
        Some(configured) => println!(
            "{} ({})",
            configured.model,
            configured.service.unwrap_or(configured.kind)
        ),
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
    let _: serde_json::Value = client
        .send(
            Method::PUT,
            "/api/provider",
            &json!({
                "kind": service.kind,
                "service": service.id,
                "model": chosen.model,
                "base_url": chosen.base_url,
                "api_key": chosen.api_key,
            }),
        )
        .await?;
    println!("Provider saved: {} ({}).", chosen.model, service.name);
    Ok(())
}

struct Setup {
    client: Client,
    runtime: Runtime,
}

impl Setup {
    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self.runtime.block_on(self.client.get(path))?)
    }

    fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        Ok(self
            .runtime
            .block_on(self.client.send(Method::POST, path, body))?)
    }

    /// Forgets the agent registration: the next sign-in as an agent registers again, with any account.
    fn forget_registration(&self) -> Result<()> {
        self.runtime.block_on(
            self.client
                .delete::<serde::de::IgnoredAny>("/api/chatgpt/agent"),
        )?;
        Ok(())
    }

    fn run(&self) -> Result<()> {
        let info: ProviderInfo = self.get("/api/provider")?;
        if let Some(configured) = &info.configured {
            println!(
                "In use: {} ({}).",
                configured.model,
                configured.service.as_deref().unwrap_or(&configured.kind)
            );
        }
        println!();
        println!("How does Kelpy connect to AI?");
        let way = pick(&info.ways, |way| {
            (way.name.clone(), Some(way.description.clone()))
        })?;
        let services: Vec<&Service> = info
            .services
            .iter()
            .filter(|service| service.way == way.id)
            .collect();
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
            .kinds
            .iter()
            .find(|kind| kind.id == service.kind)
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
        self.runtime.block_on(save(&self.client, service, &chosen))
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

        let base_url = if service.base_url.is_none() && kind.default_base_url.is_none() {
            loop {
                let address = prompt::ask("API address, such as http://10.0.0.5:8000/v1", "")?;
                if address.starts_with("http://") || address.starts_with("https://") {
                    break Some(address);
                }
                println!("The address starts with http:// or https://.");
            }
        } else {
            None
        };

        let api_key = if kind.accepts_key {
            let from_environment = kind
                .key_variable
                .as_deref()
                .filter(|_| kind.key_in_environment);
            let question = match from_environment {
                Some(variable) => format!("API key, hidden; empty to use {variable}"),
                None if kind.requires_key || service.way != "local" => "API key, hidden".to_owned(),
                None => "API key, hidden; empty if the server needs none".to_owned(),
            };
            loop {
                let key = prompt::secret(&question)?;
                let needed =
                    from_environment.is_none() && (kind.requires_key || service.way == "free");
                if !key.is_empty() || !needed {
                    break Some(key).filter(|key| !key.is_empty());
                }
                println!("This service needs an API key.");
            }
        } else {
            None
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
        let query = json!({
            "kind": service.kind, "service": service.id, "base_url": base_url, "api_key": api_key,
        });
        let list = self
            .runtime
            .block_on(
                self.client
                    .send::<ModelList>(Method::POST, "/api/provider/models", &query),
            )
            .unwrap_or_else(|error| ModelList {
                models: service
                    .models
                    .iter()
                    .map(|id| ModelChoice {
                        id: id.clone(),
                        recommended: true,
                    })
                    .collect(),
                error: Some(error.to_string()),
            });
        if let Some(error) = &list.error {
            println!("The service's list is unavailable: {error}. Any model name can be typed.");
        }
        let (recommended, others): (Vec<ModelChoice>, Vec<ModelChoice>) = list
            .models
            .into_iter()
            .partition(|choice| choice.recommended);
        let others: Vec<String> = others.into_iter().map(|choice| choice.id).collect();
        let mut options: Vec<ModelOption> = recommended
            .into_iter()
            .map(|choice| ModelOption::Model(choice.id))
            .collect();
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
        let status: ChatGptStatus = self.get("/api/chatgpt")?;
        if let Some(account) = &status.signed_in {
            println!(
                "Signed in as {} {}.",
                account.email.as_deref().unwrap_or("your ChatGPT account"),
                lifetime(status.kept)
            );
        } else {
            println!("Sign in:");
            let method = pick(methods, |method| {
                let details = format!(
                    "Recommended {}\n     Pros. {}\n     Cons. {}",
                    method.recommended, method.pros, method.cons
                );
                (method.name.clone(), Some(details))
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
                    self.forget_registration()?;
                }
                self.sign_in(keep)?;
            }
        }
        let models: Vec<ChatGptModel> = self.get("/api/chatgpt/models")?;
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
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum CodeEvent {
            Code {
                verification_url: String,
                user_code: String,
            },
            SignedIn {
                account: Account,
            },
            Failed {
                error: String,
            },
        }

        let mut outcome = None;
        self.runtime
            .block_on(
                self.client
                    .each_event("/api/chatgpt/code", &json!({ "keep": keep }), |event| match event {
                        CodeEvent::Code {
                            verification_url,
                            user_code,
                        } => {
                            println!(
                                "On any device, open this address or scan the QR code, sign in to ChatGPT and enter \
                                 the code."
                            );
                            println!();
                            println!("{}", qr_code(&verification_url));
                            println!("  {verification_url}");
                            println!();
                            println!("  Code: {user_code}");
                            println!();
                            println!("The code expires in 15 minutes. Waiting for approval…");
                        }
                        CodeEvent::SignedIn { account } => outcome = Some(Ok(account)),
                        CodeEvent::Failed { error } => outcome = Some(Err(error)),
                    }),
            )?;
        match outcome {
            Some(Ok(account)) => {
                println!(
                    "Signed in as {}.",
                    account.email.as_deref().unwrap_or("your ChatGPT account")
                );
                Ok(())
            }
            Some(Err(error)) => bail!("{error}"),
            None => bail!("the sign-in ended without an outcome"),
        }
    }

    /// Signs in as an agent: the browser may be on another computer, so the user can also paste the address OpenAI
    /// sends it back to.
    fn sign_in(&self, keep: bool) -> Result<()> {
        #[derive(Deserialize)]
        struct Started {
            url: String,
        }

        let started: Started = self.post("/api/chatgpt/sign-in", &json!({ "keep": keep }))?;
        println!("Open this address in a browser and sign in:");
        println!();
        println!("  {}", started.url);
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
                        match self.finish_sign_in(&line) {
                            Ok(()) => return Ok(()),
                            Err(error) => println!("{error:#}"),
                        }
                    }
                    let _ = wanted.send(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let status: ChatGptStatus = self.get("/api/chatgpt")?;
                    if let Some(account) = status.signed_in {
                        println!();
                        println!(
                            "Signed in as {}. Press Enter to continue.",
                            account.email.as_deref().unwrap_or("your ChatGPT account")
                        );
                        // The line the reader is waiting for.
                        let _ = lines.recv();
                        return Ok(());
                    }
                    if Instant::now() > deadline {
                        bail!("the sign-in took longer than 10 minutes; run the setup again");
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the terminal closed"),
            }
        }
    }

    fn finish_sign_in(&self, address: &str) -> Result<()> {
        let address = reqwest::Url::parse(address.trim()).context("not a web address")?;
        let param = |name: &str| {
            address
                .query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        let account: Account = self.post(
            "/api/chatgpt/callback",
            &json!({
                "code": param("code"),
                "state": param("state"),
                "client_id": param("client_id"),
                "error": param("error"),
            }),
        )?;
        println!(
            "Signed in as {}.",
            account.email.as_deref().unwrap_or("your ChatGPT account")
        );
        Ok(())
    }

    /// Ollama: where it runs, its models, and a download if the model chosen is not there yet.
    fn ollama(&self, service: &Service) -> Result<Chosen> {
        let mut address = prompt::ask("Ollama address, empty for this computer", "")?;
        let status = loop {
            let query = if address.is_empty() {
                String::new()
            } else {
                format!("?base_url={}", form_encode(&address))
            };
            let status: OllamaStatus = self.get(&format!("/api/ollama{query}"))?;
            if status.running {
                break status;
            }
            if status.installed {
                println!(
                    "Ollama is installed but does not answer at {}. Start it.",
                    status.base_url
                );
            } else if status.memory.is_none() {
                println!(
                    "No Ollama answers at {}: {}",
                    status.base_url,
                    status.error.as_deref().unwrap_or("no details")
                );
            } else {
                println!("Ollama is not installed on this computer.");
                if self.install_ollama()? {
                    continue;
                }
                println!(
                    "Download it from https://ollama.com/download, or enter the address of a computer running it."
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

        let installed: Vec<(String, String)> = status
            .models
            .iter()
            .map(|model| {
                let detail = if model.cloud {
                    "runs on ollama.com".to_owned()
                } else {
                    gigabytes(model.size)
                };
                (model.name.clone(), detail)
            })
            .collect();
        let downloadable: Vec<&Suggestion> = status
            .suggestions
            .iter()
            .filter(|suggestion| installed.iter().all(|(name, _)| *name != suggestion.name))
            .collect();
        let mut options: Vec<(String, String)> = installed
            .iter()
            .map(|(name, size)| (format!("{name}  {size}"), String::new()))
            .collect();
        options.extend(downloadable.iter().map(|suggestion| {
            (
                format!(
                    "{}  download {}",
                    suggestion.name,
                    gigabytes(suggestion.size)
                ),
                suggestion.description.clone(),
            )
        }));
        if options.is_empty() {
            bail!("no models installed, and none suggested for this computer's memory");
        }
        println!();
        match status.memory {
            Some(memory) => println!("Model (this computer has {} of RAM):", gigabytes(memory)),
            None => println!("Model:"),
        }
        let index = pick_index(&options, 0, |(name, description)| {
            (
                name.clone(),
                Some(description.clone()).filter(|text| !text.is_empty()),
            )
        })?;
        let model = match index.checked_sub(installed.len()) {
            None => installed[index].0.clone(),
            Some(suggestion) => {
                let name = downloadable[suggestion].name.clone();
                self.download(&status.base_url, &name)?;
                name
            }
        };
        Ok(Chosen {
            model,
            base_url: Some(address).filter(|address| !address.is_empty()),
            api_key: None,
        })
    }

    /// Offers to install Ollama on this computer, showing the command first. Returns whether it was installed.
    fn install_ollama(&self) -> Result<bool> {
        let Ok(installer) =
            self.post::<Installer>("/api/ollama/install", &json!({ "preview": true }))
        else {
            return Ok(false);
        };
        println!("Kelpy can install it: {}", installer.command);
        for note in &installer.notes {
            println!("  {note}");
        }
        if !prompt::confirm("Install Ollama now?", false)? {
            return Ok(false);
        }
        let mut shown = 0;
        let mut failed = None;
        self.runtime.block_on(self.client.each_event(
            "/api/ollama/install",
            &json!({}),
            |installation: Installation| {
                // The output so far comes each time: only what is new is printed.
                if let Some(new) = installation.output.get(shown..) {
                    print!("{new}");
                    shown = installation.output.len();
                }
                if installation.done {
                    failed = installation.error;
                }
            },
        ))?;
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
        let mut failed = None;
        self.runtime.block_on(self.client.each_event(
            "/api/ollama/download",
            &json!({ "base_url": base_url, "model": model }),
            |download: Download| {
                let progress = if download.total > 0 {
                    format!(
                        "  {} of {}",
                        gigabytes(download.completed),
                        gigabytes(download.total)
                    )
                } else {
                    String::new()
                };
                print!(
                    "\r{:<72}",
                    format!("{model}: {}{progress}", download.status)
                );
                let _ = io::stdout().flush();
                if download.done {
                    println!();
                    failed = download.error;
                }
            },
        ))?;
        match failed {
            Some(error) => bail!("the download failed: {error}"),
            None => Ok(()),
        }
    }
}

/// Whether a ChatGPT session is saved on disk, after the account it is signed in as: if not, it ends when Kelpy stops.
pub(crate) fn lifetime(kept: bool) -> &'static str {
    if kept {
        "(saved - log out to clear)"
    } else {
        "(not saved - close to clear)"
    }
}

/// Whether the sign-in stays across restarts: off unless the user answers yes after the warning.
fn ask_keep() -> Result<bool> {
    println!(
        "Kelpy can save the sign-in on this computer, so that ChatGPT works again after a restart. It is then \
         saved in the Kelpy folder (data/), readable only by you: anyone who can read it as you or as an administrator \
         can use your ChatGPT plan until you sign out. Not recommended on a server you hand over to others."
    );
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

/// The models whose name contains `term`, ignoring case.
fn matching<'a>(models: &'a [String], term: &str) -> Vec<&'a String> {
    let term = term.to_lowercase();
    models
        .iter()
        .filter(|model| model.to_lowercase().contains(&term))
        .collect()
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

/// `text` as a QR code in text, light on dark as most terminals show it, with its quiet zone; empty if it does not fit
/// in one.
fn qr_code(text: &str) -> String {
    use qrcode::render::unicode::Dense1x2;
    qrcode::QrCode::new(text.as_bytes())
        .map(|code| {
            code.render::<Dense1x2>()
                .dark_color(Dense1x2::Light)
                .light_color(Dense1x2::Dark)
                .build()
        })
        .unwrap_or_default()
}

/// A size in bytes, in gigabytes with one decimal: `6.6 GB`.
fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// Encodes `text` for a URL's query.
fn form_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_are_searched_by_part_of_the_name_ignoring_case() {
        let models =
            ["openai/gpt-oss-120b", "qwen/Qwen3.8-27b", "google/gemma-4"].map(String::from);
        assert_eq!(matching(&models, "QWEN"), [&models[1]]);
        assert_eq!(matching(&models, "/g").len(), 2);
        assert!(matching(&models, "claude").is_empty());
    }

    #[test]
    fn addresses_are_encoded_for_the_query() {
        assert_eq!(
            form_encode("http://10.0.0.5:11434/v1"),
            "http%3A%2F%2F10.0.0.5%3A11434%2Fv1"
        );
    }

    #[test]
    fn qr_codes_are_drawn_as_square_text() {
        let qr = qr_code("https://auth.openai.com/codex/device");
        let lines: Vec<usize> = qr.lines().map(|line| line.chars().count()).collect();
        assert!(lines.len() > 10);
        // Two modules per line: as many lines as half the width, rounded up.
        assert!(lines.iter().all(|width| *width == lines[0]));
        assert_eq!(lines.len(), lines[0].div_ceil(2));
    }

    #[test]
    fn sizes_are_shown_in_gigabytes() {
        assert_eq!(gigabytes(6_600_000_000), "6.6 GB");
    }
}
