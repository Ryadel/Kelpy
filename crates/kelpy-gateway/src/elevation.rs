//! Restarting Kelpy from an interface: as administrator, or with the other memory of the server.
//!
//! `GET /api/elevation` tells whether and how Kelpy can restart as administrator, and where a restart asked for is;
//! `POST /api/elevation` asks for it, `DELETE /api/elevation` cancels it while the system's prompt is open.
//! `GET /api/memory` tells which memory Kelpy uses and which is chosen; `POST /api/memory` `{"use": "private" |
//! "shared"}` restarts Kelpy with it, as administrator for the shared one when Kelpy is not, or only records it when
//! Kelpy uses it already but another one is chosen, as the private memory while the shared one cannot be opened.
//!
//! The gateway only records the request: the runtime restarts, through the system's own prompt when it takes
//! administrator rights, and the interfaces follow it by `elevation_changed` events.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use kelpy_common::lock;
use kelpy_core::agent::Agent;
use kelpy_core::config::MemoryUse;
use serde::{Deserialize, Serialize};
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

/// The memory of the server, as the interfaces show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryView {
    /// The memory Kelpy uses now.
    #[serde(rename = "use")]
    pub using: MemoryUse,
    /// The memory chosen, as recorded in the configuration: the shared one, when Kelpy could not open it, differs
    /// from the one used.
    pub chosen: MemoryUse,
    /// Where the shared memory is.
    pub shared: Option<String>,
    /// Why the memory used is not the one asked for: the rights the shared one needs, or a choice that could not be
    /// recorded.
    pub problem: Option<String>,
}

impl Default for MemoryView {
    fn default() -> Self {
        Self {
            using: MemoryUse::Private,
            chosen: MemoryUse::Private,
            shared: None,
            problem: None,
        }
    }
}

/// What choosing a memory does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chosen {
    /// Kelpy uses it and it is the one chosen already.
    InUse,
    /// Kelpy uses it already: it is now recorded as the one chosen, without a restart.
    Recorded,
    /// Kelpy restarts with it.
    Restarts,
}

/// Why a memory could not be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Not now, or not from an interface: a restart under way, or rights Kelpy cannot ask for here.
    Conflict(String),
    /// The configuration could not record it.
    NotRecorded(String),
}

/// Records the memory chosen in the configuration.
pub type RecordMemory = Box<dyn Fn(MemoryUse) -> Result<(), String> + Send + Sync>;

/// A restart asked for.
#[derive(Debug, Clone)]
pub struct Restart {
    /// With administrator rights, through the system's prompt.
    pub elevate: bool,
    /// With this memory, chosen from now on.
    pub memory: Option<MemoryUse>,
    /// Cancels it while the system's prompt is open.
    pub cancel: CancellationToken,
}

/// Restarting Kelpy, between the interfaces and the runtime.
pub struct Elevation {
    elevated: bool,
    how: Option<How>,
    /// Locked after `state` when both are: a choice is recorded, or a restart admitted, under the restart's lock.
    memory: Mutex<MemoryView>,
    record: RecordMemory,
    agent: Arc<Agent>,
    state: Mutex<Inner>,
    notify: Notify,
}

struct Inner {
    step: Step,
    requested: Option<Restart>,
    /// Cancels the restart under way.
    cancel: CancellationToken,
}

impl Elevation {
    /// `how` Kelpy restarts with administrator rights, unless `elevated` already; `agent` tells the interfaces.
    pub fn new(elevated: bool, how: Option<How>, agent: Arc<Agent>) -> Self {
        Self {
            elevated,
            how: if elevated { None } else { how },
            memory: Mutex::new(MemoryView::default()),
            record: Box::new(|_| Err("Kelpy cannot record the memory chosen here".to_owned())),
            agent,
            state: Mutex::new(Inner {
                step: Step::Idle,
                requested: None,
                cancel: CancellationToken::new(),
            }),
            notify: Notify::new(),
        }
    }

    /// The memory of the server Kelpy runs with, and how a choice is recorded in the configuration.
    pub fn with_memory(
        mut self,
        memory: MemoryView,
        record: impl Fn(MemoryUse) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.memory = Mutex::new(memory);
        self.record = Box::new(record);
        self
    }

    pub fn view(&self) -> ElevationView {
        ElevationView {
            elevated: self.elevated,
            how: self.how,
            step: lock(&self.state).step.clone(),
        }
    }

    pub fn memory(&self) -> MemoryView {
        lock(&self.memory).clone()
    }

    /// Where the restart is now, told to the interfaces.
    pub fn set(&self, step: Step) {
        lock(&self.state).step = step;
        self.agent.elevation_changed();
    }

    /// Asks the runtime to restart as administrator, unless it cannot from an interface or a restart is under way.
    pub fn request(&self) -> Result<(), String> {
        if !self.can_elevate() {
            return Err("Kelpy cannot restart itself as administrator here".to_owned());
        }
        self.restart(true, None)
    }

