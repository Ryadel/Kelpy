//! Signing in with ChatGPT from an interface.
//!
//! | Route | |
//! |---|---|
//! | `GET /api/chatgpt` | the account signed in and how, and the one Kelpy is registered for as an agent |
//! | `POST /api/chatgpt/code` | signs in as the Codex CLI (`keep`); streams the code to approve, then the outcome, as SSE |
//! | `POST /api/chatgpt/sign-in` | starts a sign-in as an agent (`keep`); returns the OpenAI address |
//! | `POST /api/chatgpt/callback` | completes it with the parameters the browser brought back |
//! | `GET /api/chatgpt/agent` | the agent registration, as a file for another Kelpy |
//! | `PUT /api/chatgpt/agent` | uses the agent registration file of another Kelpy |
//! | `DELETE /api/chatgpt/agent` | forgets the agent registration; returns the status |
//! | `GET /api/chatgpt/models` | the models the account may choose |
//! | `POST /api/chatgpt/sign-out` | ends the session; `{revoked, forgotten}`: revoked by OpenAI, removed from disk |
//!
//! A sign-in with a code lasts as long as its stream: closing the stream abandons it. As an agent, OpenAI sends the
//! browser back to `http://127.0.0.1:<port>/auth/callback`, a page of the Web UI, which posts the parameters here
//! with its session.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{StreamExt, stream};
use kelpy_core::agent::Agent;
use kelpy_core::chatgpt::{ChatGptError, Lifetime, REVOKE_WAIT};
use kelpy_core::providers::chatgpt::codex::CODE_LIFETIME;
use kelpy_core::providers::chatgpt::{Account, CALLBACK_PATH, Callback, SignInError};
use qrcode::QrCode;
use qrcode::render::svg;
use serde::{Deserialize, Serialize};

use crate::Activity;
use crate::chat::error;

#[derive(Clone)]
pub struct ChatGptState {
    pub agent: Arc<Agent>,
    pub activity: Arc<Activity>,
    /// The port this server listens on, where OpenAI sends the browser back.
    pub port: u16,
}

pub fn routes() -> Router<ChatGptState> {
    Router::new()
        .route("/api/chatgpt", get(status))
        .route("/api/chatgpt/code", post(code))
        .route("/api/chatgpt/sign-in", post(sign_in))
        .route("/api/chatgpt/callback", post(callback))
        .route(
            "/api/chatgpt/agent",
            get(agent_file)
                .put(use_agent_file)
                .delete(forget_registration),
        )
        .route("/api/chatgpt/models", get(models))
        .route("/api/chatgpt/sign-out", post(sign_out))
}

async fn status(State(state): State<ChatGptState>) -> Response {
    Json(state.agent.chatgpt().status()).into_response()
}

/// An event of a sign-in with a code.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodeEvent {
    /// The code to approve, the address where, and a QR code of that address as an SVG image.
    Code {
        verification_url: String,
        user_code: String,
        /// Seconds the code can be approved.
        expires_in: u64,
        qr: String,
    },
    SignedIn {
        account: Account,
    },
    Failed {
        code: &'static str,
        error: String,
    },
}

/// How long the session of a sign-in lasts, as the interface asked.
#[derive(Deserialize, Default)]
struct Keep {
    /// Save the sign-in on this computer, across restarts; off unless the user turned it on.
    #[serde(default)]
    keep: bool,
}

impl Keep {
    fn lifetime(&self) -> Lifetime {
        if self.keep {
            Lifetime::Kept
        } else {
            Lifetime::Run
        }
    }
}

