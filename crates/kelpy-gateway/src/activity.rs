//! Tracks whether anyone is using the runtime, for the idle shutdown.
//!
//! Every request counts as activity, except `/api/health`, which monitoring probes may call forever, the identity
//! proof, which the Web UI calls while it checks the connection, and the events, to which a page left open
//! subscribes again by itself. Work of
//! unknown length (a request being served, later an agent task) holds a [`Busy`] guard: while any guard is alive
//! the runtime is never idle, and the idle time restarts when the last one is dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

#[derive(Debug)]
pub struct Activity {
    started: Instant,
    /// Milliseconds since `started` at the last activity.
    last: AtomicU64,
    busy: AtomicUsize,
}

impl Activity {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            last: AtomicU64::new(0),
            busy: AtomicUsize::new(0),
        }
    }

    /// Records activity now.
    pub fn touch(&self) {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last.fetch_max(now, Ordering::Relaxed);
    }

    /// Marks the runtime busy until the returned guard is dropped.
    pub fn busy(self: &Arc<Self>) -> Busy {
        self.busy.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Busy {
            activity: Arc::clone(self),
        }
    }

    /// How long the runtime has been idle; zero while anything is busy.
    pub fn idle_for(&self) -> Duration {
        if self.busy.load(Ordering::SeqCst) > 0 {
            return Duration::ZERO;
        }
        let last = Duration::from_millis(self.last.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(last)
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

/// Keeps the runtime busy while alive.
#[derive(Debug)]
pub struct Busy {
    activity: Arc<Activity>,
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.activity.touch();
        self.activity.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

pub async fn track(
    State(activity): State<Arc<Activity>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if path == "/api/health" || path == crate::auth::PROOF_PATH || path == crate::events::PATH {
        return next.run(request).await;
    }
    let _busy = activity.busy();
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_is_never_idle_and_restarts_the_clock() {
        let activity = Arc::new(Activity::new());
        std::thread::sleep(Duration::from_millis(60));
        assert!(activity.idle_for() >= Duration::from_millis(50));

        let guard = activity.busy();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(activity.idle_for(), Duration::ZERO);

        drop(guard);
        assert!(activity.idle_for() < Duration::from_millis(50));
    }
}