    /// Asks the runtime to restart with the memory `using`, as administrator for the shared one when it is not.
    /// The memory in use already is only recorded as the one chosen, if it is not: persisted first, then shown.
    /// Nothing is chosen while a restart is under way, which carries a memory of its own.
    pub fn request_memory(&self, using: MemoryUse) -> Result<Chosen, Refused> {
        let state = lock(&self.state);
        Self::idle(&state).map_err(Refused::Conflict)?;
        {
            let mut memory = lock(&self.memory);
            if using == memory.using {
                if using == memory.chosen {
                    return Ok(Chosen::InUse);
                }
                (self.record)(using).map_err(Refused::NotRecorded)?;
                memory.chosen = using;
                memory.problem = None;
                drop(memory);
                drop(state);
                self.agent.elevation_changed();
                return Ok(Chosen::Recorded);
            }
        }
        let elevate = using == MemoryUse::Shared && !self.elevated;
        if elevate && !self.can_elevate() {
            return Err(Refused::Conflict(if cfg!(windows) {
                "only administrators can open the shared memory: start Kelpy as administrator on this computer"
                    .to_owned()
            } else {
                "only administrators can open the shared memory: start Kelpy with `sudo kelpy`"
                    .to_owned()
            }));
        }
        self.admit(state, elevate, Some(using));
        Ok(Chosen::Restarts)
    }

    fn can_elevate(&self) -> bool {
        matches!(self.how, Some(How::Prompt | How::Terminal))
    }

    /// Refuses while a restart is under way.
    fn idle(state: &Inner) -> Result<(), String> {
        if matches!(state.step, Step::Idle | Step::Failed { .. }) {
            Ok(())
        } else {
            Err("a restart of Kelpy is under way".to_owned())
        }
    }

    fn restart(&self, elevate: bool, memory: Option<MemoryUse>) -> Result<(), String> {
        let state = lock(&self.state);
        Self::idle(&state)?;
        self.admit(state, elevate, memory);
        Ok(())
    }

    /// Records the restart asked for, under the restart's lock, `state`, taken while it was idle; then tells.
    fn admit(
        &self,
        mut state: std::sync::MutexGuard<'_, Inner>,
        elevate: bool,
        memory: Option<MemoryUse>,
    ) {
        // Only the system's prompt can be cancelled: without it, the restart begins at once.
        state.step = if elevate {
            Step::Prompt
        } else {
            Step::Restarting
        };
        state.cancel = CancellationToken::new();
        state.requested = Some(Restart {
            elevate,
            memory,
            cancel: state.cancel.clone(),
        });
        drop(state);
        self.agent.elevation_changed();
        self.notify.notify_one();
    }

    /// Completes when a restart is asked for.
    pub async fn requested(&self) -> Restart {
        loop {
            if let Some(restart) = lock(&self.state).requested.take() {
                return restart;
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
    Router::new()
        .route("/api/elevation", get(view).post(request).delete(cancel))
        .route("/api/memory", get(memory).post(choose_memory))
}

async fn view(State(elevation): State<Arc<Elevation>>) -> Json<ElevationView> {
    Json(elevation.view())
}

async fn request(State(elevation): State<Arc<Elevation>>) -> Response {
    match elevation.request() {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(problem) => crate::chat::error(StatusCode::CONFLICT, "elevation", problem),
    }
}

async fn cancel(State(elevation): State<Arc<Elevation>>) -> StatusCode {
    if elevation.cancel() {
        StatusCode::ACCEPTED
    } else {
        StatusCode::CONFLICT
    }
}

async fn memory(State(elevation): State<Arc<Elevation>>) -> Json<MemoryView> {
    Json(elevation.memory())
}

#[derive(Deserialize)]
struct Choice {
    #[serde(rename = "use")]
    using: MemoryUse,
}

/// 202 when Kelpy restarts with the memory chosen; 200 with the memory as it is now when Kelpy uses it already and
/// it is recorded as the one chosen; 204 when it is both already.
async fn choose_memory(
    State(elevation): State<Arc<Elevation>>,
    Json(choice): Json<Choice>,
) -> Response {
    match elevation.request_memory(choice.using) {
        Ok(Chosen::Restarts) => StatusCode::ACCEPTED.into_response(),
        Ok(Chosen::Recorded) => Json(elevation.memory()).into_response(),
        Ok(Chosen::InUse) => StatusCode::NO_CONTENT.into_response(),
        Err(Refused::NotRecorded(problem)) => {
            crate::chat::error(StatusCode::INTERNAL_SERVER_ERROR, "memory", problem)
        }
        Err(Refused::Conflict(problem)) => {
            crate::chat::error(StatusCode::CONFLICT, "memory", problem)
        }
    }
}
