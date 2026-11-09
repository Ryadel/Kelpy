//! The remote connections API: what asks to connect, what is connected, and the administrator's decision.
//!
//! | Route | |
//! |---|---|
//! | `GET /api/connections` | the connections waiting for a decision and those open |
//! | `POST /api/connections/{id}/allow`, `POST /api/connections/{id}/deny` | decide; `interface` says from where |

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use kelpy_core::agent::Agent;
use serde::Deserialize;

use crate::chat::error;

pub fn routes() -> Router<Arc<Agent>> {
    Router::new()
        .route("/api/connections", get(list))
        .route("/api/connections/{id}/allow", post(allow))
        .route("/api/connections/{id}/deny", post(deny))
}

/// Where the decision is made, for the log.
#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Interface {
    #[default]
    Web,
    Terminal,
}

#[derive(Deserialize, Default)]
struct Decision {
    #[serde(default)]
    interface: Interface,
}

async fn list(State(agent): State<Arc<Agent>>) -> Response {
    Json(agent.connections().view()).into_response()
}

async fn allow(
    state: State<Arc<Agent>>,
    id: Path<u64>,
    decision: Option<Json<Decision>>,
) -> Response {
    decide(state, id, decision, true)
}

async fn deny(
    state: State<Arc<Agent>>,
    id: Path<u64>,
    decision: Option<Json<Decision>>,
) -> Response {
    decide(state, id, decision, false)
}

fn decide(
    State(agent): State<Arc<Agent>>,
    Path(id): Path<u64>,
    decision: Option<Json<Decision>>,
    allow: bool,
) -> Response {
    let by = match decision
        .map(|Json(decision)| decision.interface)
        .unwrap_or_default()
    {
        Interface::Web => "in the Web UI",
        Interface::Terminal => "in the terminal UI",
    };
    match agent.connections().decide(id, allow, by) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => error(StatusCode::CONFLICT, "not_waiting", failure.to_string()),
    }
}
