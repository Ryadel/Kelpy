//! The ChatGPT session: signing in with the user's ChatGPT plan, and the tokens that pay for the answers.
//!
//! Two ways to sign in ([`Method`]): as the Codex CLI, approving a short code on any device, with answers from
//! ChatGPT's Codex backend; or as an agent of the account, OpenAI's program for apps, with answers from the API.
//! Tokens live in memory: the session ends with Kelpy, which revokes the refresh token when it stops. The agent
//! registration is kept, in `data/chatgpt.toml`: this host's id, and the client id OpenAI issued for the account with
//! its email. None of it is a secret, and without the host id it can be used by another Kelpy.
//!
//! At the user's request a session is kept across restarts instead ([`Lifetime::Kept`], "Save the sign-in on this
//! computer"): `data/chatgpt-session.toml`, owner-only, holds what renews it (account, client, refresh token) and is
//! rewritten at every renewal, because refresh tokens rotate. Stopping Kelpy leaves such a session in place; signing
//! out ends it everywhere.
//!
//! One state owns the whole lifecycle (registration, sign-in under way, session), so that signing in, renewing and
//! signing out decide against the same facts. Network calls happen outside it, and their results are committed only
//! if the state is still the one they started from: a sign-in completing after a sign-out, or a renewal of a session
//! ended meanwhile, is not committed, and its tokens are revoked. Every change the interfaces show (signed in or
//! out, the registration) is published as [`Event::ProviderChanged`].

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kelpy_common::{lock, unix_now};
use kelpy_providers::chatgpt::codex::{self, CODE_LIFETIME, DeviceCode};
use kelpy_providers::chatgpt::{
    Account, Attempt, Callback, Endpoints, Model, SignInError, Tokens, new_host_id,
};
use kelpy_providers::{Provider, Settings};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::events::{Event, Events};
use crate::home::Home;
use crate::private_file;

const REGISTRATION_FILE: &str = "chatgpt.toml";
const SESSION_FILE: &str = "chatgpt-session.toml";
/// How long a sign-in as an agent may take in the browser.
const SIGN_IN_TIME: Duration = Duration::from_secs(10 * 60);
/// Access tokens are renewed this long before they expire, so that no request starts with one about to end.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
/// How long revoking tokens waits for OpenAI, unless the caller has a nearer deadline.
pub const REVOKE_WAIT: Duration = Duration::from_secs(5);
/// Heads the agent registration file, which an administrator may open.
const AGENT_FILE_HEADER: &str = "# Kelpy agent registration for ChatGPT: the agent OpenAI registered for one account. \
No secrets.\n# On another Kelpy, use it in the Web UI or run `kelpy provider --import-agent`: its sign-in then only asks \
for approval.\n\n";

#[derive(Debug, thiserror::Error)]
pub enum ChatGptError {
    #[error("not signed in with ChatGPT")]
    NotSignedIn,
    #[error(
        "you signed in with another ChatGPT account than the one Kelpy is registered in: forget the registration to \
         use another account"
    )]
    OtherAccount,
    #[error("sign out first: the session uses this registration")]
    RegistrationInUse,
    #[error("the sign-in was cancelled by a sign-out or by another sign-in")]
    Abandoned,
    #[error("the code was not approved in time: start the sign-in again")]
    Expired,
    #[error("not an agent registration file of Kelpy, or one made before the account was verified")]
    InvalidAgentFile,
    #[error(transparent)]
    SignIn(#[from] SignInError),
    #[error("cannot save the ChatGPT sign-in: {0}")]
    Save(#[from] io::Error),
}

/// How a session was signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    /// The Codex CLI's sign-in, approved with a code.
    Codex,
    /// OpenAI's program for apps: Kelpy registered as an agent of the account.
    Agent,
}

/// How long a session lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lifetime {
    /// Until Kelpy stops: nothing of it is written to disk.
    #[default]
    Run,
    /// Across restarts on this computer, until a sign-out: chosen by the user, with a warning.
    Kept,
}

/// What the interfaces show about ChatGPT. Never contains a token.
#[derive(Debug, Clone, Serialize)]
pub struct ChatGptStatus {
    /// The account signed in for this session.
    pub signed_in: Option<Account>,
    /// How the session was signed in.
    pub method: Option<Method>,
    /// The session stays signed in on this computer across restarts.
    pub kept: bool,
    /// The email of the account Kelpy is registered for as an agent, used to sign in again.
    pub registered: Option<String>,
}

/// How a sign-out went: OpenAI and this computer apart, since either may fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SignedOut {
    /// OpenAI confirmed that the tokens are revoked; `true` when there was no session.
    pub revoked: bool,
    /// The session kept on this computer was removed; `true` when there was none. Otherwise the next start signs in
    /// with it again, unless OpenAI revoked it.
    pub forgotten: bool,
}

pub struct ChatGpt {
    provider: Provider,
    endpoints: Endpoints,
    registration_path: PathBuf,
    session_path: PathBuf,
    state: Mutex<State>,
    /// Serializes renewals, so a rotating refresh token is used once. Signing out never waits for it.
    renewing: tokio::sync::Mutex<()>,
    events: Events,
}

#[derive(Default)]
struct State {
    /// What `data/chatgpt.toml` holds: changed only after the file was written.
    registration: Registration,
    pending: Option<Pending>,
    session: Option<Session>,
    /// Counts sign-outs: a sign-in under way when one happens must not complete after it.
    sign_outs: u64,
    /// Numbers the code sign-ins, so that each recognises itself as the one under way.
    codes: u64,
}

impl State {
    /// Puts `next` in place of the sign-in under way, whose wait for a code ends.
    fn replace_pending(&mut self, next: Option<Pending>) {
        if let Some(Pending::Code { stop, .. }) = std::mem::replace(&mut self.pending, next) {
            stop.cancel();
        }
    }

    /// Whether the code sign-in `number` is still the one under way: no sign-out or other sign-in took its place.
    fn is_code_under_way(&self, number: u64) -> bool {
        matches!(&self.pending, Some(Pending::Code { number: under_way, .. }) if *under_way == number)
    }
}

/// A sign-in under way, until approved.
enum Pending {
    /// As an agent: waiting for the browser to come back.
    Agent {
        attempt: Attempt,
        started: Instant,
        lifetime: Lifetime,
    },
    /// As the Codex CLI: waiting for the code to be approved, until `stop`.
    Code {
        number: u64,
        stop: CancellationToken,
    },
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Registration {
    host_id: Option<String>,
    client: Option<Client>,
}

/// The client OpenAI issued, and the account it belongs to once a sign-in verified it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Client {
    id: String,
    account: Option<Account>,
}

/// Whom a session's tokens were issued to.
#[derive(Clone)]
enum Grant {
    Codex,
    Agent { client_id: String },
}

struct Session {
    grant: Grant,
    account: Account,
    tokens: Tokens,
    expires_at: Instant,
    lifetime: Lifetime,
}

/// A session handed over to the Kelpy restarted in this one's place: what renews it, its access token while valid,
/// and whether it is also kept on disk.
#[derive(Serialize, Deserialize)]
pub struct SessionState {
    kept: Kept,
    access_token: String,
    /// Seconds the access token stays valid.
    valid_for: u64,
    kept_on_disk: bool,
}

/// A session kept on disk: only what renews it, never an access token.
#[derive(Serialize, Deserialize)]
struct Kept {
    account: Account,
    /// The client the tokens were issued to; none for the Codex CLI.
    client_id: Option<String>,
    refresh_token: String,
}

impl Session {
    fn new(grant: Grant, account: Account, tokens: Tokens, lifetime: Lifetime) -> Self {
        let expires_at = Instant::now() + Duration::from_secs(tokens.expires_in);
        Self {
            grant,
            account,
            tokens,
            expires_at,
            lifetime,
        }
    }

