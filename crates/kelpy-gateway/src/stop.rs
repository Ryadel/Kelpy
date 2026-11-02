//! Stopping Kelpy from an interface: `POST /api/shutdown`, used by the Web UI and by `kelpy stop`.
//!
//! The request only starts the runtime's usual orderly shutdown; like every API route, it needs credentials.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Extension, Router};
use kelpy_common::lock;
use tokio::sync::Notify;

use crate::auth::Client;

/// Where a request to stop came from, waited on by the runtime.
#[derive(Default)]
pub struct StopRequest {
    notify: Notify,
    from: Mutex<Option<&'static str>>,
}

impl StopRequest {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the runtime to stop. The first request is the one reported.
    pub fn request(&self, from: &'static str) {
        lock(&self.from).get_or_insert(from);
        self.notify.notify_one();
    }

    /// Completes when a stop is requested, with where the request came from.
    pub async fn requested(&self) -> &'static str {
        self.notify.notified().await;
        lock(&self.from).unwrap_or("an interface")
    }
}

pub fn routes() -> Router<Arc<StopRequest>> {
    Router::new().route("/api/shutdown", post(shutdown))
}

async fn shutdown(
    State(stop): State<Arc<StopRequest>>,
    Extension(client): Extension<Client>,
) -> StatusCode {
    stop.request(match client {
        Client::Local => "`kelpy stop`",
        Client::Browser => "the Web UI",
    });
    StatusCode::ACCEPTED
}
