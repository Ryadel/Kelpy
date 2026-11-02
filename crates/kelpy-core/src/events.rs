//! The runtime's events: every change the interfaces show, published once and received by every interface that
//! subscribes, so that each Web UI tab and, later, the terminal UI stay in step without polling.
//!
//! Events are small notices of changes. The text of an answer is not one of them: an interface showing the
//! conversation where an answer starts follows it with [`Agent::follow`](crate::agent::Agent::follow), whose memory
//! is bounded by the answer itself. Each subscriber has a bounded queue; one that falls behind receives
//! [`Event::Resync`] in place of the events it missed, and reads again what it shows.

use kelpy_storage::{Message, Session};
use serde::Serialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::sync::CancellationToken;

/// How many events a subscriber may fall behind before it misses some. Events come one or a few per user action.
const CAPACITY: usize = 256;

/// A change, as the interfaces receive it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionCreated {
        session: Session,
    },
    /// A conversation changed: its title, its explanations, or the time of its last message.
    SessionChanged {
        session: Session,
    },
    SessionDeleted {
        id: i64,
    },
    /// An answer started, after the user's message: both as stored, the answer still empty.
    AnswerStarted {
        session_id: i64,
        user: Message,
        answer: Message,
    },
    /// An answer ended, as stored, with the reason if it did not complete.
    AnswerEnded {
        session_id: i64,
        message: Message,
        error: Option<String>,
        /// The last message the conversation's summary covers, if its older part is summarised for the model.
        summarised_through: Option<i64>,
    },
    /// The provider, or what it needs to answer (a sign-in, a key), changed: the interfaces read it again.
    ProviderChanged,
    /// Some events were missed by a subscriber that fell behind: it reads again everything it shows.
    Resync,
    /// Kelpy is stopping: the last event.
    Stopping,
}

impl Event {
    /// The event's name on the wire, as in its `type` field.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "session_created",
            Self::SessionChanged { .. } => "session_changed",
            Self::SessionDeleted { .. } => "session_deleted",
            Self::AnswerStarted { .. } => "answer_started",
            Self::AnswerEnded { .. } => "answer_ended",
            Self::ProviderChanged => "provider_changed",
            Self::Resync => "resync",
            Self::Stopping => "stopping",
        }
    }
}

/// Where events are published. Clones publish to the same subscribers.
#[derive(Clone)]
pub struct Events {
    sender: broadcast::Sender<Event>,
    closed: CancellationToken,
}

impl Events {
    pub fn new() -> Self {
        Self {
            sender: broadcast::Sender::new(CAPACITY),
            closed: CancellationToken::new(),
        }
    }

    /// Sends `event` to every subscriber; with none, it is simply dropped.
    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    /// The events from now on, until Kelpy stops.
    pub fn subscribe(&self) -> Subscription {
        // Subscribed first, checked after: a subscriber either sees the closing or receives `Stopping`.
        let receiver = self.sender.subscribe();
        Subscription {
            receiver,
            stopped: self.closed.is_cancelled(),
            done: false,
        }
    }

    /// Kelpy is stopping: every subscription, present and future, ends with [`Event::Stopping`].
    pub fn close(&self) {
        self.closed.cancel();
        self.publish(Event::Stopping);
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

/// The events for one interface.
pub struct Subscription {
    receiver: broadcast::Receiver<Event>,
    /// Kelpy was already stopping when the subscription began.
    stopped: bool,
    done: bool,
}

impl Subscription {
    /// The next event, or `None` after [`Event::Stopping`].
    pub async fn next(&mut self) -> Option<Event> {
        if self.done {
            return None;
        }
        let event = if self.stopped {
            Event::Stopping
        } else {
            match self.receiver.recv().await {
                Ok(event) => event,
                Err(RecvError::Lagged(_)) => Event::Resync,
                // Every sender is gone: so is the runtime.
                Err(RecvError::Closed) => Event::Stopping,
            }
        };
        self.done = event == Event::Stopping;
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_subscriber_receives_the_events_from_its_start() {
        let events = Events::new();
        events.publish(Event::ProviderChanged);
        let (mut first, mut second) = (events.subscribe(), events.subscribe());
        events.publish(Event::SessionDeleted { id: 7 });
        assert_eq!(first.next().await, Some(Event::SessionDeleted { id: 7 }));
        assert_eq!(second.next().await, Some(Event::SessionDeleted { id: 7 }));
    }

    #[tokio::test]
    async fn a_subscriber_that_falls_behind_is_told_to_read_again() {
        let events = Events::new();
        let mut slow = events.subscribe();
        for id in 0..=CAPACITY as i64 {
            events.publish(Event::SessionDeleted { id });
        }
        assert_eq!(slow.next().await, Some(Event::Resync));
        // It goes on from the oldest event still queued.
        assert_eq!(slow.next().await, Some(Event::SessionDeleted { id: 1 }));
    }

    #[tokio::test]
    async fn closing_ends_every_subscription_present_and_future() {
        let events = Events::new();
        let mut before = events.subscribe();
        events.close();
        let mut after = events.subscribe();
        for subscription in [&mut before, &mut after] {
            assert_eq!(subscription.next().await, Some(Event::Stopping));
            assert_eq!(subscription.next().await, None);
        }
        assert_eq!(Event::Resync.name(), "resync");
        assert_eq!(
            serde_json::to_value(Event::SessionDeleted { id: 3 }).unwrap(),
            serde_json::json!({ "type": "session_deleted", "id": 3 })
        );
    }
}