    /// A kept session after a restart: without an access token, so its first use renews it.
    fn restored(kept: Kept) -> Self {
        let grant = match kept.client_id {
            Some(client_id) => Grant::Agent { client_id },
            None => Grant::Codex,
        };
        let tokens = Tokens {
            access_token: String::new(),
            refresh_token: kept.refresh_token,
            expires_in: 0,
        };
        Self::new(grant, kept.account, tokens, Lifetime::Kept)
    }

    fn kept(&self) -> Kept {
        Kept {
            account: self.account.clone(),
            client_id: match &self.grant {
                Grant::Codex => None,
                Grant::Agent { client_id } => Some(client_id.clone()),
            },
            refresh_token: self.tokens.refresh_token.clone(),
        }
    }

    fn method(&self) -> Method {
        match self.grant {
            Grant::Codex => Method::Codex,
            Grant::Agent { .. } => Method::Agent,
        }
    }
}

/// A sign-in as the Codex CLI under way: the code to show, until [`ChatGpt::finish_code`] ends.
pub struct CodeSignIn {
    code: DeviceCode,
    number: u64,
    stop: CancellationToken,
    deadline: tokio::time::Instant,
    lifetime: Lifetime,
}

impl CodeSignIn {
    pub fn code(&self) -> &DeviceCode {
        &self.code
    }
}

