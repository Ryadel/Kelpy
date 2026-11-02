//! Local models with Ollama, for the provider setup.
//!
//! | Route | |
//! |---|---|
//! | `GET /api/ollama?base_url=…` | whether Ollama answers, its models, the models suggested for this computer, downloads |
//! | `POST /api/ollama/download` | downloads a model, or follows its download; the progress streams back as SSE |
//!
//! A download goes on when its stream closes; asking again for the same model follows it from where it is.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream;
use kelpy_core::agent::Agent;
use kelpy_core::ollama::Download;
use serde::Deserialize;
use tokio::sync::watch;

use crate::Activity;
use crate::chat::error;

#[derive(Clone)]
pub struct OllamaState {
    pub agent: Arc<Agent>,
    pub activity: Arc<Activity>,
}

pub fn routes() -> Router<OllamaState> {
    Router::new()
        .route("/api/ollama", get(status))
        .route("/api/ollama/download", post(download))
}

#[derive(Deserialize)]
struct Address {
    base_url: Option<String>,
}

async fn status(State(state): State<OllamaState>, Query(address): Query<Address>) -> Response {
    Json(
        state
            .agent
            .ollama()
            .status(address.base_url.as_deref())
            .await,
    )
    .into_response()
}

#[derive(Deserialize)]
struct DownloadRequest {
    base_url: String,
    model: String,
}

async fn download(
    State(state): State<OllamaState>,
    Json(request): Json<DownloadRequest>,
) -> Response {
    if request.model.trim().is_empty() || request.base_url.trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid",
            "choose a model and an address".into(),
        );
    }
    let download = state.agent.ollama().download(
        request.base_url.trim().trim_end_matches('/'),
        &request.model,
        state.activity.busy(),
    );
    match download {
        Ok(progress) => stream_progress(progress),
        Err(stopping) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "stopping",
            stopping.to_string(),
        ),
    }
}

/// Sends the download's state at once, then its latest state at every change, and ends after the final one.
fn stream_progress(progress: watch::Receiver<Download>) -> Response {
    let events = stream::unfold(
        (progress, true, false),
        |(mut progress, first, ended)| async move {
            // A closed channel cannot come before the final state, but it ends the stream too.
            if ended || (!first && progress.changed().await.is_err()) {
                return None;
            }
            let current = progress.borrow_and_update().clone();
            let data = serde_json::to_string(&current).unwrap_or_default();
            let event = Ok::<_, Infallible>(Event::default().data(data));
            Some((event, (progress, false, current.done)))
        },
    );
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}
