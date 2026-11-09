//! The runtime's local API, as the terminal UI uses it, through the client the terminal commands use too.

use futures_util::Stream;
use kelpy_client::{Client, ClientError, Method};
use serde_json::json;

use crate::model::{Connections, Conversation, ProviderInfo, Session};

/// A client of one running Kelpy. Clones share its connections.
#[derive(Clone)]
pub struct Api(Client);

impl Api {
    pub fn new(url: String, token: String) -> Result<Self, ClientError> {
        Client::new(url, token).map(Self)
    }

    pub(crate) async fn provider(&self) -> Result<ProviderInfo, ClientError> {
        self.0.get("/api/provider").await
    }

    pub(crate) async fn sessions(&self) -> Result<Vec<Session>, ClientError> {
        self.0.get("/api/sessions").await
    }

    pub(crate) async fn conversation(&self, id: i64) -> Result<Conversation, ClientError> {
        self.0.get(&format!("/api/sessions/{id}/messages")).await
    }

    /// Creates a conversation with these modes, for its first message.
    pub(crate) async fn create_session(
        &self,
        modes: &serde_json::Value,
    ) -> Result<Session, ClientError> {
        self.0.send(Method::POST, "/api/sessions", modes).await
    }

    /// Changes a conversation's title or modes: `{"access": "full"}`.
    pub(crate) async fn update_session(
        &self,
        id: i64,
        change: &serde_json::Value,
    ) -> Result<Session, ClientError> {
        self.0
            .send(Method::PATCH, &format!("/api/sessions/{id}"), change)
            .await
    }

    pub(crate) async fn decide(
        &self,
        session: i64,
        action: i64,
        approve: bool,
    ) -> Result<(), ClientError> {
        let decision = json!({ "decision": if approve { "approve" } else { "deny" } });
        let path = format!("/api/sessions/{session}/actions/{action}");
        self.0.act(Method::POST, &path, Some(&decision)).await
    }

    pub(crate) async fn connections(&self) -> Result<Connections, ClientError> {
        self.0.get("/api/connections").await
    }

    /// Allows or refuses a remote connection, from the terminal UI.
    pub(crate) async fn decide_connection(&self, id: u64, allow: bool) -> Result<(), ClientError> {
        let path = format!(
            "/api/connections/{id}/{}",
            if allow { "allow" } else { "deny" }
        );
        let body = json!({ "interface": "terminal" });
        self.0.act(Method::POST, &path, Some(&body)).await
    }

    pub(crate) async fn cancel(&self, session: i64) -> Result<(), ClientError> {
        let path = format!("/api/sessions/{session}/cancel");
        self.0.act(Method::POST, &path, None::<&()>).await
    }

    /// Sends a message: the answer follows as a stream of events.
    pub(crate) async fn send(
        &self,
        session: i64,
        content: &str,
    ) -> Result<impl Stream<Item = Result<String, ClientError>> + use<>, ClientError> {
        let path = format!("/api/sessions/{session}/messages");
        let body = json!({ "content": content });
        self.0.events(Method::POST, &path, Some(&body)).await
    }

    /// Follows the answer being written in `session`, from where it is.
    pub(crate) async fn follow(
        &self,
        session: i64,
    ) -> Result<impl Stream<Item = Result<String, ClientError>> + use<>, ClientError> {
        let path = format!("/api/sessions/{session}/answer");
        self.0.events(Method::GET, &path, None::<&()>).await
    }

    /// The runtime's event channel.
    pub(crate) async fn subscribe(
        &self,
    ) -> Result<impl Stream<Item = Result<String, ClientError>> + use<>, ClientError> {
        self.0.events(Method::GET, "/api/events", None::<&()>).await
    }
}