impl ChatGpt {
    /// Reads the registration of `home`, if any, and the session kept there. Nothing is written until the first
    /// sign-in as an agent. Changes are published to `events`.
    pub fn new(home: &Home, provider: Provider, endpoints: Endpoints, events: Events) -> Self {
        let registration_path = home.data_dir().join(REGISTRATION_FILE);
        let session_path = home.data_dir().join(SESSION_FILE);
        // A missing or unreadable file only means signing in again.
        let registration: Registration = fs::read_to_string(&registration_path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        // An agent session belongs to the registered client: a registration made since is for another account.
        let session = fs::read_to_string(&session_path)
            .ok()
            .and_then(|text| toml::from_str::<Kept>(&text).ok())
            .filter(|kept| {
                kept.client_id.as_ref().is_none_or(|id| {
                    registration
                        .client
                        .as_ref()
                        .is_some_and(|client| &client.id == id)
                })
            })
            .map(Session::restored);
        Self {
            provider,
            endpoints,
            registration_path,
            session_path,
            state: Mutex::new(State {
                registration,
                session,
                ..State::default()
            }),
            renewing: tokio::sync::Mutex::new(()),
            events,
        }
    }

    /// The session open, for the Kelpy restarted in this one's place. A sign-in under way is not handed over.
    pub(crate) fn handed_over(&self) -> Option<SessionState> {
        let state = lock(&self.state);
        let session = state.session.as_ref()?;
        Some(SessionState {
            kept: session.kept(),
            access_token: session.tokens.access_token.clone(),
            valid_for: session
                .expires_at
                .saturating_duration_since(Instant::now())
                .as_secs(),
            kept_on_disk: session.lifetime == Lifetime::Kept,
        })
    }

    /// Takes over the session the Kelpy before this one had open, in place of one kept on disk.
    pub(crate) fn take_over(&self, handed: SessionState) {
        let mut session = Session::restored(handed.kept);
        session.tokens.access_token = handed.access_token;
        session.expires_at = Instant::now() + Duration::from_secs(handed.valid_for);
        if !handed.kept_on_disk {
            session.lifetime = Lifetime::Run;
        }
        lock(&self.state).session = Some(session);
        self.events.publish(Event::ProviderChanged);
    }

    /// Whether a session is open: answers can use the plan.
    pub fn signed_in(&self) -> bool {
        lock(&self.state).session.is_some()
    }

    pub fn status(&self) -> ChatGptStatus {
        let state = lock(&self.state);
        ChatGptStatus {
            signed_in: state
                .session
                .as_ref()
                .map(|session| session.account.clone()),
            method: state.session.as_ref().map(Session::method),
            kept: state
                .session
                .as_ref()
                .is_some_and(|session| session.lifetime == Lifetime::Kept),
            registered: state
                .registration
                .client
                .as_ref()
                .and_then(|client| client.account.as_ref()?.email.clone()),
        }
    }

    /// Starts a sign-in as an agent that comes back to `redirect_uri`, and returns the address to open in the
    /// browser. It reuses the registered client, if any: only its account is then accepted. A newer sign-in replaces
    /// one still under way. `lifetime` is how long its session will last.
    pub fn start(&self, redirect_uri: String, lifetime: Lifetime) -> Result<String, ChatGptError> {
        let mut state = lock(&self.state);
        if state.registration.host_id.is_none() {
            let candidate = Registration {
                host_id: Some(new_host_id()?),
                client: state.registration.client.clone(),
            };
            self.commit(&mut state, candidate)?;
        }
        let registration = &state.registration;
        let host_id = registration.host_id.as_deref().unwrap_or_default();
        let returning = registration.client.as_ref().map(|client| {
            let email = client
                .account
                .as_ref()
                .and_then(|account| account.email.as_deref());
            (client.id.as_str(), email)
        });
        let (attempt, url) = Attempt::start(&self.endpoints, redirect_uri, host_id, returning)?;
        state.replace_pending(Some(Pending::Agent {
            attempt,
            started: Instant::now(),
            lifetime,
        }));
        Ok(url.into())
    }

    /// Completes the sign-in as an agent with what the browser brought back, and starts the session.
    pub async fn finish(&self, callback: &Callback) -> Result<Account, ChatGptError> {
        let (attempt, accepted, sign_outs, lifetime) = {
            let mut state = lock(&self.state);
            let Some(Pending::Agent {
                attempt,
                started,
                lifetime,
            }) = state
                .pending
                .take_if(|pending| matches!(pending, Pending::Agent { .. }))
            else {
                return Err(SignInError::UnknownAttempt.into());
            };
            if started.elapsed() > SIGN_IN_TIME {
                return Err(SignInError::UnknownAttempt.into());
            }
            let accepted = attempt.accept(callback)?;
            // The issued client id is kept before the code is exchanged, so a failed exchange does not register
            // Kelpy a second time.
            if state.registration.client.as_ref().map(|client| &client.id)
                != Some(&accepted.client_id)
            {
                let candidate = Registration {
                    client: Some(Client {
                        id: accepted.client_id.clone(),
                        account: None,
                    }),
                    ..state.registration.clone()
                };
                self.commit(&mut state, candidate)?;
            }
            (attempt, accepted, state.sign_outs, lifetime)
        };
        let (account, tokens) = self
            .provider
            .chatgpt(&self.endpoints)
            .exchange(&attempt, &accepted, unix_now())
            .await?;
        let session = Session::new(
            Grant::Agent {
                client_id: accepted.client_id,
            },
            account.clone(),
            tokens,
            lifetime,
        );
        let opened = {
            let mut state = lock(&self.state);
            let admitted = self
                .admit(&mut state, sign_outs, &session)
                .and_then(|()| self.persist(&session));
            match admitted {
                Ok(()) => Ok(state.session.replace(session)),
                Err(refused) => Err((refused, session)),
            }
        };
        self.settle(opened).await.map(|()| account)
    }

    /// Whether a sign-in as an agent whose code was exchanged may become the session: no sign-out happened since it
    /// was accepted, its client is still the registered one, and it belongs to the client's account (the first
    /// sign-in records that account).
    fn admit(
        &self,
        state: &mut State,
        sign_outs: u64,
        session: &Session,
    ) -> Result<(), ChatGptError> {
        if state.sign_outs != sign_outs {
            return Err(ChatGptError::NotSignedIn);
        }
        let Grant::Agent { client_id } = &session.grant else {
            return Err(SignInError::UnknownAttempt.into());
        };
        let Some(client) = state
            .registration
            .client
            .as_ref()
            .filter(|client| &client.id == client_id)
        else {
            // Another sign-in changed the registration meanwhile.
            return Err(SignInError::UnknownAttempt.into());
        };
        let account = &session.account;
        match &client.account {
            Some(known) if known.sub != account.sub => Err(ChatGptError::OtherAccount),
            Some(known) if known == account => Ok(()),
            _ => {
                let candidate = Registration {
                    client: Some(Client {
                        id: client_id.clone(),
                        account: Some(account.clone()),
                    }),
                    ..state.registration.clone()
                };
                Ok(self.commit(state, candidate)?)
            }
        }
    }

    /// Starts a sign-in as the Codex CLI: OpenAI issues a code for the user to approve. A newer sign-in replaces one
    /// still under way. `lifetime` is how long its session will last.
    pub async fn start_code(&self, lifetime: Lifetime) -> Result<CodeSignIn, ChatGptError> {
        // Under way before OpenAI is asked, so a sign-out or a newer sign-in meanwhile takes its place.
        let stop = CancellationToken::new();
        let number = {
            let mut state = lock(&self.state);
            state.codes += 1;
            let number = state.codes;
            state.replace_pending(Some(Pending::Code {
                number,
                stop: stop.clone(),
            }));
            number
        };
        let issued = self.provider.codex(&self.endpoints).device_code().await;
        let deadline = tokio::time::Instant::now() + CODE_LIFETIME;
        let mut state = lock(&self.state);
        if !state.is_code_under_way(number) {
            return Err(ChatGptError::Abandoned);
        }
        let code = issued.inspect_err(|_| state.pending = None)?;
        Ok(CodeSignIn {
            code,
            number,
            stop,
            deadline,
            lifetime,
        })
    }

    /// Waits until the code is approved, then starts the session. Ends at once if a sign-out or another sign-in takes
    /// its place first, and when the code expires.
    pub async fn finish_code(&self, sign_in: CodeSignIn) -> Result<Account, ChatGptError> {
        let codex = self.provider.codex(&self.endpoints);
        let approval = async {
            loop {
                if let Some(approval) = codex.poll(&sign_in.code).await? {
                    return Ok::<_, SignInError>(approval);
                }
                tokio::time::sleep(sign_in.code.interval).await;
            }
        };
        let approval = tokio::select! {
            () = sign_in.stop.cancelled() => return Err(ChatGptError::Abandoned),
            approval = tokio::time::timeout_at(sign_in.deadline, approval) => {
                approval.map_err(|_| ChatGptError::Expired)??
            }
        };
        // Approved: from here on, as for an agent, only a sign-out stops it.
        let sign_outs = {
            let mut state = lock(&self.state);
            if !state.is_code_under_way(sign_in.number) {
                return Err(ChatGptError::Abandoned);
            }
            state.pending = None;
            state.sign_outs
        };
        let (account, tokens) = codex.exchange(&approval, unix_now()).await?;
        let session = Session::new(Grant::Codex, account.clone(), tokens, sign_in.lifetime);
        let opened = {
            let mut state = lock(&self.state);
            let admitted = if state.sign_outs == sign_outs {
                self.persist(&session)
            } else {
                Err(ChatGptError::Abandoned)
            };
            match admitted {
                Ok(()) => Ok(state.session.replace(session)),
                Err(refused) => Err((refused, session)),
            }
        };
        self.settle(opened).await.map(|()| account)
    }

    /// Puts on disk what `session` needs there before it becomes the session: a kept session is written, any other
    /// removes the kept one. Called under the state lock, so what is on disk never outlives what is in memory.
    fn persist(&self, session: &Session) -> Result<(), ChatGptError> {
        Ok(match session.lifetime {
            Lifetime::Kept => self.keep(session),
            Lifetime::Run => self.forget_kept(),
        }?)
    }

    /// Ends a sign-in whose tokens were obtained: revokes the session it replaced, or its own tokens if it was refused.
    async fn settle(
        &self,
        opened: Result<Option<Session>, (ChatGptError, Session)>,
    ) -> Result<(), ChatGptError> {
        let deadline = tokio::time::Instant::now() + REVOKE_WAIT;
        match opened {
            Ok(replaced) => {
                self.events.publish(Event::ProviderChanged);
                if let Some(replaced) = replaced {
                    self.end(&replaced, deadline).await;
                }
                Ok(())
            }
            Err((refused, unused)) => {
                // Best effort: these tokens are never used, and they expire on their own.
                self.end(&unused, deadline).await;
                Err(refused)
            }
        }
    }

    /// Makes `settings` call the model with the session: its access token, renewed first if about to expire, sent
    /// only to the address the session belongs to.
    pub async fn authorize(&self, settings: &mut Settings) -> Result<(), ChatGptError> {
        let (grant, token) = self.access().await?;
        (settings.base_url, settings.headers) = match grant {
            Grant::Codex => (Some(self.endpoints.codex.clone()), codex::headers(&token)),
            Grant::Agent { .. } => (Some(self.endpoints.api.clone()), Vec::new()),
        };
        settings.api_key = Some(token);
        Ok(())
    }

    /// The session's grant and access token, renewed first if it is about to expire.
    async fn access(&self) -> Result<(Grant, String), ChatGptError> {
        let _renewing = self.renewing.lock().await;
        loop {
            let (grant, refresh_token) = {
                let state = lock(&self.state);
                let session = state.session.as_ref().ok_or(ChatGptError::NotSignedIn)?;
                if session.expires_at.saturating_duration_since(Instant::now()) >= REFRESH_MARGIN {
                    return Ok((session.grant.clone(), session.tokens.access_token.clone()));
                }
                (session.grant.clone(), session.tokens.refresh_token.clone())
            };
            let renewed = match &grant {
                Grant::Codex => {
                    let codex = self.provider.codex(&self.endpoints);
                    codex.refresh(&refresh_token, unix_now()).await
                }
                Grant::Agent { client_id } => {
                    let chatgpt = self.provider.chatgpt(&self.endpoints);
                    chatgpt.refresh(client_id, &refresh_token, unix_now()).await
                }
            };

            let superseded = {
                let mut state = lock(&self.state);
                // Only the session that was renewed may take the new tokens: meanwhile it may have ended, or a
                // sign-in may have replaced it, whose own token the next round returns.
                let current = state
                    .session
                    .as_mut()
                    .filter(|session| session.tokens.refresh_token == refresh_token);
                match (current, renewed) {
                    (Some(session), Ok(tokens)) => {
                        let mut renewed = Session::new(
                            grant.clone(),
                            session.account.clone(),
                            tokens,
                            session.lifetime,
                        );
                        // The old refresh token no longer works: if the new one cannot be kept, the session stops
                        // being kept (the interfaces show it) rather than leaving a dead token on disk.
                        if renewed.lifetime == Lifetime::Kept && self.keep(&renewed).is_err() {
                            renewed.lifetime = Lifetime::Run;
                            // Best effort: the file holds the old token, which OpenAI no longer accepts.
                            let _ = self.forget_kept();
                            self.events.publish(Event::ProviderChanged);
                        }
                        *session = renewed;
                        return Ok((grant, session.tokens.access_token.clone()));
                    }
                    (Some(_), Err(SignInError::Ended)) => {
                        state.session = None;
                        // Best effort: OpenAI ended the session, so the token on disk no longer works.
                        let _ = self.forget_kept();
                        self.events.publish(Event::ProviderChanged);
                        return Err(SignInError::Ended.into());
                    }
                    (Some(_), Err(error)) => return Err(error.into()),
                    (None, Ok(tokens)) => tokens,
                    (None, Err(_)) => continue,
                }
            };
            let deadline = tokio::time::Instant::now() + REVOKE_WAIT;
            self.revoke(&grant, &superseded.refresh_token, deadline)
                .await;
        }
    }

    /// The models the signed-in account may choose.
    pub async fn models(&self) -> Result<Vec<Model>, ChatGptError> {
        let (grant, token) = self.access().await?;
        Ok(match grant {
            Grant::Codex => self.provider.codex(&self.endpoints).models(&token).await?,
            Grant::Agent { .. } => {
                self.provider
                    .chatgpt(&self.endpoints)
                    .models(&token)
                    .await?
            }
        })
    }

    /// Ends the session at once, a kept one too: a sign-in under way is abandoned, the tokens are forgotten, here
    /// and on disk, and the refresh token is revoked, waiting for OpenAI until `deadline` at most. The registration
    /// stays, for the next sign-in.
    pub async fn sign_out(&self, deadline: tokio::time::Instant) -> SignedOut {
        self.leave(deadline, false).await
    }

    /// Kelpy is stopping: a kept session stays, as its user asked; any other ends as with [`ChatGpt::sign_out`].
    /// Returns whether OpenAI confirmed the revocation.
    pub async fn close(&self, deadline: tokio::time::Instant) -> bool {
        self.leave(deadline, true).await.revoked
    }

    /// Ends the sign-in under way and the session, except a kept session when `spare_kept`.
    async fn leave(&self, deadline: tokio::time::Instant, spare_kept: bool) -> SignedOut {
        let (ended, forgotten) = {
            let mut state = lock(&self.state);
            state.sign_outs += 1;
            state.replace_pending(None);
            let ended = state
                .session
                .take_if(|session| !(spare_kept && session.lifetime == Lifetime::Kept));
            (ended, spare_kept || self.forget_kept().is_ok())
        };
        let revoked = match ended {
            Some(session) => {
                self.events.publish(Event::ProviderChanged);
                self.end(&session, deadline).await
            }
            None => true,
        };
        SignedOut { revoked, forgotten }
    }

    /// The agent registration for another Kelpy, as the text of a file: the client OpenAI issued and its account. No
    /// host id, which each installation keeps its own, and no token. `None` until a sign-in verified the account.
    pub fn agent_file(&self) -> Option<String> {
        let state = lock(&self.state);
        let client = state
            .registration
            .client
            .clone()
            .filter(|client| client.account.is_some())?;
        let file = Registration {
            host_id: None,
            client: Some(client),
        };
        // Strings and an optional table only: serializing cannot fail.
        let text = toml::to_string(&file).ok()?;
        Some(format!("{AGENT_FILE_HEADER}{text}"))
    }

    /// Uses the agent registration of another Kelpy, from [`ChatGpt::agent_file`]: the next sign-in as an agent asks
    /// only for approval, and only that account is accepted. A host id in the file is ignored: this installation
    /// keeps its own. Returns the account's email.
    pub fn use_agent_file(&self, text: &str) -> Result<Option<String>, ChatGptError> {
        let file: Registration =
            toml::from_str(text).map_err(|_| ChatGptError::InvalidAgentFile)?;
        let client = file
            .client
            .filter(|client| !client.id.trim().is_empty() && client.account.is_some())
            .ok_or(ChatGptError::InvalidAgentFile)?;
        let email = client
            .account
            .as_ref()
            .and_then(|account| account.email.clone());
        let mut state = lock(&self.state);
        let candidate = Registration {
            host_id: Some(self.host_id(&state.registration)?),
            client: Some(client),
        };
        self.commit(&mut state, candidate)?;
        Ok(email)
    }

    /// Forgets the agent registration, so that the next sign-in as an agent registers again, with any account. The
    /// agent stays in the ChatGPT account until removed there. Refused while a session uses the registration; a
    /// sign-in as an agent under way is abandoned, since completing it would bring the forgotten client back.
    pub fn forget_registration(&self) -> Result<(), ChatGptError> {
        let mut state = lock(&self.state);
        if state.registration.client.is_none() {
            return Ok(());
        }
        if state
            .session
            .as_ref()
            .is_some_and(|session| session.method() == Method::Agent)
        {
            return Err(ChatGptError::RegistrationInUse);
        }
        if matches!(state.pending, Some(Pending::Agent { .. })) {
            state.replace_pending(None);
        }
        let candidate = Registration {
            client: None,
            ..state.registration.clone()
        };
        Ok(self.commit(&mut state, candidate)?)
    }

    /// This installation's host id: the registered one, or a new one.
    fn host_id(&self, registration: &Registration) -> Result<String, ChatGptError> {
        match &registration.host_id {
            Some(host_id) => Ok(host_id.clone()),
            None => Ok(new_host_id()?),
        }
    }

    /// Revokes a session's tokens, waiting for OpenAI until `deadline` at most; whether it confirmed.
    async fn end(&self, session: &Session, deadline: tokio::time::Instant) -> bool {
        self.revoke(&session.grant, &session.tokens.refresh_token, deadline)
            .await
    }

    async fn revoke(
        &self,
        grant: &Grant,
        refresh_token: &str,
        deadline: tokio::time::Instant,
    ) -> bool {
        let revoke = async {
            match grant {
                Grant::Codex => {
                    let codex = self.provider.codex(&self.endpoints);
                    codex.revoke(refresh_token).await
                }
                Grant::Agent { client_id } => {
                    let chatgpt = self.provider.chatgpt(&self.endpoints);
                    chatgpt.revoke(client_id, refresh_token).await
                }
            }
        };
        matches!(tokio::time::timeout_at(deadline, revoke).await, Ok(Ok(())))
    }

    /// Writes what renews `session` to disk, owner-only.
    fn keep(&self, session: &Session) -> io::Result<()> {
        let text = toml::to_string(&session.kept()).map_err(io::Error::other)?;
        private_file::write(&self.session_path, text.as_bytes())
    }

    /// Removes the kept session from disk, if there is one.
    fn forget_kept(&self) -> io::Result<()> {
        match fs::remove_file(&self.session_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    /// Writes `candidate` to disk, then makes it the registration: memory never holds what could not be saved.
    fn commit(&self, state: &mut State, candidate: Registration) -> io::Result<()> {
        let text = toml::to_string(&candidate).map_err(io::Error::other)?;
        private_file::write(&self.registration_path, text.as_bytes())?;
        state.registration = candidate;
        self.events.publish(Event::ProviderChanged);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kelpy_test_support::{FakeServer, OPENAI_ISSUER, id_token, jwks};
    use serde_json::json;

    const TOKEN: &str = "POST /api/accounts/oauth/token";
    const REVOKE: &str = "POST /api/accounts/oauth/revoke";

    const REDIRECT: &str = "http://127.0.0.1:4000/auth/callback";
    const GRANTED: &str =
        "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

    const CODE_TOKEN: &str = "POST /oauth/token";
    const APPROVAL: &str = "POST /api/accounts/deviceauth/token";
    const USER_CODE: &str = "POST /api/accounts/deviceauth/usercode";

    /// A fake OpenAI for signing in: keys, codes and revocations answer at once, tokens and approvals as each test
    /// sets them.
    pub(crate) async fn fake_openai() -> (FakeServer, Endpoints) {
        let server = FakeServer::start(vec![
            ("GET /.well-known/jwks.json", "200 OK", jwks().to_string()),
            ("POST /api/accounts/oauth/revoke", "200 OK", String::new()),
            ("POST /oauth/revoke", "200 OK", String::new()),
            (
                USER_CODE,
                "200 OK",
                json!({ "device_auth_id": "device-1", "user_code": "ABCD-1234", "interval": "1" })
                    .to_string(),
            ),
            (APPROVAL, "403 Forbidden", String::new()),
        ])
        .await;
        let endpoints = Endpoints {
            auth: server.url.clone(),
            api: format!("{}/v1", server.url),
            codex: format!("{}/codex", server.url),
        };
        (server, endpoints)
    }

    impl ChatGpt {
        async fn access_token(&self) -> Result<String, ChatGptError> {
            self.access().await.map(|(_, token)| token)
        }
    }

    /// Makes OpenAI approve the code under way for `sub`, of the ChatGPT account `workspace-1`, with tokens valid
    /// for `expires_in` seconds.
    pub(crate) fn approve_code(server: &FakeServer, sub: &str, expires_in: u64) {
        let access_token = id_token(&json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "workspace-1" },
        }));
        let tokens = json!({
            "access_token": access_token, "refresh_token": format!("refresh-{sub}"), "expires_in": expires_in,
            "id_token": id_token(&json!({ "sub": sub, "email": format!("{sub}@example.com") })),
        });
        server.respond(CODE_TOKEN, "200 OK", tokens.to_string());
        server.respond(
            APPROVAL,
            "200 OK",
            json!({ "authorization_code": "auth-code", "code_verifier": "verifier" }).to_string(),
        );
    }

    fn param(url: &str, name: &str) -> Option<String> {
        url.split_once('?')?
            .1
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .map(str::to_owned)
    }

    /// Starts signing in as `sub` and returns what the browser will bring back; OpenAI will issue `oaiapp_1` and
    /// tokens valid for `expires_in` seconds.
    fn begin_sign_in(
        chatgpt: &ChatGpt,
        server: &FakeServer,
        sub: &str,
        expires_in: u64,
        lifetime: Lifetime,
    ) -> Result<Callback, ChatGptError> {
        let url = chatgpt.start(REDIRECT.into(), lifetime)?;
        let claims = json!({
            "iss": OPENAI_ISSUER, "aud": "oaiapp_1", "sub": sub, "email": format!("{sub}@example.com"),
            "nonce": param(&url, "nonce"), "exp": unix_now() + 3600,
        });
        let tokens = json!({
            "access_token": format!("access-{sub}"), "refresh_token": format!("refresh-{sub}"),
            "expires_in": expires_in, "scope": GRANTED, "id_token": id_token(&claims),
        });
        server.respond(TOKEN, "200 OK", tokens.to_string());
        Ok(Callback {
            code: Some("code".into()),
            state: param(&url, "state"),
            client_id: Some("oaiapp_1".into()),
            error: None,
        })
    }

    /// Signs in as `sub`, as [`begin_sign_in`] describes.
    pub(crate) async fn sign_in(
        chatgpt: &ChatGpt,
        server: &FakeServer,
        sub: &str,
        expires_in: u64,
    ) -> Result<Account, ChatGptError> {
        let callback = begin_sign_in(chatgpt, server, sub, expires_in, Lifetime::Run)?;
        chatgpt.finish(&callback).await
    }

    fn soon() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(1)
    }

