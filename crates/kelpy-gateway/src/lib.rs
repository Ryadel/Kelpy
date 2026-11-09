//! Kelpy local gateway: the HTTP server shared by the Web UI and, later, the terminal UI and the CLI.
//!
//! It listens on loopback only, rejects requests whose `Host` or `Origin` does not belong to this server, and
//! answers API requests only with credentials (see [`Auth`]).

mod activity;
mod assets;
mod auth;
mod chat;
mod chatgpt;
mod connections;
mod events;
mod files;
mod ollama;
mod secrets;
mod security;
mod stop;

pub use activity::{Activity, Busy};
pub use auth::Auth;
pub use kelpy_core::agent::Agent;
pub use stop::StopRequest;

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router, middleware};
use serde::Serialize;
use tokio::net::TcpListener;

/// What the runtime reports about itself through `/api/status`.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub version: String,
    pub pid: u32,
    pub home: PathBuf,
    /// Unix time, in seconds.
    pub started_at: i64,
    /// The account Kelpy and its actions run as, such as `jsmith (not elevated)`.
    pub account: String,
    /// Without administrator rights, what fails and how to have them.
    pub account_limits: Option<String>,
}

#[derive(Debug, Serialize)]
struct Health {
    status: &'static str,
}

/// Ports that browsers refuse to open, so that a Web UI there could not be reached: those of Chromium's and Firefox's
/// lists that an operating system may hand out, from 1024 up.
const BROWSER_BLOCKED_PORTS: [u16; 19] = [
    1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668, 6669,
    6679, 6697, 10080,
];
/// How many ports the operating system is asked for, when the ones it picks are refused by browsers.
const PORT_ATTEMPTS: usize = 8;

/// Binds the server socket. `host` must be a loopback address; port 0 lets the operating system choose, never a port
/// that browsers refuse. With port 0, `preferred` is tried first (the port of the last start, so that open pages and
/// SSH tunnels to it keep working) unless it is taken or refused by browsers.
pub async fn bind(host: IpAddr, port: u16, preferred: Option<u16>) -> io::Result<TcpListener> {
    if !host.is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to listen on {host}: only loopback addresses are allowed"),
        ));
    }
    let blocked = |port| BROWSER_BLOCKED_PORTS.contains(&port);
    if let Some(preferred) = preferred.filter(|&preferred| port == 0 && !blocked(preferred))
        && let Ok(listener) = TcpListener::bind(SocketAddr::new(host, preferred)).await
    {
        return Ok(listener);
    }
    bind_avoiding(SocketAddr::new(host, port), blocked).await
}

/// Binds `address`, refusing the ports for which `blocked` is true: with port 0, the operating system is asked again.
async fn bind_avoiding(
    address: SocketAddr,
    mut blocked: impl FnMut(u16) -> bool,
) -> io::Result<TcpListener> {
    if address.port() != 0 && blocked(address.port()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "browsers refuse to open port {}: choose another port in config.toml",
                address.port()
            ),
        ));
    }
    for _ in 0..PORT_ATTEMPTS {
        let listener = TcpListener::bind(address).await?;
        if !blocked(listener.local_addr()?.port()) {
            return Ok(listener);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "the operating system offered only ports that browsers refuse to open",
    ))
}

/// Builds the routes for a server listening on `port`, protected by `auth`, with conversations served by `agent`.
/// Accepted requests and answers being written are recorded in `activity`; requests to stop Kelpy go to `stop`.
pub fn router(
    status: RuntimeStatus,
    port: u16,
    activity: Arc<Activity>,
    auth: Arc<Auth>,
    agent: Arc<Agent>,
    stop: Arc<StopRequest>,
) -> Router {
    let allowed = Arc::new(security::AllowedHosts::for_port(port));
    let chatgpt = chatgpt::ChatGptState {
        agent: Arc::clone(&agent),
        activity: Arc::clone(&activity),
        port,
    };
    let ollama = ollama::OllamaState {
        agent: Arc::clone(&agent),
        activity: Arc::clone(&activity),
    };
    let chat = chat::ChatState {
        agent: Arc::clone(&agent),
        activity: Arc::clone(&activity),
    };
    Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(runtime_status))
        .fallback(assets::serve)
        .with_state(Arc::new(status))
        .merge(auth::routes().with_state(Arc::clone(&auth)))
        .merge(chat::routes().with_state(chat))
        .merge(secrets::routes().with_state(Arc::clone(&agent)))
        .merge(files::routes().with_state(Arc::clone(&agent)))
        .merge(connections::routes().with_state(Arc::clone(&agent)))
        .merge(events::routes().with_state(agent))
        .merge(chatgpt::routes().with_state(chatgpt))
        .merge(ollama::routes().with_state(ollama))
        .merge(stop::routes().with_state(stop))
        // Layers run outermost-last: foreign and unauthenticated requests are rejected before they count as
        // activity.
        .layer(middleware::from_fn_with_state(activity, activity::track))
        .layer(middleware::from_fn_with_state(auth, auth::require))
        .layer(middleware::from_fn_with_state(allowed, security::guard))
}

/// Serves until `shutdown` completes, then lets in-flight requests finish.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn runtime_status(State(status): State<Arc<RuntimeStatus>>) -> Json<RuntimeStatus> {
    Json(status.as_ref().clone())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[tokio::test]
    async fn a_port_browsers_refuse_is_never_used() {
        let localhost = IpAddr::from(Ipv4Addr::LOCALHOST);
        let refused = bind(localhost, 6666, None).await.unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);

        // The first port the operating system offers is treated as refused: another one is asked for.
        let mut offered = Vec::new();
        let listener = bind_avoiding(SocketAddr::new(localhost, 0), |port| {
            offered.push(port);
            offered.len() == 1
        })
        .await
        .unwrap();
        assert_eq!(offered.len(), 2);
        assert_eq!(listener.local_addr().unwrap().port(), offered[1]);

        let other = bind(localhost, 0, Some(6666)).await.unwrap();
        assert_ne!(other.local_addr().unwrap().port(), 6666);
    }

    #[tokio::test]
    async fn the_last_port_is_used_again_when_it_is_free() {
        let localhost = IpAddr::from(Ipv4Addr::LOCALHOST);
        let first = bind(localhost, 0, None).await.unwrap();
        let port = first.local_addr().unwrap().port();

        let taken = bind(localhost, 0, Some(port)).await.unwrap();
        assert_ne!(taken.local_addr().unwrap().port(), port);

        drop(first);
        let again = bind(localhost, 0, Some(port)).await.unwrap();
        assert_eq!(again.local_addr().unwrap().port(), port);
    }
}
