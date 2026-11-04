//! The conversation API: provider settings, conversations, and answers streamed as Server-Sent Events.
//!
//! | Route | |
//! |---|---|
//! | `GET /api/provider`, `PUT /api/provider` | provider settings; the API key is accepted but never returned |
//! | `POST /api/provider/models` | the models a service offers, for the setup; takes the key being entered |
//! | `GET /api/sessions`, `POST /api/sessions` | list, create |
//! | `PATCH /api/sessions/{id}`, `DELETE /api/sessions/{id}` | change the title, explanations, access or approval mode; delete |
//! | `GET /api/sessions/{id}/messages` | the messages with their actions, and whether an answer is being written |
//! | `POST /api/sessions/{id}/messages` | send a message; the answer streams back as SSE |
//! | `GET /api/sessions/{id}/answer` | follow the answer being written, from where it is, as SSE |
//! | `POST /api/sessions/{id}/cancel` | stop the answer being written |
//! | `POST /api/sessions/{id}/actions/{action}` | approve or deny an action waiting for a decision |
//! | `POST /api/sessions/{id}/actions/{action}/stop` | stop an action running; the answer goes on |
//! | `POST /api/sessions/{id}/actions/{action}/restore` | put a file back from the backup of a change; `preview` only shows it |
//! | `GET /api/sessions/{id}/report` | the handover report, in Markdown |
//! | `GET /api/sessions/{id}/export` | the operations that succeeded, as JSON |
//! | `GET /api/differences`, `POST /api/differences/check` | changes made outside Kelpy; check again now |
//! | `POST /api/differences/{id}/accept` | keep what was found |
//! | `POST /api/sessions/{id}/differences/{difference}/put-back` | put Kelpy's version back; `preview` only shows it |

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream;
use kelpy_core::agent::{
    Access, Action, Agent, AgentError, AnswerEvents, Approval, ChatEvent, ModelQuery, Modes,
    ProviderUpdate,
};
use kelpy_core::providers::ProviderError;
use kelpy_core::storage::{Message, StoreError};
use serde::{Deserialize, Serialize};

use crate::Activity;
use crate::chatgpt::chatgpt_failure;

#[derive(Clone)]
pub struct ChatState {
    pub agent: Arc<Agent>,
    pub activity: Arc<Activity>,
}

pub fn routes() -> Router<ChatState> {
    Router::new()
        .route("/api/provider", get(provider).put(configure_provider))
        .route("/api/provider/models", post(provider_models))
        .route("/api/sessions", get(sessions).post(create_session))
        .route(
            "/api/sessions/{id}",
            axum::routing::patch(update_session).delete(delete_session),
        )
        .route(
            "/api/sessions/{id}/messages",
            get(messages).post(send_message),
        )
        .route("/api/sessions/{id}/answer", get(follow_answer))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .route("/api/sessions/{id}/actions/{action}", post(decide))
        .route(
            "/api/sessions/{id}/actions/{action}/stop",
            post(stop_action),
        )
        .route("/api/sessions/{id}/actions/{action}/restore", post(restore))
        .route("/api/sessions/{id}/report", get(report))
        .route("/api/sessions/{id}/export", get(export))
        .route("/api/differences", get(differences))
        .route("/api/differences/check", post(check_differences))
        .route("/api/differences/{id}/accept", post(accept_difference))
        .route(
            "/api/sessions/{id}/differences/{difference}/put-back",
            post(put_back),
        )
}

async fn provider(State(state): State<ChatState>) -> Response {
    Json(state.agent.provider_info()).into_response()
}

async fn configure_provider(
    State(state): State<ChatState>,
    Json(update): Json<ProviderUpdate>,
) -> Response {
    match state.agent.configure_provider(update) {
        Ok(info) => Json(info).into_response(),
        Err(error) => failure(&error),
    }
}

/// A POST, not a GET: the request may carry the API key being entered.
async fn provider_models(
    State(state): State<ChatState>,
    Json(query): Json<ModelQuery>,
) -> Response {
    match state.agent.models(query).await {
        Ok(list) => Json(list).into_response(),
        Err(error) => failure(&error),
    }
}

async fn sessions(State(state): State<ChatState>) -> Response {
    match state.agent.sessions().await {
        Ok(sessions) => Json(sessions).into_response(),
        Err(error) => failure(&error),
    }
}

/// The modes are chosen before the first message, so that they apply to the first answer.
#[derive(Deserialize, Default)]
struct NewSession {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    explanations: bool,
    #[serde(default)]
    access: Access,
    #[serde(default)]
    approval: Approval,
}