async fn code(State(state): State<ChatGptState>, Json(request): Json<Keep>) -> Response {
    let sign_in = match state.agent.chatgpt().start_code(request.lifetime()).await {
        Ok(sign_in) => sign_in,
        Err(failure) => return chatgpt_failure(&failure),
    };
    let issued = sign_in.code();
    let first = CodeEvent::Code {
        qr: qr_code(&issued.verification_url),
        verification_url: issued.verification_url.clone(),
        user_code: issued.user_code.clone(),
        expires_in: CODE_LIFETIME.as_secs(),
    };
    // Someone is signing in: Kelpy is not idle until the sign-in ends.
    let busy = state.activity.busy();
    let agent = state.agent;
    let outcome = async move {
        let _busy = busy;
        match agent.chatgpt().finish_code(sign_in).await {
            Ok(account) => CodeEvent::SignedIn { account },
            Err(failure) => CodeEvent::Failed {
                code: classify(&failure).1,
                error: failure.to_string(),
            },
        }
    };
    let events = stream::once(async { first })
        .chain(stream::once(outcome))
        .map(|event| {
            let data = serde_json::to_string(&event).unwrap_or_default();
            Ok::<_, Infallible>(Event::default().data(data))
        });
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `text` as a QR code, an SVG image; empty if it does not fit in one.
fn qr_code(text: &str) -> String {
    QrCode::new(text.as_bytes())
        .map(|code| {
            code.render::<svg::Color<'_>>()
                .min_dimensions(160, 160)
                .build()
        })
        .unwrap_or_default()
}

#[derive(Deserialize, Default)]
struct SignIn {
    #[serde(flatten)]
    keep: Keep,
}

#[derive(Serialize)]
struct SignInStarted {
    url: String,
}

async fn sign_in(State(state): State<ChatGptState>, Json(request): Json<SignIn>) -> Response {
    // OpenAI accepts only 127.0.0.1 here, never `localhost`.
    let redirect_uri = format!("http://127.0.0.1:{}{CALLBACK_PATH}", state.port);
    match state
        .agent
        .chatgpt()
        .start(redirect_uri, request.keep.lifetime())
    {
        Ok(url) => Json(SignInStarted { url }).into_response(),
        Err(failure) => chatgpt_failure(&failure),
    }
}

async fn callback(State(state): State<ChatGptState>, Json(callback): Json<Callback>) -> Response {
    match state.agent.chatgpt().finish(&callback).await {
        Ok(account) => Json(account).into_response(),
        Err(failure) => chatgpt_failure(&failure),
    }
}

/// An agent registration file's text.
#[derive(Serialize, Deserialize)]
struct AgentFile {
    text: String,
}

async fn agent_file(State(state): State<ChatGptState>) -> Response {
    match state.agent.chatgpt().agent_file() {
        Some(text) => Json(AgentFile { text }).into_response(),
        None => error(
            StatusCode::CONFLICT,
            "not_registered",
            "Kelpy is not registered as an agent yet: sign in as an agent first".into(),
        ),
    }
}

#[derive(Serialize)]
struct Registered {
    /// The email of the account the registration belongs to.
    registered: Option<String>,
}

async fn use_agent_file(
    State(state): State<ChatGptState>,
    Json(file): Json<AgentFile>,
) -> Response {
    match state.agent.chatgpt().use_agent_file(&file.text) {
        Ok(registered) => Json(Registered { registered }).into_response(),
        Err(failure) => chatgpt_failure(&failure),
    }
}

async fn forget_registration(State(state): State<ChatGptState>) -> Response {
    let chatgpt = state.agent.chatgpt();
    match chatgpt.forget_registration() {
        Ok(()) => Json(chatgpt.status()).into_response(),
        Err(failure) => chatgpt_failure(&failure),
    }
}

async fn models(State(state): State<ChatGptState>) -> Response {
    match state.agent.chatgpt().models().await {
        Ok(models) => Json(models).into_response(),
        Err(failure) => chatgpt_failure(&failure),
    }
}

async fn sign_out(State(state): State<ChatGptState>) -> Response {
    let deadline = tokio::time::Instant::now() + REVOKE_WAIT;
    Json(state.agent.chatgpt().sign_out(deadline).await).into_response()
}

pub fn chatgpt_failure(failure: &ChatGptError) -> Response {
    let (status, code) = classify(failure);
    error(status, code, failure.to_string())
}

/// `not_signed_in` when a new sign-in is needed, `sign_in_failed` when it did not complete, `invalid` for a file that
/// is not a registration, `signed_in` for a registration a session still uses, `provider` when OpenAI could not be
/// reached or answered unexpectedly.
fn classify(failure: &ChatGptError) -> (StatusCode, &'static str) {
    match failure {
        ChatGptError::NotSignedIn | ChatGptError::SignIn(SignInError::Ended) => {
            (StatusCode::CONFLICT, "not_signed_in")
        }
        ChatGptError::SignIn(SignInError::Provider(_)) => (StatusCode::BAD_GATEWAY, "provider"),
        ChatGptError::Save(_) | ChatGptError::SignIn(SignInError::NoRandom) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
        ChatGptError::InvalidAgentFile => (StatusCode::BAD_REQUEST, "invalid"),
        ChatGptError::RegistrationInUse => (StatusCode::CONFLICT, "signed_in"),
        ChatGptError::OtherAccount
        | ChatGptError::Abandoned
        | ChatGptError::Expired
        | ChatGptError::SignIn(_) => (StatusCode::BAD_REQUEST, "sign_in_failed"),
    }
}
