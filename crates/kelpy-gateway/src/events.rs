//! `GET /api/events`: the runtime's events as Server-Sent Events, for every interface to stay in step (see
//! [`kelpy_core::events`]). The stream ends with `stopping` when Kelpy stops.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;
use kelpy_core::agent::Agent;

pub const PATH: &str = "/api/events";

pub fn routes() -> Router<Arc<Agent>> {
    Router::new().route(PATH, get(events))
}

/// Subscribed before the response starts: an interface that loads what it shows once the stream is open misses
/// nothing in between.
async fn events(State(agent): State<Arc<Agent>>) -> Response {
    let events = stream::unfold(agent.subscribe(), |mut subscription| async move {
        let event = subscription.next().await?;
        let data = serde_json::to_string(&event).unwrap_or_default();
        Some((
            Ok::<_, Infallible>(Event::default().event(event.name()).data(data)),
            subscription,
        ))
    });
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}