/// The body is optional: empty means an untitled conversation with the default modes.
async fn create_session(State(state): State<ChatState>, body: axum::body::Bytes) -> Response {
    let request: NewSession = if body.iter().all(u8::is_ascii_whitespace) {
        NewSession::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(parse) => return error(StatusCode::BAD_REQUEST, "invalid", parse.to_string()),
        }
    };
    match state
        .agent
        .create_session(
            request.title.as_deref(),
            Modes {
                explanations: request.explanations,
                access: request.access,
                approval: request.approval,
            },
        )
        .await
    {
        Ok(session) => (StatusCode::CREATED, Json(session)).into_response(),
        Err(error) => failure(&error),
    }
}

/// The fields to change; at least one.
#[derive(Deserialize)]
struct SessionChange {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    explanations: Option<bool>,
    #[serde(default)]
    access: Option<Access>,
    #[serde(default)]
    approval: Option<Approval>,
}

async fn update_session(
    State(state): State<ChatState>,
    Path(id): Path<i64>,
    Json(change): Json<SessionChange>,
) -> Response {
    let mut session = None;
    if let Some(title) = &change.title {
        match state.agent.rename_session(id, title).await {
            Ok(renamed) => session = Some(renamed),
            Err(error) => return failure(&error),
        }
    }
    if let Some(explanations) = change.explanations {
        match state.agent.set_explanations(id, explanations).await {
            Ok(changed) => session = Some(changed),
            Err(error) => return failure(&error),
        }
    }
    if let Some(access) = change.access {
        match state.agent.set_access(id, access).await {
            Ok(changed) => session = Some(changed),
            Err(error) => return failure(&error),
        }
    }
    if let Some(approval) = change.approval {
        match state.agent.set_approval(id, approval).await {
            Ok(changed) => session = Some(changed),
            Err(error) => return failure(&error),
        }
    }
    match session {
        Some(session) => Json(session).into_response(),
        None => error(
            StatusCode::BAD_REQUEST,
            "invalid",
            "nothing to change".into(),
        ),
    }
}

async fn delete_session(State(state): State<ChatState>, Path(id): Path<i64>) -> Response {
    match state.agent.delete_session(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(&error),
    }
}

#[derive(Serialize)]
struct Conversation {
    messages: Vec<Message>,
    /// The operations the administrator asked for, such as putting a file back: in the Command log, not in answers.
    administrator_actions: Vec<Action>,
    /// An answer is being written: its latest saved text is in `messages`, and it will complete on its own.
    running: bool,
    /// The last message the summary given to the model covers, if the older messages are summarised.
    summarised_through: Option<i64>,
}

async fn messages(State(state): State<ChatState>, Path(id): Path<i64>) -> Response {
    let conversation = async {
        Ok::<_, AgentError>(Conversation {
            messages: state.agent.messages(id).await?,
            administrator_actions: state.agent.administrator_actions(id).await?,
            running: state.agent.is_running(id),
            summarised_through: state.agent.summarised_through(id).await?,
        })
    };
    match conversation.await {
        Ok(conversation) => Json(conversation).into_response(),
        Err(error) => failure(&error),
    }
}

#[derive(Deserialize)]
struct Send {
    content: String,
}

/// Starts the answer and streams its events. Closing the stream does not stop the answer: it is saved anyway.
async fn send_message(
    State(state): State<ChatState>,
    Path(id): Path<i64>,
    Json(request): Json<Send>,
) -> Response {
    match state
        .agent
        .send(id, &request.content, state.activity.busy())
        .await
    {
        Ok(answer) => stream_answer(answer),
        Err(error) => failure(&error),
    }
}

/// Streams the answer being written, for an interface that did not start it or reconnected.
async fn follow_answer(State(state): State<ChatState>, Path(id): Path<i64>) -> Response {
    match state.agent.follow(id) {
        Some(answer) => stream_answer(answer),
        None => not_running(),
    }
}

fn stream_answer(answer: AnswerEvents) -> Response {
    let events = stream::unfold(answer, |mut answer| async move {
        let event = answer.next().await?;
        let name = match &event {
            ChatEvent::User { .. } => "user",
            ChatEvent::Started { .. } => "started",
            ChatEvent::Delta { .. } => "delta",
            ChatEvent::Action { .. } => "action",
            ChatEvent::Done { .. } => "done",
        };
        let data = serde_json::to_string(&event).unwrap_or_default();
        Some((
            Ok::<_, Infallible>(Event::default().event(name).data(data)),
            answer,
        ))
    });
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn not_running() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "not_running",
        "no answer is being written".into(),
    )
}

async fn cancel(State(state): State<ChatState>, Path(id): Path<i64>) -> Response {
    if state.agent.cancel(id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_running()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Approve,
    Deny,
}

#[derive(Deserialize)]
struct Decision {
    decision: Verdict,
    /// With an approval: the Approval mode to switch the conversation to, in which it would not have asked.
    #[serde(default)]
    switch_to: Option<Approval>,
}

async fn decide(
    State(state): State<ChatState>,
    Path((id, action)): Path<(i64, i64)>,
    Json(decision): Json<Decision>,
) -> Response {
    let approve = matches!(decision.decision, Verdict::Approve);
    match state
        .agent
        .decide(id, action, approve, decision.switch_to)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(&error),
    }
}

