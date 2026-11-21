//! The setup window: how Kelpy connects to AI, chosen inside the terminal UI, with the questions of `kelpy provider`
//! in the same order ([`kelpy_client::setup`]). Over Kelpy's own SSH server it is the only way to sign in or enter a
//! key, as there is no shell there and the Web UI is elsewhere.
//!
//! Like [`crate::app`], it is pure: what needs the runtime goes out as a [`Call`], its answer comes back as a
//! [`Reply`]. Each step has a ticket, raised at every change of step, so that an answer for a step left behind is
//! dropped. The API key typed is shown as dots, and goes nowhere but to the runtime with the setup's requests.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kelpy_client::setup::{
    self, Account, ChatGptModel, ChatGptStatus, Chosen, Download, Installation, Installer,
    KEEP_WARNING, KeyQuestion, ModelList, OLLAMA_DOWNLOAD, OllamaOption, OllamaProblem,
    OllamaStatus, ProviderInfo, Service, gigabytes, lifetime, matching, model_options,
    ollama_options,
};

/// What the setup asks of the runtime, for the step of its ticket.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    /// The models `service` offers, with the address and key entered for it.
    Models {
        service: Box<Service>,
        base_url: Option<String>,
        api_key: Option<String>,
    },
    ChatGptStatus,
    /// Signs in as the Codex CLI: the code, then the outcome, for as long as the step lasts.
    SignInWithCode {
        keep: bool,
    },
    /// Signs in as an agent, after forgetting the registration if asked: the address to open, then the outcome once
    /// the browser comes back to Kelpy.
    StartSignIn {
        keep: bool,
        forget: bool,
    },
    /// Completes the sign-in as an agent with the address the browser was sent back to.
    FinishSignIn(String),
    ChatGptModels,
    Ollama(String),
    Installer,
    Install,
    Download {
        base_url: String,
        model: String,
    },
    /// Saves the provider: the body of `PUT /api/provider`.
    Save(serde_json::Value),
    /// Closes the window in a moment, unless something else happened.
    CloseLater,
    /// Stops following what the step followed: a sign-in waiting is abandoned; a download or an installation goes
    /// on in Kelpy.
    Stop,
    /// Closes the window.
    Close,
}

/// What the runtime answered.
#[derive(Debug, Clone)]
pub enum Reply {
    Models(ModelList),
    ChatGpt(ChatGptStatus),
    Code { url: String, code: String },
    SignInAddress(String),
    SignedIn(Account),
    ChatGptModels(Vec<ChatGptModel>),
    Ollama(OllamaStatus),
    Installer(Result<Installer, String>),
    Installation(Installation),
    Download(Download),
    Saved,
    Close,
    Failed(String),
}

/// What the provider lacks to answer: the setup opens by itself for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lack {
    Provider,
    SignIn,
    Key,
}

/// What the provider lacks, if anything.
pub fn lack(info: &ProviderInfo) -> Option<Lack> {
    if info.configured.is_none() {
        return Some(Lack::Provider);
    }
    match info.needs.as_deref() {
        Some("sign_in") => Some(Lack::SignIn),
        Some("key") => Some(Lack::Key),
        _ => None,
    }
}

#[derive(Debug, Clone)]
enum Step {
    /// Until the runtime describes the choices.
    Loading,
    Way {
        cursor: usize,
    },
    Service {
        way: String,
        cursor: usize,
    },
    Address {
        service: Service,
        field: String,
    },
    Key {
        service: Service,
        base_url: Option<String>,
        question: KeyQuestion,
        field: String,
    },
    Models {
        service: Service,
        base_url: Option<String>,
        api_key: Option<String>,
        /// The recommended models, the others, and why the service's list is missing; none while it is read.
        list: Option<(Vec<String>, Vec<String>, Option<String>)>,
        search: String,
        cursor: usize,
    },
    /// Whether a ChatGPT session is open.
    ChatGpt,
    SignInMethod {
        cursor: usize,
    },
    Keep {
        codex: bool,
        save: bool,
    },
    Registered {
        keep: bool,
        email: String,
        use_it: bool,
    },
    Code {
        code: Option<(String, String)>,
    },
    Agent {
        url: Option<String>,
        field: String,
    },
    ChatGptModels {
        /// `Signed in as …`.
        account: String,
        list: Option<Vec<ChatGptModel>>,
        cursor: usize,
    },
    Ollama {
        service: Service,
        field: String,
        /// The address checked, as typed then: the status and the installer are for it, not for the field.
        checked: Option<String>,
        checking: bool,
        status: Option<OllamaStatus>,
        installer: Option<Result<Installer, String>>,
        /// 0 installs, 1 checks again, when Kelpy can install Ollama.
        cursor: usize,
    },
    Installing {
        service: Service,
        address: String,
        output: String,
        failed: Option<String>,
    },
    OllamaModels {
        service: Service,
        address: String,
        status: OllamaStatus,
        cursor: usize,
    },
    Downloading {
        service: Service,
        chosen: Chosen,
        progress: Option<Download>,
        failed: Option<String>,
    },
    Saving {
        label: String,
    },
    Saved {
        label: String,
    },
}

impl Step {
    /// Whether Esc comes back to it: a question, not a wait.
    fn kept(&self) -> bool {
        !matches!(
            self,
            Self::Loading
                | Self::ChatGpt
                | Self::Code { .. }
                | Self::Agent { .. }
                | Self::Installing { .. }
                | Self::Downloading { .. }
                | Self::Saving { .. }
                | Self::Saved { .. }
        )
    }
}

/// The setup window's state.
pub struct Setup {
    info: ProviderInfo,
    step: Step,
    /// The steps Esc goes back to, the last first.
    back: Vec<Step>,
    /// Raised at each change of step: answers for another ticket are dropped.
    pub ticket: u64,
    /// What went wrong, until the next key.
    pub error: Option<String>,
    /// The ChatGPT session, once read.
    chatgpt: Option<ChatGptStatus>,
}

impl Setup {
    /// The setup from its first question; reading the choices first without `info`.
    pub fn new(info: Option<ProviderInfo>) -> Self {
        let mut setup = Self {
            info: ProviderInfo::default(),
            step: Step::Loading,
            back: Vec::new(),
            ticket: 0,
            error: None,
            chatgpt: None,
        };
        if let Some(info) = info {
            setup.loaded(info);
        }
        setup
    }

    /// The setup opened by itself for what the provider lacks: from the first question without one, at the sign-in
    /// or the key of the one in use otherwise. Returns its first calls.
    pub fn lacking(info: ProviderInfo, lack: Lack) -> (Self, Vec<Call>) {
        let mut setup = Self::new(Some(info));
        let Some(service) = setup.info.configured_service().cloned() else {
            return (setup, Vec::new());
        };
        let calls = match lack {
            Lack::Provider => Vec::new(),
            Lack::SignIn => setup.choose_service(service),
            Lack::Key => {
                // The way and the service chosen before, so that Esc goes back to them.
                setup.choose_way(&service.way);
                let base_url = setup
                    .info
                    .configured
                    .as_ref()
                    .and_then(|configured| configured.base_url.clone());
                match setup.info.kind_of(&service).cloned() {
                    Some(kind) => match setup::key_question(&service, &kind) {
                        Some(question) => {
                            setup.to(Step::Key {
                                service,
                                base_url,
                                question,
                                field: String::new(),
                            });
                            Vec::new()
                        }
                        None => setup.choose_service(service),
                    },
                    None => Vec::new(),
                }
            }
        };
        (setup, calls)
    }