    fn requests_with(server: &FakeServer, text: &str) -> Vec<String> {
        server
            .requests()
            .into_iter()
            .filter(|request| request.contains(text))
            .collect()
    }

    async fn fixture() -> (tempfile::TempDir, Home, FakeServer, ChatGpt) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let (server, endpoints) = fake_openai().await;
        let chatgpt = ChatGpt::new(&home, Provider::new().unwrap(), endpoints, Events::new());
        (dir, home, server, chatgpt)
    }

    #[tokio::test]
    async fn a_kelpy_restarted_in_another_process_goes_on_with_the_session_and_its_token() {
        let (_dir, home, server, before) = fixture().await;
        sign_in(&before, &server, "user-1", 3600).await.unwrap();
        let state = serde_json::to_string(&before.handed_over().unwrap()).unwrap();
        let endpoints = before.endpoints.clone();
        let after = ChatGpt::new(&home, Provider::new().unwrap(), endpoints, Events::new());
        assert!(!after.signed_in());
        after.take_over(serde_json::from_str(&state).unwrap());
        let status = after.status();
        assert_eq!(status.signed_in.unwrap().sub, "user-1");
        assert!(!status.kept);
        // The same access token, not renewed: the session goes on as it was.
        let mut settings = Settings {
            kind: kelpy_providers::Kind::ChatGpt,
            model: "gpt-5".into(),
            base_url: None,
            api_key: None,
            headers: Vec::new(),
        };
        after.authorize(&mut settings).await.unwrap();
        assert_eq!(settings.api_key.as_deref(), Some("access-user-1"));
        assert!(requests_with(&server, "refresh-user-1").is_empty());
        // Nothing of it was written to disk.
        assert!(!after.session_path.exists());
    }

    #[tokio::test]
    async fn signing_in_and_out_is_published() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        let mut events = chatgpt.events.subscribe();
        // Each step tells the interfaces to read the provider again: a mark ends those of the sign-in.
        sign_in(&chatgpt, &server, "user-1", 3600).await.unwrap();
        chatgpt.events.publish(Event::Resync);
        let mut published = Vec::new();
        while let Some(event) = events.next().await.filter(|event| *event != Event::Resync) {
            published.push(event);
        }
        assert!(!published.is_empty());
        assert!(
            published
                .iter()
                .all(|event| *event == Event::ProviderChanged)
        );

        chatgpt.sign_out(soon()).await;
        assert_eq!(events.next().await, Some(Event::ProviderChanged));
    }

    #[tokio::test]
    async fn signing_in_keeps_only_the_registration_on_disk() {
        let (_dir, home, server, chatgpt) = fixture().await;
        let account = sign_in(&chatgpt, &server, "user-1", 3600).await.unwrap();
        assert_eq!(account.sub, "user-1");
        assert_eq!(chatgpt.access_token().await.unwrap(), "access-user-1");

        let saved = fs::read_to_string(home.data_dir().join(REGISTRATION_FILE)).unwrap();
        for kept in ["oaiapp_1", "urn:uuid:", "user-1@example.com"] {
            assert!(saved.contains(kept), "{kept} missing from {saved}");
        }
        assert!(
            !saved.contains("access-") && !saved.contains("refresh-"),
            "{saved}"
        );
        assert!(!home.data_dir().join(SESSION_FILE).exists());

        // After a restart the account is remembered, the session is not.
        let restarted = ChatGpt::new(
            &home,
            Provider::new().unwrap(),
            chatgpt.endpoints.clone(),
            Events::new(),
        );
        let status = restarted.status();
        assert!(status.signed_in.is_none());
        assert_eq!(status.registered.as_deref(), Some("user-1@example.com"));
        let url = restarted.start(REDIRECT.into(), Lifetime::Run).unwrap();
        assert_eq!(param(&url, "client_id").as_deref(), Some("oaiapp_1"));
        assert_eq!(param(&url, "agent_name_hint"), None);
    }

    #[tokio::test]
    async fn the_issued_client_is_kept_even_if_the_exchange_fails() {
        let (_dir, home, server, chatgpt) = fixture().await;
        let url = chatgpt.start(REDIRECT.into(), Lifetime::Run).unwrap();
        assert_eq!(
            param(&url, "client_id").as_deref(),
            Some("dynamic_agent_client")
        );
        server.respond(
            "POST /api/accounts/oauth/token",
            "500 Internal Server Error",
            String::new(),
        );
        let callback = Callback {
            code: Some("code".into()),
            state: param(&url, "state"),
            client_id: Some("oaiapp_1".into()),
            error: None,
        };
        assert!(chatgpt.finish(&callback).await.is_err());

        // Kept on disk, so even after a restart the next sign-in reuses the client.
        let restarted = ChatGpt::new(
            &home,
            Provider::new().unwrap(),
            chatgpt.endpoints.clone(),
            Events::new(),
        );
        let url = restarted.start(REDIRECT.into(), Lifetime::Run).unwrap();
        assert_eq!(param(&url, "client_id").as_deref(), Some("oaiapp_1"));
    }

    #[tokio::test]
    async fn tokens_are_renewed_once_before_they_expire() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        // Valid for one more minute: inside the margin, so the next request renews it first.
        sign_in(&chatgpt, &server, "user-1", 60).await.unwrap();
        let renewed =
            json!({ "access_token": "access-2", "refresh_token": "refresh-2", "expires_in": 3600 });
        server.respond(
            "POST /api/accounts/oauth/token",
            "200 OK",
            renewed.to_string(),
        );

        let (first, second) = tokio::join!(chatgpt.access_token(), chatgpt.access_token());
        assert_eq!(first.unwrap(), "access-2");
        assert_eq!(second.unwrap(), "access-2");
        let refreshes = requests_with(&server, "grant_type=refresh_token");
        assert_eq!(refreshes.len(), 1);
        assert!(refreshes[0].contains("refresh_token=refresh-user-1"));
    }

    #[tokio::test]
    async fn an_ended_session_must_sign_in_again() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        sign_in(&chatgpt, &server, "user-1", 60).await.unwrap();
        server.respond(
            "POST /api/accounts/oauth/token",
            "400 Bad Request",
            json!({ "error": "refresh_token_expired" }).to_string(),
        );
        assert!(matches!(
            chatgpt.access_token().await,
            Err(ChatGptError::SignIn(SignInError::Ended))
        ));
        assert!(matches!(
            chatgpt.access_token().await,
            Err(ChatGptError::NotSignedIn)
        ));
        assert!(chatgpt.status().signed_in.is_none());
    }

    #[tokio::test]
    async fn signing_out_revokes_the_session_and_keeps_the_registration() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        sign_in(&chatgpt, &server, "user-1", 3600).await.unwrap();
        assert!(chatgpt.sign_out(soon()).await.revoked);
        let revoked = requests_with(&server, "token_type_hint=refresh_token");
        assert_eq!(revoked.len(), 1);
        assert!(revoked[0].contains("token=refresh-user-1"));
        assert!(revoked[0].contains("client_id=oaiapp_1"));
        assert!(matches!(
            chatgpt.access_token().await,
            Err(ChatGptError::NotSignedIn)
        ));
        assert!(chatgpt.status().registered.is_some());
        // Nothing left to revoke.
        assert!(chatgpt.sign_out(soon()).await.revoked);
        assert_eq!(requests_with(&server, "token_type_hint").len(), 1);
    }

    #[tokio::test]
    async fn a_kept_session_survives_a_restart_until_a_sign_out() {
        let (_dir, home, server, chatgpt) = fixture().await;
        let callback = begin_sign_in(&chatgpt, &server, "user-1", 3600, Lifetime::Kept).unwrap();
        chatgpt.finish(&callback).await.unwrap();
        assert!(chatgpt.status().kept);
        let path = home.data_dir().join(SESSION_FILE);
        let saved = fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("refresh-user-1") && saved.contains("oaiapp_1"),
            "{saved}"
        );
        assert!(!saved.contains("access-"), "{saved}");

        // Stopping Kelpy leaves it signed in: nothing is revoked.
        assert!(chatgpt.close(soon()).await);
        assert!(requests_with(&server, "token_type_hint").is_empty());

        // After a restart it is signed in, and its first use renews the token, on disk too.
        let renewed =
            json!({ "access_token": "access-2", "refresh_token": "refresh-2", "expires_in": 3600 });
        server.respond(TOKEN, "200 OK", renewed.to_string());
        let restarted = ChatGpt::new(
            &home,
            Provider::new().unwrap(),
            chatgpt.endpoints.clone(),
            Events::new(),
        );
        let status = restarted.status();
        assert_eq!(status.signed_in.unwrap().sub, "user-1");
        assert!(status.kept);
        assert_eq!(restarted.access_token().await.unwrap(), "access-2");
        let refreshes = requests_with(&server, "grant_type=refresh_token");
        assert!(refreshes[0].contains("refresh_token=refresh-user-1"));
        assert!(fs::read_to_string(&path).unwrap().contains("refresh-2"));

        // Signing out ends it everywhere.
        assert!(restarted.sign_out(soon()).await.revoked);
        assert!(!path.exists());
        let again = ChatGpt::new(
            &home,
            Provider::new().unwrap(),
            chatgpt.endpoints.clone(),
            Events::new(),
        );
        assert!(again.status().signed_in.is_none());
    }

    #[tokio::test]
    async fn a_sign_in_for_the_run_removes_a_kept_session_from_disk() {
        let (_dir, home, server, chatgpt) = fixture().await;
        let callback = begin_sign_in(&chatgpt, &server, "user-1", 3600, Lifetime::Kept).unwrap();
        chatgpt.finish(&callback).await.unwrap();
        sign_in(&chatgpt, &server, "user-1", 3600).await.unwrap();
        assert!(!chatgpt.status().kept);
        assert!(!home.data_dir().join(SESSION_FILE).exists());
        // And stopping Kelpy ends it.
        let before = requests_with(&server, "token_type_hint").len();
        assert!(chatgpt.close(soon()).await);
        assert_eq!(requests_with(&server, "token_type_hint").len(), before + 1);
    }

    #[tokio::test]
    async fn a_kept_code_session_comes_back_as_a_codex_session() {
        let (_dir, home, server, chatgpt) = fixture().await;
        let sign_in = chatgpt.start_code(Lifetime::Kept).await.unwrap();
        approve_code(&server, "user-1", 3600);
        chatgpt.finish_code(sign_in).await.unwrap();
        let restarted = ChatGpt::new(
            &home,
            Provider::new().unwrap(),
            chatgpt.endpoints.clone(),
            Events::new(),
        );
        let status = restarted.status();
        assert_eq!(status.method, Some(Method::Codex));
        assert!(status.kept);
    }

    #[tokio::test]
    async fn a_kept_agent_session_of_another_registration_is_not_restored() {
        let (_dir, home, server, chatgpt) = fixture().await;
        let callback = begin_sign_in(&chatgpt, &server, "user-1", 3600, Lifetime::Kept).unwrap();
        chatgpt.finish(&callback).await.unwrap();
        let path = home.data_dir().join(SESSION_FILE);
        let other = fs::read_to_string(&path)
            .unwrap()
            .replace("oaiapp_1", "oaiapp_other");
        fs::write(&path, other).unwrap();
        let restarted = ChatGpt::new(
            &home,
            Provider::new().unwrap(),
            chatgpt.endpoints.clone(),
            Events::new(),
        );
        assert!(restarted.status().signed_in.is_none());
    }

    #[tokio::test]
    async fn another_account_cannot_take_over_the_registered_client() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        sign_in(&chatgpt, &server, "user-1", 3600).await.unwrap();

        let other = sign_in(&chatgpt, &server, "user-2", 3600).await;
        assert!(matches!(other, Err(ChatGptError::OtherAccount)));
        assert_eq!(chatgpt.access_token().await.unwrap(), "access-user-1");
        assert_eq!(
            requests_with(&server, "token=refresh-user-2").len(),
            1,
            "the unused tokens are revoked"
        );

        // Forgetting the registration lets another account register, once its session has ended.
        assert!(matches!(
            chatgpt.forget_registration(),
            Err(ChatGptError::RegistrationInUse)
        ));
        chatgpt.sign_out(soon()).await;
        chatgpt.forget_registration().unwrap();
        assert_eq!(chatgpt.status().registered, None);
        let account = sign_in(&chatgpt, &server, "user-2", 3600).await.unwrap();
        assert_eq!(account.sub, "user-2");
        assert_eq!(
            chatgpt.status().registered.as_deref(),
            Some("user-2@example.com")
        );
    }

    #[tokio::test]
    async fn forgetting_the_registration_abandons_a_sign_in_under_way() {
        let (_dir, home, server, chatgpt) = fixture().await;
        sign_in(&chatgpt, &server, "user-1", 3600).await.unwrap();
        chatgpt.sign_out(soon()).await;
        let callback = begin_sign_in(&chatgpt, &server, "user-1", 3600, Lifetime::Run).unwrap();
        chatgpt.forget_registration().unwrap();
        assert!(chatgpt.finish(&callback).await.is_err());
        assert!(chatgpt.status().signed_in.is_none());
        let restarted = ChatGpt::new(
            &home,
            Provider::new().unwrap(),
            chatgpt.endpoints.clone(),
            Events::new(),
        );
        assert_eq!(restarted.status().registered, None);
    }

    #[tokio::test]
    async fn a_callback_without_a_sign_in_under_way_is_refused() {
        let (_dir, home, _server, chatgpt) = fixture().await;
        let forged = Callback {
            code: Some("code".into()),
            state: Some("guessed".into()),
            client_id: Some("oaiapp_9".into()),
            error: None,
        };
        assert!(matches!(
            chatgpt.finish(&forged).await,
            Err(ChatGptError::SignIn(SignInError::UnknownAttempt))
        ));
        assert!(!home.data_dir().join(REGISTRATION_FILE).exists());
    }

    #[tokio::test]
    async fn signing_out_does_not_wait_for_a_renewal_which_then_cannot_revive_the_session() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        sign_in(&chatgpt, &server, "user-1", 60).await.unwrap();
        let renewed =
            json!({ "access_token": "access-2", "refresh_token": "refresh-2", "expires_in": 3600 });
        server.respond(TOKEN, "200 OK", renewed.to_string());
        let renewal = server.hold(TOKEN);

        let request = chatgpt.access_token();
        tokio::pin!(request);
        tokio::select! {
            _ = &mut request => panic!("the renewal was held"),
            () = server.received("grant_type=refresh_token") => {}
        }
        let signed_out =
            tokio::time::timeout(Duration::from_secs(2), chatgpt.sign_out(soon())).await;
        assert_eq!(
            signed_out.ok().map(|outcome| outcome.revoked),
            Some(true),
            "sign-out waited for the renewal"
        );

        renewal.open();
        assert!(matches!(request.await, Err(ChatGptError::NotSignedIn)));
        assert!(chatgpt.status().signed_in.is_none());
        server.received("token=refresh-2&token_type_hint").await;
    }

    #[tokio::test]
    async fn a_sign_out_that_cannot_remove_the_kept_session_says_so() {
        let (_dir, home, server, chatgpt) = fixture().await;
        let callback = begin_sign_in(&chatgpt, &server, "user-1", 3600, Lifetime::Kept).unwrap();
        chatgpt.finish(&callback).await.unwrap();
        // A folder in its place cannot be removed as a file, like a file another program holds open.
        let kept = home.data_dir().join(SESSION_FILE);
        fs::remove_file(&kept).unwrap();
        fs::create_dir(&kept).unwrap();
        server.respond(REVOKE, "500 Internal Server Error", String::new());

        let signed_out = chatgpt.sign_out(soon()).await;
        assert_eq!(
            signed_out,
            SignedOut {
                revoked: false,
                forgotten: false
            }
        );
        assert!(chatgpt.status().signed_in.is_none());
    }

    #[tokio::test]
    async fn signing_out_waits_for_openai_until_its_deadline_only() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        sign_in(&chatgpt, &server, "user-1", 3600).await.unwrap();
        let _revocation = server.hold(REVOKE);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let signed_out =
            tokio::time::timeout(Duration::from_secs(2), chatgpt.sign_out(deadline)).await;
        assert_eq!(
            signed_out.ok().map(|outcome| outcome.revoked),
            Some(false),
            "unconfirmed, and on time"
        );
        assert!(chatgpt.status().signed_in.is_none());
    }

    #[tokio::test]
    async fn a_sign_in_completing_after_a_sign_out_is_refused_and_revoked() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        let callback = begin_sign_in(&chatgpt, &server, "user-1", 3600, Lifetime::Run).unwrap();
        let exchange = server.hold(TOKEN);

        let finish = chatgpt.finish(&callback);
        tokio::pin!(finish);
        tokio::select! {
            _ = &mut finish => panic!("the exchange was held"),
            () = server.received("grant_type=authorization_code") => {}
        }
        assert!(chatgpt.sign_out(soon()).await.revoked);

        exchange.open();
        assert!(matches!(finish.await, Err(ChatGptError::NotSignedIn)));
        assert!(chatgpt.status().signed_in.is_none());
        assert!(matches!(
            chatgpt.access_token().await,
            Err(ChatGptError::NotSignedIn)
        ));
        assert_eq!(
            requests_with(&server, "token=refresh-user-1&token_type_hint").len(),
            1
        );
    }

    #[tokio::test]
    async fn a_registration_that_cannot_be_saved_is_not_kept_in_memory() {
        let (_dir, home, _server, chatgpt) = fixture().await;
        let path = home.data_dir().join(REGISTRATION_FILE);
        // A folder where the file should be: writing it fails.
        fs::create_dir(&path).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                chatgpt.start(REDIRECT.into(), Lifetime::Run),
                Err(ChatGptError::Save(_))
            ));
        }
        fs::remove_dir(&path).unwrap();
        chatgpt.start(REDIRECT.into(), Lifetime::Run).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("urn:uuid:"));
    }

    fn settings_elsewhere() -> Settings {
        Settings {
            kind: kelpy_providers::Kind::ChatGpt,
            model: "gpt-test".into(),
            base_url: Some("https://elsewhere.example/v1".into()),
            api_key: None,
            headers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_code_sign_in_opens_a_session_for_the_codex_backend_only() {
        let (_dir, home, server, chatgpt) = fixture().await;
        let sign_in = chatgpt.start_code(Lifetime::Run).await.unwrap();
        assert_eq!(sign_in.code().user_code, "ABCD-1234");
        approve_code(&server, "user-1", 3600);
        assert_eq!(chatgpt.finish_code(sign_in).await.unwrap().sub, "user-1");
        let status = chatgpt.status();
        assert_eq!(status.method, Some(Method::Codex));
        assert!(status.registered.is_none());
        assert!(
            !home.data_dir().join(REGISTRATION_FILE).exists(),
            "nothing is registered"
        );

        let mut settings = settings_elsewhere();
        chatgpt.authorize(&mut settings).await.unwrap();
        assert_eq!(settings.base_url.as_ref(), Some(&chatgpt.endpoints.codex));
        assert!(
            settings
                .headers
                .contains(&("chatgpt-account-id", "workspace-1".into()))
        );

        assert!(chatgpt.sign_out(soon()).await.revoked);
        assert_eq!(
            requests_with(&server, "\"token\":\"refresh-user-1\"").len(),
            1,
            "revoked for the Codex CLI's client"
        );
    }

    #[tokio::test]
    async fn an_agent_session_sends_its_token_to_the_api_only() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        sign_in(&chatgpt, &server, "user-1", 3600).await.unwrap();
        assert_eq!(chatgpt.status().method, Some(Method::Agent));
        let mut settings = settings_elsewhere();
        chatgpt.authorize(&mut settings).await.unwrap();
        assert_eq!(settings.base_url.as_ref(), Some(&chatgpt.endpoints.api));
        assert_eq!(settings.api_key.as_deref(), Some("access-user-1"));
        assert!(settings.headers.is_empty());
    }

    #[tokio::test]
    async fn a_code_session_is_renewed_with_the_codex_cli_client() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        let sign_in = chatgpt.start_code(Lifetime::Run).await.unwrap();
        approve_code(&server, "user-1", 60);
        chatgpt.finish_code(sign_in).await.unwrap();
        let renewed =
            json!({ "access_token": "access-2", "refresh_token": "refresh-2", "expires_in": 3600 });
        server.respond(CODE_TOKEN, "200 OK", renewed.to_string());
        assert_eq!(chatgpt.access_token().await.unwrap(), "access-2");
        let renewals = requests_with(&server, "\"grant_type\":\"refresh_token\"");
        assert_eq!(renewals.len(), 1);
        assert!(renewals[0].contains("\"refresh_token\":\"refresh-user-1\""));
    }

    #[tokio::test]
    async fn a_code_sign_in_ends_when_another_sign_in_or_a_sign_out_takes_its_place() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        let first = chatgpt.start_code(Lifetime::Run).await.unwrap();
        let waiting = chatgpt.finish_code(first);
        tokio::pin!(waiting);
        tokio::select! {
            _ = &mut waiting => panic!("not approved yet"),
            () = server.received("\"device_auth_id\"") => {}
        }
        let second = chatgpt.start_code(Lifetime::Run).await.unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(2), waiting).await;
        assert!(
            matches!(ended, Ok(Err(ChatGptError::Abandoned))),
            "replaced"
        );

        let waiting = chatgpt.finish_code(second);
        tokio::pin!(waiting);
        tokio::select! {
            _ = &mut waiting => panic!("not approved yet"),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        assert!(chatgpt.sign_out(soon()).await.revoked);
        let ended = tokio::time::timeout(Duration::from_secs(2), waiting).await;
        assert!(
            matches!(ended, Ok(Err(ChatGptError::Abandoned))),
            "signed out"
        );
        assert!(chatgpt.status().signed_in.is_none());
    }

    /// Starts a sign-in with a code whose request OpenAI holds; returns it once the request arrived.
    async fn start_code_held(
        chatgpt: &ChatGpt,
        server: &FakeServer,
    ) -> (
        impl Future<Output = Result<CodeSignIn, ChatGptError>>,
        kelpy_test_support::Gate,
    ) {
        let gate = server.hold(USER_CODE);
        let mut start = Box::pin(chatgpt.start_code(Lifetime::Run));
        tokio::select! {
            _ = &mut start => panic!("the code request was held"),
            () = server.received("/deviceauth/usercode") => {}
        }
        (start, gate)
    }

    #[tokio::test]
    async fn a_code_issued_after_a_sign_out_is_abandoned() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        let (start, gate) = start_code_held(&chatgpt, &server).await;
        assert!(chatgpt.sign_out(soon()).await.revoked);
        gate.open();
        let started = start.await;
        assert!(matches!(started, Err(ChatGptError::Abandoned)), "issued");
    }

    #[tokio::test]
    async fn a_code_issued_late_leaves_a_newer_sign_in_under_way() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        let (start, gate) = start_code_held(&chatgpt, &server).await;
        let callback = begin_sign_in(&chatgpt, &server, "user-1", 3600, Lifetime::Run).unwrap();
        gate.open();
        assert!(matches!(start.await, Err(ChatGptError::Abandoned)));
        assert!(chatgpt.finish(&callback).await.is_ok());
    }

    #[tokio::test]
    async fn a_code_not_approved_in_time_expires() {
        let (_dir, _home, _server, chatgpt) = fixture().await;
        let mut sign_in = chatgpt.start_code(Lifetime::Run).await.unwrap();
        sign_in.deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        let ended =
            tokio::time::timeout(Duration::from_secs(2), chatgpt.finish_code(sign_in)).await;
        assert!(matches!(ended, Ok(Err(ChatGptError::Expired))));
    }

    #[tokio::test]
    async fn a_stray_agent_callback_leaves_a_code_sign_in_under_way() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        let sign_in = chatgpt.start_code(Lifetime::Run).await.unwrap();
        let stray = Callback {
            code: Some("code".into()),
            state: Some("guessed".into()),
            client_id: None,
            error: None,
        };
        assert!(matches!(
            chatgpt.finish(&stray).await,
            Err(ChatGptError::SignIn(SignInError::UnknownAttempt))
        ));
        approve_code(&server, "user-1", 3600);
        assert!(chatgpt.finish_code(sign_in).await.is_ok());
    }

    #[tokio::test]
    async fn a_code_approved_before_a_sign_out_is_refused_and_revoked() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        let sign_in = chatgpt.start_code(Lifetime::Run).await.unwrap();
        approve_code(&server, "user-1", 3600);
        let exchange = server.hold(CODE_TOKEN);
        let finish = chatgpt.finish_code(sign_in);
        tokio::pin!(finish);
        tokio::select! {
            _ = &mut finish => panic!("the exchange was held"),
            () = server.received("grant_type=authorization_code") => {}
        }
        assert!(chatgpt.sign_out(soon()).await.revoked);

        exchange.open();
        assert!(matches!(finish.await, Err(ChatGptError::Abandoned)));
        assert!(chatgpt.status().signed_in.is_none());
        server.received("\"token\":\"refresh-user-1\"").await;
    }

    #[tokio::test]
    async fn the_agent_registration_moves_to_another_home_without_its_host_id() {
        let (_dir, _home, server, chatgpt) = fixture().await;
        assert!(chatgpt.agent_file().is_none(), "nothing verified yet");
        sign_in(&chatgpt, &server, "user-1", 3600).await.unwrap();
        let file = chatgpt.agent_file().unwrap();
        assert!(file.contains("oaiapp_1") && file.contains("user-1@example.com"));
        assert!(
            !file.contains("host_id") && !file.contains("refresh-"),
            "{file}"
        );
        let own_host = param(
            &chatgpt.start(REDIRECT.into(), Lifetime::Run).unwrap(),
            "ext_agent_host_id",
        );

        let other_dir = tempfile::tempdir().unwrap();
        let other_home = Home::init(other_dir.path()).unwrap();
        let other = ChatGpt::new(
            &other_home,
            Provider::new().unwrap(),
            chatgpt.endpoints.clone(),
            Events::new(),
        );
        let with_host = format!("host_id = \"urn:uuid:copied\"\n{file}");
        assert_eq!(
            other.use_agent_file(&with_host).unwrap().as_deref(),
            Some("user-1@example.com")
        );
        assert_eq!(
            other.status().registered.as_deref(),
            Some("user-1@example.com")
        );
        let url = other.start(REDIRECT.into(), Lifetime::Run).unwrap();
        assert_eq!(param(&url, "client_id").as_deref(), Some("oaiapp_1"));
        assert_eq!(param(&url, "agent_name_hint"), None, "no new registration");
        let host = param(&url, "ext_agent_host_id").unwrap();
        assert!(!host.contains("copied"));
        assert_ne!(Some(host), own_host, "each home keeps its own host id");

        assert!(matches!(
            sign_in(&other, &server, "user-2", 3600).await,
            Err(ChatGptError::OtherAccount)
        ));
        assert!(matches!(
            other.use_agent_file("client = 3"),
            Err(ChatGptError::InvalidAgentFile)
        ));
        assert!(matches!(
            other.use_agent_file("[client]\nid = \"oaiapp_9\"\n"),
            Err(ChatGptError::InvalidAgentFile)
        ));
    }
}