async fn stop_action(
    State(state): State<ChatState>,
    Path((id, action)): Path<(i64, i64)>,
) -> Response {
    match state.agent.stop_action(id, action) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(&error),
    }
}

/// An operation of the administrator: shown first with `preview`, then run.
#[derive(Deserialize, Default)]
struct Operation {
    #[serde(default)]
    preview: bool,
}

/// The body is optional: empty runs the operation.
fn operation(body: &[u8]) -> Result<Operation, serde_json::Error> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Operation::default());
    }
    serde_json::from_slice(body)
}

async fn restore(
    State(state): State<ChatState>,
    Path((id, action)): Path<(i64, i64)>,
    body: axum::body::Bytes,
) -> Response {
    let operation = match operation(&body) {
        Ok(operation) => operation,
        Err(parse) => return error(StatusCode::BAD_REQUEST, "invalid", parse.to_string()),
    };
    // Kelpy does not stop for inactivity while it changes a file.
    let _busy = state.activity.busy();
    match state.agent.restore(id, action, operation.preview).await {
        Ok(operated) => Json(operated).into_response(),
        Err(error) => failure(&error),
    }
}

async fn put_back(
    State(state): State<ChatState>,
    Path((id, difference)): Path<(i64, i64)>,
    body: axum::body::Bytes,
) -> Response {
    let operation = match operation(&body) {
        Ok(operation) => operation,
        Err(parse) => return error(StatusCode::BAD_REQUEST, "invalid", parse.to_string()),
    };
    let _busy = state.activity.busy();
    match state
        .agent
        .put_back(id, difference, operation.preview)
        .await
    {
        Ok(operated) => Json(operated).into_response(),
        Err(error) => failure(&error),
    }
}

async fn report(State(state): State<ChatState>, Path(id): Path<i64>) -> Response {
    match state.agent.report(id).await {
        Ok(report) => (
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            report,
        )
            .into_response(),
        Err(error) => failure(&error),
    }
}

async fn export(State(state): State<ChatState>, Path(id): Path<i64>) -> Response {
    match state.agent.export(id).await {
        Ok(export) => Json(export).into_response(),
        Err(error) => failure(&error),
    }
}

async fn differences(State(state): State<ChatState>) -> Response {
    match state.agent.differences().await {
        Ok(differences) => Json(differences).into_response(),
        Err(error) => failure(&error),
    }
}

/// Checks the server now, then lists the differences.
async fn check_differences(State(state): State<ChatState>) -> Response {
    let _busy = state.activity.busy();
    if let Err(error) = state.agent.check_server().await {
        return failure(&error);
    }
    differences(State(state)).await
}

async fn accept_difference(State(state): State<ChatState>, Path(id): Path<i64>) -> Response {
    match state.agent.accept_difference(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(&error),
    }
}

#[derive(Serialize)]
struct ErrorBody {
    /// Stable, for the interfaces: `not_configured`, `missing_key`, `missing_base_url`, `busy`, `stopping`,
    /// `deleting`, `not_found`, `not_running`, `not_waiting`, `invalid`, `internal`, and those of [`chatgpt_failure`].
    code: &'static str,
    error: String,
}

fn failure(failure: &AgentError) -> Response {
    let (status, code) = match failure {
        AgentError::NotConfigured => (StatusCode::CONFLICT, "not_configured"),
        AgentError::Provider(ProviderError::MissingKey { .. }) => {
            (StatusCode::CONFLICT, "missing_key")
        }
        AgentError::Provider(ProviderError::MissingBaseUrl) => {
            (StatusCode::BAD_REQUEST, "missing_base_url")
        }
        AgentError::Busy => (StatusCode::CONFLICT, "busy"),
        AgentError::Stopping => (StatusCode::SERVICE_UNAVAILABLE, "stopping"),
        AgentError::Deleting => (StatusCode::CONFLICT, "deleting"),
        AgentError::NotWaiting => (StatusCode::CONFLICT, "not_waiting"),
        AgentError::NotRunning => (StatusCode::CONFLICT, "not_running"),
        AgentError::EmptyMessage | AgentError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid"),
        AgentError::Store(
            StoreError::SessionNotFound(_)
            | StoreError::ActionNotFound(_)
            | StoreError::DifferenceNotFound(_),
        ) => (StatusCode::NOT_FOUND, "not_found"),
        AgentError::ChatGpt(failure) => return chatgpt_failure(failure),
        AgentError::Provider(_)
        | AgentError::Store(_)
        | AgentError::Config(_)
        | AgentError::NotStarted => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    error(status, code, failure.to_string())
}

pub(crate) fn error(status: StatusCode, code: &'static str, message: String) -> Response {
    (
        status,
        Json(ErrorBody {
            code,
            error: message,
        }),
    )
        .into_response()
}