    /// The choices are read: the first question, the way in use proposed.
    pub fn loaded(&mut self, info: ProviderInfo) {
        if !matches!(self.step, Step::Loading) {
            return;
        }
        let cursor = info
            .configured_service()
            .and_then(|service| info.ways.iter().position(|way| way.id == service.way))
            .unwrap_or(0);
        self.info = info;
        self.step = Step::Way { cursor };
        self.ticket += 1;
    }

    /// The address to open in a browser, for Ctrl+O, while the step shows one.
    pub fn link(&self) -> Option<&str> {
        match &self.step {
            Step::Code {
                code: Some((url, _)),
            } => Some(url),
            Step::Agent { url: Some(url), .. } => Some(url),
            _ => None,
        }
    }

    /// Goes to `step`, keeping the current one for Esc if it is a question.
    fn to(&mut self, step: Step) {
        let left = std::mem::replace(&mut self.step, step);
        if left.kept() {
            self.back.push(left);
        }
        self.ticket += 1;
    }

    /// Esc: back one step, or closes from the first.
    fn back(&mut self) -> Vec<Call> {
        match self.back.pop() {
            Some(step) => {
                self.step = step;
                self.ticket += 1;
                vec![Call::Stop]
            }
            None => vec![Call::Close],
        }
    }

    /// Text pasted goes to the field of the step, if it has one.
    pub fn paste(&mut self, text: &str) {
        let text = text.trim_end_matches(['\r', '\n']);
        if let Some(field) = self.field() {
            field.push_str(&text.replace(['\r', '\n'], ""));
        }
        self.field_changed();
    }

    /// The field was changed: a search starts from its first match, and what was checked of another Ollama
    /// address is dropped, an answer still on its way included.
    fn field_changed(&mut self) {
        match &mut self.step {
            Step::Models { cursor, .. } => *cursor = 0,
            Step::Ollama {
                field,
                checked,
                checking,
                status,
                installer,
                cursor,
                ..
            } if *checking
                || checked
                    .as_deref()
                    .is_some_and(|address| address != field.trim()) =>
            {
                *checked = None;
                *checking = false;
                *status = None;
                *installer = None;
                *cursor = 0;
                self.ticket += 1;
            }
            _ => {}
        }
    }

