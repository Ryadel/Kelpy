//! Restarting Kelpy as administrator from an interface: `GET /api/elevation` tells whether and how it can, and where
//! a restart asked for is; `POST /api/elevation` asks for it, `DELETE /api/elevation` cancels it while the system's
//! prompt is open.
//!
//! The gateway only records the request: the runtime restarts, through the system's own prompt, and the interfaces
//! follow it by `elevation_changed` events.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use kelpy_common::lock;
use kelpy_core::agent::Agent;
use serde::Serialize;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// How Kelpy restarts with administrator rights here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    /// The system asks on this computer's screen: User Account Control.
    Prompt,
    /// `sudo` asks for the password in the terminal Kelpy runs in.
    Terminal,
    /// The administrator restarts it by hand, with `sudo kelpy`: Kelpy has no terminal to ask in.
    Command,
}

/// Where a restart asked for is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    Idle,
    /// The system's prompt is open, or `sudo` asks for the password.
    Prompt,
    /// Handing over to the new Kelpy: the interfaces reconnect by themselves.
    Restarting,
    /// It did not restart, and why: refused, cancelled, or failed.
    Failed {
        reason: String,
    },
}

/// What the interfaces show.
#[derive(Debug, Clone, Serialize)]
pub struct ElevationView {
    /// Whether Kelpy runs with administrator rights already.
    pub elevated: bool,
    /// How it can restart with them; none when it has them or cannot.
    pub how: Option<How>,
    #[serde(flatten)]
    pub step: Step,
}

/// The restart as administrator, between the interfaces and the runtime.
pub struct Elevation {
    elevated: bool,
    how: Option<How>,
    agent: Arc<Agent>,
    state: Mutex<Inner>,
    notify: Notify,
}

struct Inner {
    step: Step,
    requested: bool,
    /// Cancels the restart under way.
    cancel: CancellationToken,
}

impl Elevation {
    /// `how` Kelpy restarts with administrator rights, unless `elevated` already; `agent` tells the interfaces.
    pub fn new(elevated: bool, how: Option<How>, agent: Arc<Agent>) -> Self {
        Self {
            elevated,
            how: if elevated { None } else { how },
            agent,
            state: Mutex::new(Inner {
                step: Step::Idle,
                requested: false,
                cancel: CancellationToken::new(),
            }),
            notify: Notify::new(),
        }
    }

    pub fn view(&self) -> ElevationView {
        ElevationView {
            elevated: self.elevated,
            how: self.how,
            step: lock(&self.state).step.clone(),
        }
    }

    /// Where the restart is now, told to the interfaces.
    pub fn set(&self, step: Step) {
        lock(&self.state).step = step;
        self.agent.elevation_changed();
    }

    /// Asks the runtime to restart, unless it cannot from an interface or a restart is under way.
    pub fn request(&self) -> Result<(), &'static str> {
        if !matches!(self.how, Some(How::Prompt | How::Terminal)) {
            return Err("Kelpy cannot restart itself as administrator here");
        }
        {
            let mut state = lock(&self.state);
            if !matches!(state.step, Step::Idle | Step::Failed { .. }) {
                return Err("a restart as administrator is under way");
            }
            state.step = Step::Prompt;
            state.requested = true;
            state.cancel = CancellationToken::new();
        }
        self.agent.elevation_changed();
        self.notify.notify_one();
        Ok(())
    }

    /// Completes when a restart is asked for, with what cancels it.
    pub async fn requested(&self) -> CancellationToken {
        loop {
            {
                let mut state = lock(&self.state);
                if std::mem::take(&mut state.requested) {
                    return state.cancel.clone();
                }
            }
            self.notify.notified().await;
        }
    }

    /// Cancels the restart under way, while it can still be: the old Kelpy goes on.
    pub(crate) fn cancel(&self) -> bool {
        let state = lock(&self.state);
        let cancellable = state.step == Step::Prompt;
        if cancellable {
            state.cancel.cancel();
        }
        cancellable
    }
}

pub fn routes() -> Router<Arc<Elevation>> {
    Router::new().route("/api/elevation", get(view).post(request).delete(cancel))
}

async fn view(State(elevation): State<Arc<Elevation>>) -> Json<ElevationView> {
    Json(elevation.view())
}

async fn request(State(elevation): State<Arc<Elevation>>) -> Response {
    match elevation.request() {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(problem) => crate::chat::error(StatusCode::CONFLICT, "elevation", problem.to_owned()),
    }
}

async fn cancel(State(elevation): State<Arc<Elevation>>) -> StatusCode {
    if elevation.cancel() {
        StatusCode::ACCEPTED
    } else {
        StatusCode::CONFLICT
    }
}