    fn field(&mut self) -> Option<&mut String> {
        match &mut self.step {
            Step::Address { field, .. }
            | Step::Key { field, .. }
            | Step::Agent { field, .. }
            | Step::Ollama { field, .. } => Some(field),
            Step::Models { search, .. } => Some(search),
            _ => None,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Vec<Call> {
        self.error = None;
        if let Step::Saved { .. } = self.step {
            return vec![Call::Close];
        }
        if key.code == KeyCode::Esc {
            return self.back();
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        // Typing goes to the field, where the step has one.
        if let Some(field) = self.field() {
            match key.code {
                KeyCode::Char('u') if control => field.clear(),
                KeyCode::Char(char) if !control => field.push(char),
                KeyCode::Backspace => {
                    field.pop();
                }
                _ => return self.step_key(key.code),
            }
            self.field_changed();
            return Vec::new();
        }
        self.step_key(key.code)
    }

    /// The keys that choose and confirm.
    fn step_key(&mut self, code: KeyCode) -> Vec<Call> {
        let count = self.choices();
        if let Some(cursor) = self.cursor() {
            match code {
                KeyCode::Up => *cursor = cursor.saturating_sub(1),
                KeyCode::Down => *cursor = (*cursor + 1).min(count.saturating_sub(1)),
                KeyCode::Home => *cursor = 0,
                KeyCode::End => *cursor = count.saturating_sub(1),
                KeyCode::PageUp => *cursor = cursor.saturating_sub(10),
                KeyCode::PageDown => *cursor = (*cursor + 10).min(count.saturating_sub(1)),
                _ => {}
            }
        }
        match (&mut self.step, code) {
            (
                Step::Keep { save: flag, .. } | Step::Registered { use_it: flag, .. },
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab,
            ) => {
                *flag = !*flag;
                Vec::new()
            }
            (_, KeyCode::Enter) => self.confirm(),
            _ => Vec::new(),
        }
    }

    /// How many choices the step lists.
    fn choices(&self) -> usize {
        match &self.step {
            Step::Way { .. } => self.info.ways.len(),
            Step::Service { way, .. } => self.info.services_of(way).len(),
            Step::Models { .. } => self.models().len(),
            Step::SignInMethod { .. } => self.info.sign_in_methods.len(),
            Step::ChatGptModels { list, .. } => list.as_ref().map_or(0, Vec::len),
            Step::Ollama { installer, .. } => {
                if matches!(installer, Some(Ok(_))) {
                    2
                } else {
                    0
                }
            }
            Step::OllamaModels { status, .. } => ollama_options(status).len(),
            _ => 0,
        }
    }

    fn cursor(&mut self) -> Option<&mut usize> {
        match &mut self.step {
            Step::Way { cursor }
            | Step::Service { cursor, .. }
            | Step::Models { cursor, .. }
            | Step::SignInMethod { cursor }
            | Step::ChatGptModels { cursor, .. }
            | Step::Ollama { cursor, .. }
            | Step::OllamaModels { cursor, .. } => Some(cursor),
            _ => None,
        }
    }

    /// The models of the Models step that match what is typed, then the name typed, if it is not one of them.
    fn models(&self) -> Vec<ModelChoice> {
        let Step::Models { list, search, .. } = &self.step else {
            return Vec::new();
        };
        let mut choices: Vec<ModelChoice> = match list {
            Some((recommended, others, _)) => {
                let all: Vec<String> = recommended.iter().chain(others).cloned().collect();
                matching(&all, search.trim())
                    .into_iter()
                    .map(|model| ModelChoice::Listed(model.clone()))
                    .collect()
            }
            None => Vec::new(),
        };
        let typed = search.trim();
        if !typed.is_empty()
            && !choices
                .iter()
                .any(|choice| matches!(choice, ModelChoice::Listed(model) if model == typed))
        {
            choices.push(ModelChoice::Typed(typed.to_owned()));
        }
        choices
    }

    /// Enter.
    fn confirm(&mut self) -> Vec<Call> {
        match self.step.clone() {
            Step::Way { cursor } => {
                let Some(way) = self.info.ways.get(cursor).map(|way| way.id.clone()) else {
                    return Vec::new();
                };
                let services: Vec<Service> =
                    self.info.services_of(&way).into_iter().cloned().collect();
                match &services[..] {
                    [] => Vec::new(),
                    [service] => self.choose_service(service.clone()),
                    _ => {
                        let cursor = self.proposed_service(&services);
                        self.to(Step::Service { way, cursor });
                        Vec::new()
                    }
                }
            }
            Step::Service { way, cursor } => {
                let service = self
                    .info
                    .services_of(&way)
                    .get(cursor)
                    .map(|s| (*s).clone());
                service.map_or_else(Vec::new, |service| self.choose_service(service))
            }
            Step::Address { service, field } => {
                let address = field.trim().to_owned();
                if !setup::is_address(&address) {
                    self.error = Some("The address starts with http:// or https://.".into());
                    return Vec::new();
                }
                self.ask_key(service, Some(address))
            }
            Step::Key {
                service,
                base_url,
                question,
                field,
            } => {
                let key = field.trim().to_owned();
                if key.is_empty() && question.needed {
                    self.error = Some("This service needs an API key.".into());
                    return Vec::new();
                }
                self.ask_model(service, base_url, Some(key).filter(|key| !key.is_empty()))
            }
            Step::Models {
                service,
                base_url,
                api_key,
                cursor,
                ..
            } => {
                let choices = self.models();
                let Some(choice) = choices.get(cursor.min(choices.len().saturating_sub(1))) else {
                    self.error = Some("Type the model's name.".into());
                    return Vec::new();
                };
                let model = match choice {
                    ModelChoice::Listed(model) | ModelChoice::Typed(model) => model.clone(),
                };
                self.save(
                    &service,
                    Chosen {
                        model,
                        base_url,
                        api_key,
                    },
                )
            }
            Step::SignInMethod { cursor } => {
                let Some(method) = self.info.sign_in_methods.get(cursor) else {
                    return Vec::new();
                };
                let codex = method.id == "codex";
                self.to(Step::Keep { codex, save: false });
                Vec::new()
            }
            Step::Keep { codex: true, save } => {
                self.to(Step::Code { code: None });
                vec![Call::SignInWithCode { keep: save }]
            }
            Step::Keep { codex: false, save } => {
                let registered = self
                    .chatgpt
                    .as_ref()
                    .and_then(|status| status.registered.clone());
                match registered {
                    Some(email) => {
                        self.to(Step::Registered {
                            keep: save,
                            email,
                            use_it: true,
                        });
                        Vec::new()
                    }
                    None => self.start_sign_in(save, false),
                }
            }
            Step::Registered { keep, use_it, .. } => self.start_sign_in(keep, !use_it),
            Step::Agent { field, .. } => {
                let address = field.trim().to_owned();
                if address.is_empty() {
                    return Vec::new();
                }
                if let Step::Agent { field, .. } = &mut self.step {
                    field.clear();
                }
                vec![Call::FinishSignIn(address)]
            }
            Step::ChatGptModels { list, cursor, .. } => {
                let Some(model) = list.as_ref().and_then(|list| list.get(cursor)) else {
                    return Vec::new();
                };
                let model = model.slug.clone();
                let Some(service) = self.info.services_of("plan").first().map(|s| (*s).clone())
                else {
                    return Vec::new();
                };
                self.save(
                    &service,
                    Chosen {
                        model,
                        base_url: None,
                        api_key: None,
                    },
                )
            }
            Step::Ollama {
                field,
                checked,
                checking,
                installer,
                cursor,
                service,
                ..
            } => {
                if checking {
                    return Vec::new();
                }
                if matches!(installer, Some(Ok(_))) && cursor == 0 {
                    self.to(Step::Installing {
                        service,
                        address: checked.unwrap_or_else(|| field.trim().to_owned()),
                        output: String::new(),
                        failed: None,
                    });
                    return vec![Call::Install];
                }
                self.check_ollama()
            }
            Step::OllamaModels {
                service,
                address,
                status,
                cursor,
            } => {
                let options = ollama_options(&status);
                let Some(option) = options.get(cursor) else {
                    return Vec::new();
                };
                let chosen = Chosen {
                    model: option.name.clone(),
                    base_url: Some(address).filter(|address| !address.is_empty()),
                    api_key: None,
                };
                if option.download {
                    let model = chosen.model.clone();
                    self.to(Step::Downloading {
                        service,
                        chosen,
                        progress: None,
                        failed: None,
                    });
                    return vec![Call::Download {
                        base_url: status.base_url.clone(),
                        model,
                    }];
                }
                self.save(&service, chosen)
            }
            _ => Vec::new(),
        }
    }

    /// The service of the way in use, if it is among `services`.
    fn proposed_service(&self, services: &[Service]) -> usize {
        self.info
            .configured_service()
            .and_then(|current| services.iter().position(|service| service == current))
            .unwrap_or(0)
    }

    /// The way `id` chosen, and its service list if it has several: the steps before a service's own questions.
    fn choose_way(&mut self, id: &str) {
        let services: Vec<Service> = self.info.services_of(id).into_iter().cloned().collect();
        if let Step::Way { cursor } = &mut self.step
            && let Some(at) = self.info.ways.iter().position(|way| way.id == id)
        {
            *cursor = at;
        }
        if services.len() > 1 {
            let cursor = self.proposed_service(&services);
            self.to(Step::Service {
                way: id.to_owned(),
                cursor,
            });
        }
    }

    /// The questions of `service`, after its way and its place in the list.
    fn choose_service(&mut self, service: Service) -> Vec<Call> {
        if matches!(self.step, Step::Way { .. }) && self.info.services_of(&service.way).len() > 1 {
            self.choose_way(&service.way);
        }
        match service.kind.as_str() {
            "chatgpt" => {
                self.to(Step::ChatGpt);
                vec![Call::ChatGptStatus]
            }
            "ollama" => {
                self.to(Step::Ollama {
                    service,
                    field: String::new(),
                    checked: None,
                    checking: false,
                    status: None,
                    installer: None,
                    cursor: 0,
                });
                Vec::new()
            }
            _ => {
                let Some(kind) = self.info.kind_of(&service) else {
                    self.error = Some("Kelpy did not describe this kind of provider.".into());
                    return Vec::new();
                };
                if setup::asks_address(&service, kind) {
                    let field = self
                        .info
                        .configured
                        .as_ref()
                        .filter(|configured| configured.service.as_deref() == Some(&service.id))
                        .and_then(|configured| configured.base_url.clone())
                        .unwrap_or_default();
                    self.to(Step::Address { service, field });
                    Vec::new()
                } else {
                    self.ask_key(service, None)
                }
            }
        }
    }

    fn ask_key(&mut self, service: Service, base_url: Option<String>) -> Vec<Call> {
        let question = self
            .info
            .kind_of(&service)
            .and_then(|kind| setup::key_question(&service, kind));
        match question {
            Some(question) => {
                self.to(Step::Key {
                    service,
                    base_url,
                    question,
                    field: String::new(),
                });
                Vec::new()
            }
            None => self.ask_model(service, base_url, None),
        }
    }

    fn ask_model(
        &mut self,
        service: Service,
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> Vec<Call> {
        let call = Call::Models {
            service: Box::new(service.clone()),
            base_url: base_url.clone(),
            api_key: api_key.clone(),
        };
        self.to(Step::Models {
            service,
            base_url,
            api_key,
            list: None,
            search: String::new(),
            cursor: 0,
        });
        vec![call]
    }

    fn start_sign_in(&mut self, keep: bool, forget: bool) -> Vec<Call> {
        self.to(Step::Agent {
            url: None,
            field: String::new(),
        });
        vec![Call::StartSignIn { keep, forget }]
    }

    fn check_ollama(&mut self) -> Vec<Call> {
        let Step::Ollama {
            field,
            checked,
            checking,
            status,
            installer,
            cursor,
            ..
        } = &mut self.step
        else {
            return Vec::new();
        };
        let address = field.trim().to_owned();
        *checked = Some(address.clone());
        *checking = true;
        *status = None;
        *installer = None;
        *cursor = 0;
        vec![Call::Ollama(address)]
    }

    fn signed_in(&mut self, account: &Account, keep: bool) -> Vec<Call> {
        self.to(Step::ChatGptModels {
            account: format!("Signed in as {} {}", account.name(), lifetime(keep)),
            list: None,
            cursor: 0,
        });
        vec![Call::ChatGptModels]
    }

    fn save(&mut self, service: &Service, chosen: Chosen) -> Vec<Call> {
        let label = format!("{} ({})", chosen.model, service.name);
        let request = chosen.request(service);
        self.to(Step::Saving { label });
        vec![Call::Save(request)]
    }

    /// The runtime's answer for the current step.
    pub fn reply(&mut self, reply: Reply) -> Vec<Call> {
        let current_model = self
            .info
            .configured
            .as_ref()
            .map(|configured| configured.model.clone());
        match (&mut self.step, reply) {
            (Step::Models { list, cursor, .. }, Reply::Models(models)) => {
                let error = models.error.clone();
                let (recommended, others) = model_options(models);
                *cursor = recommended
                    .iter()
                    .chain(&others)
                    .position(|model| Some(model) == current_model.as_ref())
                    .unwrap_or(0);
                *list = Some((recommended, others, error));
            }
            (Step::ChatGpt, Reply::ChatGpt(status)) => {
                let signed_in = status.signed_in.clone();
                let kept = status.kept;
                self.chatgpt = Some(status);
                return match signed_in {
                    Some(account) => self.signed_in(&account, kept),
                    None => {
                        self.to(Step::SignInMethod { cursor: 0 });
                        Vec::new()
                    }
                };
            }
            (Step::Code { code }, Reply::Code { url, code: shown }) => *code = Some((url, shown)),
            (Step::Agent { url, .. }, Reply::SignInAddress(address)) => *url = Some(address),
            (Step::Code { .. } | Step::Agent { .. }, Reply::SignedIn(account)) => {
                let keep = self.keep();
                return self.signed_in(&account, keep);
            }
            (Step::ChatGptModels { list, cursor, .. }, Reply::ChatGptModels(models)) => {
                if models.is_empty() {
                    self.error = Some("The ChatGPT account offers no models.".into());
                }
                *cursor = models
                    .iter()
                    .position(|model| Some(&model.slug) == current_model.as_ref())
                    .unwrap_or(0);
                *list = Some(models);
            }
            (
                Step::Ollama {
                    service,
                    checked,
                    checking,
                    status,
                    ..
                },
                Reply::Ollama(found),
            ) => {
                *checking = false;
                if found.running {
                    let (service, address) = (service.clone(), checked.clone().unwrap_or_default());
                    self.to(Step::OllamaModels {
                        service,
                        address,
                        status: found,
                        cursor: 0,
                    });
                    return Vec::new();
                }
                let missing = found.problem() == Some(OllamaProblem::NotInstalled);
                *status = Some(found);
                if missing {
                    return vec![Call::Installer];
                }
            }
            (Step::Ollama { installer, .. }, Reply::Installer(found)) => *installer = Some(found),
            (Step::Installing { output, failed, .. }, Reply::Installation(installation)) => {
                *output = installation.output;
                if installation.done {
                    match installation.error {
                        Some(error) => *failed = Some(error),
                        None => return vec![Call::Ollama(self.installed_address())],
                    }
                }
            }
            (
                Step::Installing {
                    service,
                    address,
                    failed,
                    ..
                },
                Reply::Ollama(found),
            ) => {
                if found.running {
                    let (service, address) = (service.clone(), address.clone());
                    self.to(Step::OllamaModels {
                        service,
                        address,
                        status: found,
                        cursor: 0,
                    });
                } else {
                    *failed = found.explain();
                }
            }
            (
                Step::Downloading {
                    service,
                    chosen,
                    progress,
                    failed,
                },
                Reply::Download(download),
            ) => {
                let done = download.done;
                if let Some(error) = &download.error {
                    *failed = Some(error.clone());
                }
                *progress = Some(download);
                if done && failed.is_none() {
                    let (service, chosen) = (service.clone(), chosen.clone());
                    return self.save(&service, chosen);
                }
            }
            (Step::Saving { label }, Reply::Saved) => {
                let label = label.clone();
                self.to(Step::Saved { label });
                return vec![Call::CloseLater];
            }
            (Step::Saved { .. }, Reply::Close) => return vec![Call::Close],
            (Step::Saving { .. }, Reply::Failed(error)) => {
                // Back to the last question, to change what was refused.
                self.error = Some(error);
                if let Some(step) = self.back.pop() {
                    self.step = step;
                    self.ticket += 1;
                }
            }
            (Step::Ollama { checking, .. }, Reply::Failed(error)) => {
                *checking = false;
                self.error = Some(error);
            }
            (Step::Installing { failed, .. }, Reply::Failed(error))
            | (Step::Downloading { failed, .. }, Reply::Failed(error)) => *failed = Some(error),
            (_, Reply::Failed(error)) => self.error = Some(error),
            _ => {}
        }
        Vec::new()
    }

    fn installed_address(&self) -> String {
        match &self.step {
            Step::Installing { address, .. } => address.clone(),
            _ => String::new(),
        }
    }

    /// Whether the sign-in under way is saved, as chosen.
    fn keep(&self) -> bool {
        self.back.iter().rev().find_map(|step| match step {
            Step::Keep { save, .. } => Some(*save),
            _ => None,
        }) == Some(true)
    }

    /// What the window shows. `links`: Ctrl+O opens addresses in this computer's browser.
    pub fn view(&self, links: bool) -> View {
        let mut view = View::default();
        let service_title = |service: &Service, what: &str| format!("{} · {what}", service.name);
        match &self.step {
            Step::Loading => {
                view.title = "Connect to AI".into();
                view.controls
                    .push(Control::Para(Para::dim("Reading what Kelpy offers…")));
                view.keys = "Esc close".into();
            }
            Step::Way { cursor } => {
                view.title = "How does Kelpy connect to AI?".into();
                view.text.push(match self.info.in_use() {
                    Some(in_use) => Para::dim(format!("In use: {in_use}.")),
                    None => Para::dim("In use: none. Kelpy cannot answer until one is chosen."),
                });
                for (index, way) in self.info.ways.iter().enumerate() {
                    view.controls.push(Control::Choice {
                        name: way.name.clone(),
                        detail: None,
                        description: Some(way.description.clone()),
                        selected: index == *cursor,
                    });
                }
                view.keys = "↑↓ choose · Enter next · Esc close".into();
            }
            Step::Service { way, cursor } => {
                let name = self
                    .info
                    .ways
                    .iter()
                    .find(|known| known.id == *way)
                    .map_or("Service", |known| known.name.as_str());
                view.title = format!("{name} · Service");
                for (index, service) in self.info.services_of(way).into_iter().enumerate() {
                    view.controls.push(Control::Choice {
                        name: service.name.clone(),
                        detail: None,
                        description: Some(service.description.clone()),
                        selected: index == *cursor,
                    });
                }
                view.keys = "↑↓ choose · Enter next · Esc back".into();
            }
            Step::Address { service, field } => {
                view.title = service_title(service, "Address");
                view.text.push(Para::text(service.description.clone()));
                view.controls.push(Control::Field {
                    label: "API address".into(),
                    note: Some(", such as http://10.0.0.5:8000/v1".into()),
                    shown: field.clone(),
                    search: false,
                });
                view.keys = "Enter next · Esc back".into();
            }
            Step::Key {
                service,
                question,
                field,
                ..
            } => {
                view.title = service_title(service, "Key");
                if !service.steps.is_empty() {
                    view.text.push(Para::strong("To get a key"));
                    for (number, step) in service.steps.iter().enumerate() {
                        view.text.push(Para::numbered(number + 1, step.clone()));
                    }
                }
                if let Some(page) = &service.key_page {
                    view.text.push(Para::link("Key page  ", page.clone()));
                }
                let note = question
                    .note
                    .as_ref()
                    .map_or(", hidden".to_owned(), |note| format!(", hidden; {note}"));
                view.controls.push(Control::Field {
                    label: "API key".into(),
                    note: Some(note),
                    // Never shown: as many dots as characters.
                    shown: "•".repeat(field.chars().count()),
                    search: false,
                });
                view.controls.push(Control::Para(Para::dim(
                    "Never shown, not kept in the message history. Paste it with the terminal's paste keys.",
                )));
                view.keys = "Enter next · Esc back".into();
            }
            Step::Models {
                service,
                list,
                search,
                cursor,
                ..
            } => {
                view.title = service_title(service, "Model");
                let note = match list {
                    None => "Reading the service's models…".to_owned(),
                    Some((_, _, Some(error))) => format!(
                        "The service's list is unavailable: {error}. Any model name can be typed."
                    ),
                    Some((recommended, others, None)) => format!(
                        "{} models the service lists. Type to search.",
                        recommended.len() + others.len()
                    ),
                };
                view.controls.push(Control::Para(Para::dim(note)));
                view.controls.push(Control::Field {
                    label: String::new(),
                    note: None,
                    shown: search.clone(),
                    search: true,
                });
                let recommended = list
                    .as_ref()
                    .map_or(&[][..], |(recommended, ..)| recommended);
                let choices = self.models();
                let selected = (*cursor).min(choices.len().saturating_sub(1));
                for (index, choice) in choices.iter().enumerate() {
                    let (name, detail) = match choice {
                        ModelChoice::Listed(model) => (
                            model.clone(),
                            recommended
                                .contains(model)
                                .then(|| "recommended".to_owned()),
                        ),
                        ModelChoice::Typed(model) => (format!("Use \"{model}\" as typed"), None),
                    };
                    view.controls.push(Control::Choice {
                        name,
                        detail,
                        description: None,
                        selected: index == selected,
                    });
                }
                view.keys = "↑↓ choose · Enter save · Esc back".into();
            }
            Step::ChatGpt => {
                view.title = "ChatGPT plan".into();
                view.controls
                    .push(Control::Para(Para::dim("Checking the ChatGPT sign-in…")));
                view.keys = "Esc back".into();
            }
            Step::SignInMethod { cursor } => {
                view.title = "ChatGPT plan · Sign in".into();
                for (index, method) in self.info.sign_in_methods.iter().enumerate() {
                    view.controls.push(Control::Choice {
                        name: method.name.clone(),
                        detail: None,
                        description: Some(method.details().join("\n")),
                        selected: index == *cursor,
                    });
                }
                view.keys = "↑↓ choose · Enter next · Esc back".into();
            }
            Step::Keep { save, .. } => {
                view.title = "ChatGPT plan · Save the sign-in?".into();
                view.text.push(Para::text(KEEP_WARNING));
                view.controls.push(Control::Buttons(vec![
                    ("Save".into(), *save),
                    ("Do not save".into(), !*save),
                ]));
                view.keys = "←→ choose · Enter next · Esc back".into();
            }
            Step::Registered { email, use_it, .. } => {
                view.title = "ChatGPT plan · Agent".into();
                view.text.push(Para::text(format!(
                    "Kelpy is registered as an agent in {email}'s ChatGPT account. Registering again lets another \
                     account approve it."
                )));
                view.controls.push(Control::Buttons(vec![
                    ("Use it".into(), *use_it),
                    ("Register again".into(), !*use_it),
                ]));
                view.keys = "←→ choose · Enter next · Esc back".into();
            }
            Step::Code { code } => {
                view.title = "ChatGPT plan · Approve the code".into();
                match code {
                    None => view
                        .controls
                        .push(Control::Para(Para::dim("Asking OpenAI for a code…"))),
                    Some((url, code)) => {
                        view.text.push(Para::text(
                            "On any device, open this address or scan the QR code, sign in to ChatGPT and enter the \
                             code.",
                        ));
                        view.text.push(Para::qr(setup::qr_code(url)));
                        view.text.push(Para::link("", url.clone()));
                        view.text.push(Para::labelled("Code  ", code.clone()));
                        view.controls.push(Control::Para(Para::dim(
                            "Waiting for approval · the code expires in 15 minutes",
                        )));
                    }
                }
                view.keys = if links && code.is_some() {
                    "Ctrl+O open the address · Esc cancel".into()
                } else {
                    "Esc cancel".into()
                };
            }
            Step::Agent { url, field } => {
                view.title = "ChatGPT plan · Sign in as an agent".into();
                match url {
                    None => view
                        .controls
                        .push(Control::Para(Para::dim("Asking OpenAI for the address…"))),
                    Some(url) => {
                        view.text
                            .push(Para::text("Open this address in a browser and sign in:"));
                        view.text.push(Para::link("", url.clone()));
                        view.text.push(Para::text(
                            "OpenAI then sends the browser to a 127.0.0.1 address. If Kelpy's page opens, sign in \
                             there if asked; the sign-in completes by itself. If the page cannot be reached, copy its \
                             address from the address bar and paste it here.",
                        ));
                        view.controls.push(Control::Field {
                            label: "Address".into(),
                            note: Some(", if the page cannot be reached".into()),
                            shown: field.clone(),
                            search: false,
                        });
                        view.controls
                            .push(Control::Para(Para::dim("Waiting for the sign-in…")));
                    }
                }
                view.keys = if links && url.is_some() {
                    "Enter use the address · Ctrl+O open · Esc cancel".into()
                } else {
                    "Enter use the address · Esc cancel".into()
                };
            }
            Step::ChatGptModels {
                account,
                list,
                cursor,
            } => {
                view.title = "ChatGPT plan · Model".into();
                view.controls
                    .push(Control::Para(Para::good(format!("✓ {account}"))));
                match list {
                    None => view
                        .controls
                        .push(Control::Para(Para::dim("Reading the account's models…"))),
                    Some(models) => {
                        for (index, model) in models.iter().enumerate() {
                            view.controls.push(Control::Choice {
                                name: model.display_name.clone(),
                                detail: None,
                                description: None,
                                selected: index == *cursor,
                            });
                        }
                    }
                }
                view.keys = "↑↓ choose · Enter save · Esc back".into();
            }
            Step::Ollama {
                field,
                checking,
                status,
                installer,
                cursor,
                ..
            } => {
                view.title = "Local model · Ollama".into();
                view.controls.push(Control::Field {
                    label: "Ollama address".into(),
                    note: Some(", empty for this computer".into()),
                    shown: field.clone(),
                    search: false,
                });
                if *checking {
                    view.controls.push(Control::Para(Para::dim("Checking…")));
                }
                if let Some(problem) = status.as_ref().and_then(OllamaStatus::explain) {
                    view.controls
                        .push(Control::Para(Para::warn(format!("! {problem}"))));
                }
                let missing = status.as_ref().and_then(OllamaStatus::problem)
                    == Some(OllamaProblem::NotInstalled);
                match installer {
                    Some(Ok(installer)) => {
                        view.controls
                            .push(Control::Para(Para::text("Kelpy can install it:")));
                        view.controls
                            .push(Control::Para(Para::dim(format!("  {}", installer.command))));
                        for note in &installer.notes {
                            view.controls
                                .push(Control::Para(Para::dim(format!("  {note}"))));
                        }
                        for (index, name) in ["Install Ollama", "Check again"].iter().enumerate() {
                            view.controls.push(Control::Choice {
                                name: (*name).to_owned(),
                                detail: None,
                                description: None,
                                selected: index == *cursor,
                            });
                        }
                    }
                    _ if missing => view.controls.push(Control::Para(Para::text(format!(
                        "Download it from {OLLAMA_DOWNLOAD}, or enter the address of a computer running it."
                    )))),
                    _ => {}
                }
                view.keys = if matches!(installer, Some(Ok(_))) {
                    "↑↓ choose · Enter confirm · Esc back".into()
                } else {
                    "Enter check · Esc back".into()
                };
            }
            Step::Installing { output, failed, .. } => {
                view.title = "Local model · Installing Ollama".into();
                let lines: Vec<&str> = output.lines().collect();
                for line in &lines[lines.len().saturating_sub(8)..] {
                    view.controls.push(Control::Para(Para::dim(*line)));
                }
                view.controls.push(Control::Para(match failed {
                    Some(error) => Para::bad(format!("The installation did not complete: {error}")),
                    None => Para::text("Installing…"),
                }));
                view.keys = if failed.is_some() {
                    "Esc back".into()
                } else {
                    "Esc hide · the installation goes on".into()
                };
            }
            Step::OllamaModels {
                service,
                status,
                cursor,
                ..
            } => {
                view.title = "Local model · Model".into();
                for step in &service.steps {
                    view.text.push(Para::text(step.clone()));
                }
                view.controls.push(Control::Para(Para::good(format!(
                    "✓ Ollama answers at {}.",
                    status.base_url
                ))));
                let options: Vec<OllamaOption> = ollama_options(status);
                view.controls.push(Control::Para(match status.memory {
                    Some(memory) => Para::dim(format!(
                        "Model (this computer has {} of RAM)",
                        gigabytes(memory)
                    )),
                    None => Para::dim("Model"),
                }));
                if options.is_empty() {
                    view.controls.push(Control::Para(Para::warn(
                        "No models installed, and none suggested for this computer's memory.",
                    )));
                }
                for (index, option) in options.iter().enumerate() {
                    view.controls.push(Control::Choice {
                        name: option.name.clone(),
                        detail: Some(option.detail.clone()),
                        description: option.description.clone(),
                        selected: index == *cursor,
                    });
                }
                let download = options.get(*cursor).is_some_and(|option| option.download);
                view.keys = if download {
                    "↑↓ choose · Enter download · Esc back".into()
                } else {
                    "↑↓ choose · Enter save · Esc back".into()
                };
            }
            Step::Downloading {
                chosen,
                progress,
                failed,
                ..
            } => {
                view.title = format!("Local model · Downloading {}", chosen.model);
                let shown = progress
                    .as_ref()
                    .map_or("starting".to_owned(), Download::progress);
                view.controls.push(Control::Para(Para::text(format!(
                    "{}: {shown}",
                    chosen.model
                ))));
                if let Some(progress) = progress.as_ref().filter(|progress| progress.total > 0) {
                    view.controls.push(Control::Bar(
                        progress.completed as f64 / progress.total as f64,
                    ));
                }
                if let Some(error) = failed {
                    view.controls.push(Control::Para(Para::bad(format!(
                        "The download failed: {error}"
                    ))));
                }
                view.keys = if failed.is_some() {
                    "Esc back".into()
                } else {
                    "Esc hide · the download goes on".into()
                };
            }
            Step::Saving { label } => {
                view.title = "Connect to AI".into();
                view.controls
                    .push(Control::Para(Para::dim(format!("Saving {label}…"))));
            }
            Step::Saved { label } => {
                view.title = "Connected".into();
                view.controls.push(Control::Para(Para::good(format!(
                    "✓ Provider saved: {label}."
                ))));
                view.keys = "Any key closes".into();
            }
        }
        if let Some(error) = &self.error {
            view.controls.push(Control::Para(Para::bad(error.clone())));
        }
        view
    }
}

/// A model to choose in the Models step.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelChoice {
    Listed(String),
    /// A name not listed, as typed.
    Typed(String),
}

/// What the setup window shows, for the full screen and inline alike.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    pub title: String,
    /// What the step explains, which does not change while it lasts: inline it goes into the terminal's history.
    pub text: Vec<Para>,
    /// What is chosen, typed, or under way.
    pub controls: Vec<Control>,
    pub keys: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Choice {
        name: String,
        /// Beside the name, dim: a size, `recommended`.
        detail: Option<String>,
        /// Under the name; lines apart with `\n`.
        description: Option<String>,
        selected: bool,
    },
    Field {
        /// `API key`; empty for a search.
        label: String,
        /// After the label, dim: `, hidden`.
        note: Option<String>,
        /// What it shows of the value typed: the value, or dots for a key.
        shown: String,
        search: bool,
    },
    /// Buttons side by side, the one chosen lit.
    Buttons(Vec<(String, bool)>),
    /// How far a download is, from 0 to 1.
    Bar(f64),
    Para(Para),
}

/// A paragraph, wrapped to the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Para {
    pub tone: Tone,
    /// Before the text, on its first line, dim: `1 `, `Key page  `.
    pub lead: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Text,
    Dim,
    Strong,
    Link,
    Good,
    Warn,
    Bad,
    /// A QR code: drawn as it is, never wrapped, left out where it does not fit.
    Qr,
}

impl Para {
    fn new(tone: Tone, lead: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            tone,
            lead: lead.into(),
            text: text.into(),
        }
    }

    fn text(text: impl Into<String>) -> Self {
        Self::new(Tone::Text, "", text)
    }

    fn dim(text: impl Into<String>) -> Self {
        Self::new(Tone::Dim, "", text)
    }

    fn strong(text: impl Into<String>) -> Self {
        Self::new(Tone::Strong, "", text)
    }

    fn good(text: impl Into<String>) -> Self {
        Self::new(Tone::Good, "", text)
    }

    fn warn(text: impl Into<String>) -> Self {
        Self::new(Tone::Warn, "", text)
    }

    fn bad(text: impl Into<String>) -> Self {
        Self::new(Tone::Bad, "", text)
    }

    fn numbered(number: usize, text: String) -> Self {
        Self::new(Tone::Text, format!("{number} "), text)
    }

    fn link(lead: &str, url: String) -> Self {
        Self::new(Tone::Link, lead, url)
    }

    fn labelled(lead: &str, text: String) -> Self {
        Self::new(Tone::Strong, lead, text)
    }

    fn qr(code: String) -> Self {
        Self::new(Tone::Qr, "", code)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(setup: &mut Setup, text: &str) {
        for char in text.chars() {
            setup.key(key(KeyCode::Char(char)));
        }
    }

    /// What the runtime describes, as `GET /api/provider` sends it.
    pub(crate) fn info(configured: serde_json::Value, needs: Option<&str>) -> ProviderInfo {
        let service = |id: &str, name: &str, way: &str, kind: &str, base_url: Option<&str>| {
            json!({
                "id": id, "name": name, "way": way, "kind": kind, "base_url": base_url,
                "description": format!("{name}, described."), "key_page": null,
                "steps": [format!("Get a key from {name}.")], "models": ["suggested-1"],
            })
        };
        let kind = |id: &str, requires: bool, accepts: bool| {
            json!({
                "id": id, "default_base_url": null, "key_variable": null, "accepts_key": accepts,
                "requires_key": requires, "key_in_environment": false,
            })
        };
        serde_json::from_value(json!({
            "configured": configured,
            "needs": needs,
            "kinds": [kind("chatgpt", false, false), kind("anthropic", true, true),
                kind("openai_compatible", false, true), kind("ollama", false, false)],
            "ways": [
                {"id": "plan", "name": "ChatGPT plan", "description": "Paid by your plan."},
                {"id": "key", "name": "API key", "description": "Billed per use."},
                {"id": "local", "name": "Local model", "description": "On your network."},
            ],
            "services": [
                service("chatgpt", "ChatGPT", "plan", "chatgpt", None),
                service("anthropic", "Anthropic (Claude)", "key", "anthropic", Some("https://api.anthropic.com")),
                service("openai-compatible", "Other service", "key", "openai_compatible", None),
                service("ollama", "Ollama", "local", "ollama", None),
            ],
            "sign_in_methods": [
                {"id": "codex", "name": "Sign in as Codex CLI", "pros": "p", "cons": "c", "recommended": "r"},
                {"id": "agent", "name": "Sign in as an agent", "pros": "p", "cons": "c", "recommended": "r"},
            ],
        }))
        .unwrap()
    }

    #[test]
    fn an_api_key_is_typed_hidden_and_the_model_searched_or_typed() {
        let mut setup = Setup::new(Some(info(json!(null), None)));
        assert!(setup.view(false).title.starts_with("How does Kelpy"));
        setup.key(key(KeyCode::Down));
        setup.key(key(KeyCode::Enter));
        // Two services for an API key: Anthropic, then its key.
        assert_eq!(setup.view(false).title, "API key · Service");
        setup.key(key(KeyCode::Enter));
        typed(&mut setup, "sk-ant-secret");
        let view = setup.view(false);
        assert!(!format!("{view:?}").contains("secret"), "{view:?}");
        assert!(format!("{view:?}").contains(&"•".repeat(13)));
        let ticket = setup.ticket;
        let calls = setup.key(key(KeyCode::Enter));
        let [Call::Models { api_key, .. }] = &calls[..] else {
            panic!("{calls:?}")
        };
        assert_eq!(api_key.as_deref(), Some("sk-ant-secret"));
        assert_ne!(setup.ticket, ticket);

        setup.reply(Reply::Models(
            serde_json::from_value(json!({"models": [
                {"id": "claude-sonnet-5-5", "recommended": true},
                {"id": "claude-haiku-5-5", "recommended": false},
                {"id": "claude-sonnet-5", "recommended": false},
            ], "error": null}))
            .unwrap(),
        ));
        typed(&mut setup, "sonnet-5");
        setup.key(key(KeyCode::Down));
        let calls = setup.key(key(KeyCode::Enter));
        let [Call::Save(request)] = &calls[..] else {
            panic!("{calls:?}")
        };
        assert_eq!(request["model"], "claude-sonnet-5");
        assert_eq!(request["service"], "anthropic");
        assert_eq!(request["api_key"], "sk-ant-secret");
        assert_eq!(setup.reply(Reply::Saved), [Call::CloseLater]);
        assert_eq!(setup.reply(Reply::Close), [Call::Close]);
    }

    #[test]
    fn a_refused_key_goes_back_to_the_question_with_why() {
        let mut setup = Setup::new(Some(info(json!(null), None)));
        setup.key(key(KeyCode::Down));
        setup.key(key(KeyCode::Enter));
        setup.key(key(KeyCode::Enter));
        // Required: an empty key is refused at once.
        assert!(setup.key(key(KeyCode::Enter)).is_empty());
        assert_eq!(
            setup.error.as_deref(),
            Some("This service needs an API key.")
        );
        typed(&mut setup, "k");
        setup.key(key(KeyCode::Enter));
        typed(&mut setup, "my-model");
        let calls = setup.key(key(KeyCode::Enter));
        assert!(matches!(&calls[..], [Call::Save(request)] if request["model"] == "my-model"));
        setup.reply(Reply::Failed("the key was refused".into()));
        let view = setup.view(false);
        assert_eq!(view.title, "Anthropic (Claude) · Model");
        assert!(format!("{view:?}").contains("the key was refused"));
    }

    #[test]
    fn esc_goes_back_one_question_and_closes_from_the_first() {
        let mut setup = Setup::new(Some(info(json!(null), None)));
        setup.key(key(KeyCode::Down));
        setup.key(key(KeyCode::Enter));
        setup.key(key(KeyCode::Down));
        setup.key(key(KeyCode::Enter));
        assert_eq!(setup.view(false).title, "Other service · Address");
        typed(&mut setup, "10.0.0.5");
        assert!(setup.key(key(KeyCode::Enter)).is_empty());
        assert!(setup.error.is_some());
        assert_eq!(setup.key(key(KeyCode::Esc)), [Call::Stop]);
        assert_eq!(setup.view(false).title, "API key · Service");
        setup.key(key(KeyCode::Esc));
        assert_eq!(setup.key(key(KeyCode::Esc)), [Call::Close]);
    }

    #[test]
    fn an_answer_for_a_step_left_behind_is_not_taken() {
        let mut setup = Setup::new(Some(info(json!(null), None)));
        let first = setup.ticket;
        setup.key(key(KeyCode::Enter));
        assert_ne!(setup.ticket, first);
        // The app drops replies whose ticket differs; the setup takes only those its step expects.
        assert!(setup.reply(Reply::Saved).is_empty());
        assert_eq!(setup.view(false).title, "ChatGPT plan");
    }

    #[test]
    fn a_chatgpt_plan_signs_in_with_a_code_then_offers_the_account_models() {
        let mut setup = Setup::new(Some(info(json!(null), None)));
        assert_eq!(setup.key(key(KeyCode::Enter)), [Call::ChatGptStatus]);
        setup.reply(Reply::ChatGpt(
            serde_json::from_value(json!({"signed_in": null, "registered": null, "kept": false}))
                .unwrap(),
        ));
        assert_eq!(setup.view(false).title, "ChatGPT plan · Sign in");
        setup.key(key(KeyCode::Enter));
        setup.key(key(KeyCode::Left));
        assert_eq!(
            setup.key(key(KeyCode::Enter)),
            [Call::SignInWithCode { keep: true }]
        );
        setup.reply(Reply::Code {
            url: "https://auth.openai.com/codex/device".into(),
            code: "K7QF-2MXD".into(),
        });
        assert_eq!(setup.link(), Some("https://auth.openai.com/codex/device"));
        let view = setup.view(true);
        assert!(view.text.iter().any(|para| para.tone == Tone::Qr));
        assert!(view.keys.starts_with("Ctrl+O"));
        let calls = setup.reply(Reply::SignedIn(Account {
            email: Some("admin@example.com".into()),
        }));
        assert_eq!(calls, [Call::ChatGptModels]);
        setup.reply(Reply::ChatGptModels(
            serde_json::from_value(json!([
                {"slug": "gpt-5.5", "display_name": "GPT-5.5"},
                {"slug": "gpt-5.5-mini", "display_name": "GPT-5.5 mini"},
            ]))
            .unwrap(),
        ));
        assert!(format!("{:?}", setup.view(false)).contains("(saved - log out to clear)"));
        setup.key(key(KeyCode::Down));
        let calls = setup.key(key(KeyCode::Enter));
        assert!(
            matches!(&calls[..], [Call::Save(request)] if request["model"] == "gpt-5.5-mini" && request["kind"] == "chatgpt")
        );
        // Esc from the models goes back to the questions, not to the code waiting.
        let mut back = Setup::new(Some(info(json!(null), None)));
        back.key(key(KeyCode::Enter));
        back.reply(Reply::ChatGpt(
            serde_json::from_value(
                json!({"signed_in": {"email": null}, "registered": null, "kept": true}),
            )
            .unwrap(),
        ));
        back.key(key(KeyCode::Esc));
        assert!(back.view(false).title.starts_with("How does Kelpy"));
    }

    #[test]
    fn signing_in_as_an_agent_takes_the_address_pasted() {
        let mut setup = Setup::new(Some(info(json!(null), None)));
        setup.key(key(KeyCode::Enter));
        setup.reply(Reply::ChatGpt(
            serde_json::from_value(
                json!({"signed_in": null, "registered": "admin@example.com", "kept": false}),
            )
            .unwrap(),
        ));
        setup.key(key(KeyCode::Down));
        setup.key(key(KeyCode::Enter));
        setup.key(key(KeyCode::Enter));
        assert_eq!(setup.view(false).title, "ChatGPT plan · Agent");
        setup.key(key(KeyCode::Right));
        assert_eq!(
            setup.key(key(KeyCode::Enter)),
            [Call::StartSignIn {
                keep: false,
                forget: true
            }]
        );
        setup.reply(Reply::SignInAddress(
            "https://auth.openai.com/authorize?x".into(),
        ));
        setup.paste("http://127.0.0.1:4321/auth/callback?code=abc&state=s\r\n");
        assert_eq!(
            setup.key(key(KeyCode::Enter)),
            [Call::FinishSignIn(
                "http://127.0.0.1:4321/auth/callback?code=abc&state=s".into()
            )]
        );
    }

    #[test]
    fn ollama_missing_is_installed_then_a_model_downloaded_and_saved() {
        let mut setup = Setup::new(Some(info(json!(null), None)));
        setup.key(key(KeyCode::Down));
        setup.key(key(KeyCode::Down));
        setup.key(key(KeyCode::Enter));
        assert_eq!(
            setup.key(key(KeyCode::Enter)),
            [Call::Ollama(String::new())]
        );
        let status = |running: bool, installed: bool| -> OllamaStatus {
            serde_json::from_value(json!({
                "base_url": "http://127.0.0.1:11434", "running": running, "error": "refused",
                "installed": installed, "models": [], "memory": 16_000_000_000_u64,
                "suggestions": [{"name": "qwen3.5:9b", "size": 6_600_000_000_u64, "description": "Good."}],
            }))
            .unwrap()
        };
        assert_eq!(
            setup.reply(Reply::Ollama(status(false, false))),
            [Call::Installer]
        );
        setup.reply(Reply::Installer(Ok(Installer {
            command: "curl -fsSL https://ollama.com/install.sh | sh".into(),
            notes: vec!["Ollama's own installer.".into()],
        })));
        assert_eq!(setup.key(key(KeyCode::Enter)), [Call::Install]);
        let installed = |done: bool| Installation {
            output: ">>> Installing ollama\n".into(),
            done,
            error: None,
        };
        assert!(
            setup
                .reply(Reply::Installation(installed(false)))
                .is_empty()
        );
        assert_eq!(
            setup.reply(Reply::Installation(installed(true))),
            [Call::Ollama(String::new())]
        );
        setup.reply(Reply::Ollama(status(true, true)));
        assert_eq!(
            setup.view(false).keys,
            "↑↓ choose · Enter download · Esc back"
        );
        assert_eq!(
            setup.key(key(KeyCode::Enter)),
            [Call::Download {
                base_url: "http://127.0.0.1:11434".into(),
                model: "qwen3.5:9b".into()
            }]
        );
        let download = |done: bool| Download {
            status: "pulling".into(),
            completed: 3,
            total: 6,
            done,
            error: None,
        };
        assert!(setup.reply(Reply::Download(download(false))).is_empty());
        assert!(setup.view(false).controls.contains(&Control::Bar(0.5)));
        let calls = setup.reply(Reply::Download(download(true)));
        assert!(
            matches!(&calls[..], [Call::Save(request)] if request["model"] == "qwen3.5:9b" && request["base_url"].is_null())
        );
    }

    #[test]
    fn an_ollama_address_changed_while_it_is_checked_drops_that_check() {
        let mut setup = Setup::new(Some(info(json!(null), None)));
        setup.key(key(KeyCode::Down));
        setup.key(key(KeyCode::Down));
        setup.key(key(KeyCode::Enter));
        typed(&mut setup, "http://a:11434");
        assert_eq!(
            setup.key(key(KeyCode::Enter)),
            [Call::Ollama("http://a:11434".into())]
        );
        let checking = setup.ticket;
        // B typed while A is checked: A's answer is for another ticket, and is dropped by the app.
        setup.paste("/b");
        assert_ne!(setup.ticket, checking);
        assert!(!format!("{:?}", setup.view(false)).contains("Checking"));
        let running: OllamaStatus = serde_json::from_value(json!({
            "base_url": "http://b:11434/b", "running": true, "error": null, "installed": true,
            "models": [{"name": "qwen3.5:4b", "size": 3_400_000_000_u64}], "memory": null, "suggestions": [],
        }))
        .unwrap();
        // Checked again, the answer is saved with the address it was for.
        assert_eq!(
            setup.key(key(KeyCode::Enter)),
            [Call::Ollama("http://a:11434/b".into())]
        );
        setup.reply(Reply::Ollama(running));
        let calls = setup.key(key(KeyCode::Enter));
        assert!(
            matches!(&calls[..], [Call::Save(request)] if request["base_url"] == "http://a:11434/b"),
            "{calls:?}"
        );
    }

    #[test]
    fn opened_for_what_the_provider_lacks_it_starts_at_that_question() {
        let groq =
            json!({"kind": "anthropic", "service": "anthropic", "model": "claude-sonnet-5-5"});
        let lacking = info(groq.clone(), Some("key"));
        assert_eq!(lack(&lacking), Some(Lack::Key));
        let (mut setup, calls) = Setup::lacking(lacking, Lack::Key);
        assert!(calls.is_empty());
        assert_eq!(setup.view(false).title, "Anthropic (Claude) · Key");
        // Esc goes back to the service, then the way, with the one in use proposed.
        setup.key(key(KeyCode::Esc));
        assert_eq!(setup.view(false).title, "API key · Service");
        setup.key(key(KeyCode::Esc));
        assert!(matches!(setup.step, Step::Way { cursor: 1 }));

        let chatgpt = json!({"kind": "chatgpt", "service": "chatgpt", "model": "gpt-5.5"});
        let (setup, calls) = Setup::lacking(info(chatgpt, Some("sign_in")), Lack::SignIn);
        assert_eq!(calls, [Call::ChatGptStatus]);
        assert_eq!(setup.view(false).title, "ChatGPT plan");
        assert_eq!(lack(&info(json!(null), None)), Some(Lack::Provider));
        assert_eq!(lack(&info(groq, None)), None);
    }
}
