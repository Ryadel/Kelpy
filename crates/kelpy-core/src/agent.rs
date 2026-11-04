//! The agent: conversations with the configured model, shared by every interface.
//!
//! Sending a message stores it, starts the answer in the background and returns its [`AnswerEvents`]. The answer
//! keeps going if the interface disconnects: it is saved as it arrives, can be followed again from any interface
//! with [`Agent::follow`], and can be stopped with [`Agent::cancel`]. While answering, the model may look at the
//! system, as each conversation's [`Access`] allows; its actions wait for the administrator's decision
//! ([`Agent::decide`]) when its [`Approval`] mode asks for one.
//!
//! The agent puts together four parts with one job each: [`providers`] chooses the provider, [`turns`] runs the
//! turns of the conversations, fitting each to the model's window with [`context`], [`server`] keeps what Kelpy left
//! on the server and runs the administrator's own operations, and [`Storage`] serves the database. The agent itself
//! only passes requests on, and publishes what changed to the interfaces' [`Events`].

mod context;
mod providers;
mod server;
mod turns;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use kelpy_providers::chatgpt::Endpoints;
use kelpy_providers::{Provider, ProviderError};
pub use kelpy_storage::{Access, Action, ActionState, Approval, Modes, UNTITLED};
use kelpy_storage::{Message, Session, Storage, Store, StoreError};
use kelpy_tools::Executor;
use tokio_util::task::TaskTracker;

use crate::chatgpt::{ChatGpt, ChatGptError};
use crate::config::{ConfigError, ProviderConfig};
use crate::events::{Event, Events, Subscription};
use crate::home::Home;
use crate::ollama::Ollama;
use providers::Providers;
pub use providers::{
    KeySource, KindInfo, ModelChoice, ModelList, ModelQuery, Needed, ProviderInfo, ProviderUpdate,
};
use server::Server;
pub use server::{DifferenceView, Operated};
use turns::Turns;
pub use turns::{AnswerEvents, ChatEvent};

/// How long deleting a conversation waits for its answer to stop. Stopping takes one database write.
const STOP_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("no AI provider is configured yet")]
    NotConfigured,
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("an answer is already being written in this conversation")]
    Busy,
    #[error("Kelpy is stopping: no new answers can start")]
    Stopping,
    #[error("this conversation is being deleted")]
    Deleting,
    #[error("the message is empty")]
    EmptyMessage,
    #[error("this action is not waiting for a decision")]
    NotWaiting,
    #[error("this action is not running")]
    NotRunning,
    /// The task that owns the answer ended before reporting its start: only a panic can do that.
    #[error("the answer could not start")]
    NotStarted,
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    ChatGpt(#[from] ChatGptError),
}

pub struct Agent {
    store: Storage,
    providers: Providers,
    turns: Arc<Turns>,
    server: Arc<Server>,
    /// Where the work in the background runs, answers and downloads: [`Agent::close`] waits for it.
    tasks: TaskTracker,
    events: Events,
}

impl Agent {
    /// The agent of `home`, with conversations in `store`. `chatgpt` is where signing in with ChatGPT goes:
    /// OpenAI, or fakes in tests.
    pub fn new(
        home: &Home,
        store: Store,
        provider: Option<ProviderConfig>,
        chatgpt: Endpoints,
    ) -> Result<Self, AgentError> {
        let client = Provider::new()?;
        let tasks = TaskTracker::new();
        let events = Events::new();
        let store = Storage::start(store)?;
        let backups = home.data_dir().join("backups");
        // Kelpy's own data holds credentials and every conversation: never read or changed for the model.
        let executor =
            Executor::local(vec![home.data_dir()], backups.clone()).with_memory(home.memory_dir());
        let providers = Providers::new(
            client.clone(),
            ChatGpt::new(home, client.clone(), chatgpt, events.clone()),
            Arc::new(Ollama::new(client.clone(), tasks.clone(), executor.clone())),
            home.config_path(),
            provider,
        );
        let server = Arc::new(Server::new(
            store.clone(),
            executor.clone(),
            events.clone(),
            backups,
            home.memory_dir(),
        ));
        let turns = Turns::new(
            store.clone(),
            client,
            tasks.clone(),
            events.clone(),
            home.memory_dir().join("MEMORY.md"),
            executor,
            Arc::clone(&server),
        );
        Ok(Self {
            store,
            providers,
            turns: Arc::new(turns),
            server,
            tasks,
            events,
        })
    }

    /// Checks what Kelpy left on the server against what is there now, as when Kelpy starts: the differences found
    /// are shown and told to the model, never reverted.
    pub async fn check_server(&self) -> Result<(), AgentError> {
        self.server.check(true).await
    }

    /// The differences found between what Kelpy left on the server and what is there, not settled yet.
    pub async fn differences(&self) -> Result<Vec<DifferenceView>, AgentError> {
        self.server.differences().await
    }

    /// Accepts a difference: what was found is expected from now on.
    pub async fn accept_difference(&self, id: i64) -> Result<(), AgentError> {
        self.server.accept(id).await
    }

    /// Puts back a file as Kelpy last wrote it, in place of the difference `id`, as an operation of the
    /// administrator in the conversation `session_id`; only shown if `preview`.
    pub async fn put_back(
        &self,
        session_id: i64,
        id: i64,
        preview: bool,
    ) -> Result<Operated, AgentError> {
        self.server.put_back(session_id, id, preview).await
    }

    /// Puts a file back as it was before the change of an action, from its backup, as an operation of the
    /// administrator; only shown if `preview`. The administrator's own request: it does not depend on the
    /// conversation's System access or Approval mode, which govern what the model does.
    pub async fn restore(
        &self,
        session_id: i64,
        action_id: i64,
        preview: bool,
    ) -> Result<Operated, AgentError> {
        self.server.restore(session_id, action_id, preview).await
    }

    /// The operations the administrator asked for in a conversation, oldest first.
    pub async fn administrator_actions(&self, session_id: i64) -> Result<Vec<Action>, AgentError> {
        Ok(self
            .store
            .call(move |store| store.administrator_actions(session_id))
            .await?)
    }

    /// The handover report of a conversation, in Markdown.
    pub async fn report(&self, session_id: i64) -> Result<String, AgentError> {
        self.server.report(session_id).await
    }

    /// The operations of a conversation that succeeded, with their arguments and checks, as JSON.
    pub async fn export(&self, session_id: i64) -> Result<serde_json::Value, AgentError> {
        self.server.export(session_id).await
    }

    /// The changes from now on, until Kelpy stops: conversations, answers, the provider.
    pub fn subscribe(&self) -> Subscription {
        self.events.subscribe()
    }

    pub fn chatgpt(&self) -> &ChatGpt {
        self.providers.chatgpt()
    }

    pub fn ollama(&self) -> &Arc<Ollama> {
        self.providers.ollama()
    }

    pub fn provider_info(&self) -> ProviderInfo {
        self.providers.info()
    }

    /// Saves the provider to `config.toml`; the API key, if given, stays in memory until Kelpy stops. The file
    /// and the state in memory change together, one update at a time.
    pub fn configure_provider(&self, update: ProviderUpdate) -> Result<ProviderInfo, AgentError> {
        let info = self.providers.configure(update)?;
        self.events.publish(Event::ProviderChanged);
        Ok(info)
    }

    /// The models the service of a setup offers, its recommended ones first. A list the service refuses is not an
    /// error: the recommended models are offered with the reason, and any name can still be typed.
    pub async fn models(&self, query: ModelQuery) -> Result<ModelList, AgentError> {
        self.providers.models(query).await
    }

    pub async fn sessions(&self) -> Result<Vec<Session>, AgentError> {
        Ok(self.store.call(|store| store.sessions()).await?)
    }

    pub async fn create_session(
        &self,
        title: Option<&str>,
        modes: Modes,
    ) -> Result<Session, AgentError> {
        let title = title
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .unwrap_or(UNTITLED)
            .to_owned();
        let session = self
            .store
            .call(move |store| store.create_session(&title, modes))
            .await?;
        self.events.publish(Event::SessionCreated {
            session: session.clone(),
        });
        Ok(session)
    }

    pub async fn rename_session(&self, id: i64, title: &str) -> Result<Session, AgentError> {
        let title = title.trim().to_owned();
        if title.is_empty() {
            return Err(AgentError::Invalid("the title is empty".into()));
        }
        let session = self
            .store
            .call(move |store| {
                store.rename_session(id, &title)?;
                store.session(id)
            })
            .await?;
        Ok(self.changed(session))
    }

    /// Turns plain-word explanations on or off for the next answers of a conversation.
    pub async fn set_explanations(
        &self,
        id: i64,
        explanations: bool,
    ) -> Result<Session, AgentError> {
        let session = self
            .store
            .call(move |store| {
                store.set_explanations(id, explanations)?;
                store.session(id)
            })
            .await?;
        Ok(self.changed(session))
    }

    /// What Kelpy may do on the system in a conversation, from its next step on. None also denies the actions
    /// waiting for a decision; an action already running ends as it would.
    pub async fn set_access(&self, id: i64, access: Access) -> Result<Session, AgentError> {
        let session = self
            .store
            .call(move |store| {
                store.set_access(id, access)?;
                store.session(id)
            })
            .await?;
        if access == Access::None {
            self.turns.revoke(id);
        }
        Ok(self.changed(session))
    }

    /// When Kelpy asks before acting in a conversation, from its next action on.
    pub async fn set_approval(&self, id: i64, approval: Approval) -> Result<Session, AgentError> {
        let session = self
            .store
            .call(move |store| {
                store.set_approval(id, approval)?;
                store.session(id)
            })
            .await?;
        Ok(self.changed(session))
    }

    /// Approves or denies an action waiting for a decision in the answer being written in a conversation.
    /// Approving can also switch the conversation to another Approval mode, in which the action would not have
    /// asked: the following ones then follow it. A decision refused changes no mode.
    pub async fn decide(
        &self,
        session_id: i64,
        action_id: i64,
        approve: bool,
        switch_to: Option<Approval>,
    ) -> Result<(), AgentError> {
        let switched = self
            .turns
            .decide(session_id, action_id, approve, switch_to)
            .await?;
        if let Some(session) = switched {
            self.changed(session);
        }
        Ok(())
    }

    /// Stops an action running in the answer being written in a conversation. The answer goes on, and the model is
    /// told that the administrator stopped it.
    pub fn stop_action(&self, session_id: i64, action_id: i64) -> Result<(), AgentError> {
        self.turns.halt(session_id, action_id)
    }

    /// The account Kelpy's actions run as.
    pub fn account(&self) -> &kelpy_tools::Account {
        self.turns.account()
    }

    /// Publishes that `session` changed, and returns it.
    fn changed(&self, session: Session) -> Session {
        self.events.publish(Event::SessionChanged {
            session: session.clone(),
        });
        session
    }

    /// Deletes a conversation. From the start no new answer can begin in it; an answer being written is stopped
    /// and the deletion waits until it has ended, so that nothing is written for the conversation afterwards.
    pub async fn delete_session(&self, id: i64) -> Result<(), AgentError> {
        let (_deletion, answer_end) = self.turns.begin_deletion(id)?;
        if let Some(end) = answer_end
            && tokio::time::timeout(STOP_WAIT, end.cancelled())
                .await
                .is_err()
        {
            return Err(AgentError::Busy);
        }
        self.store
            .call(move |store| store.delete_session(id))
            .await?;
        self.server.forget(id);
        self.events.publish(Event::SessionDeleted { id });
        Ok(())
    }

    pub async fn messages(&self, session_id: i64) -> Result<Vec<Message>, AgentError> {
        Ok(self
            .store
            .call(move |store| store.messages(session_id))
            .await?)
    }

    /// The last message of a conversation that its summary covers, if its older part is summarised for the model.
    /// The messages after it are given to the model as they are.
    pub async fn summarised_through(&self, session_id: i64) -> Result<Option<i64>, AgentError> {
        let summary = self
            .store
            .call(move |store| store.summary(session_id))
            .await?;
        Ok(summary.map(|summary| summary.through))
    }

    /// Whether an answer is being written in this conversation.
    pub fn is_running(&self, session_id: i64) -> bool {
        self.turns.is_running(session_id)
    }

    /// Stops the answer being written in a conversation. Returns whether there was one.
    pub fn cancel(&self, session_id: i64) -> bool {
        self.turns.cancel(session_id)
    }

    /// Refuses new answers and downloads from now on, stops every answer being written and tells the interfaces that
    /// Kelpy is stopping. This happens at once, when called; the returned future then waits, up to `limit`, until the
    /// answers are saved, and tells whether all were.
    pub fn stop_all(&self, limit: Duration) -> impl Future<Output = bool> + use<> {
        self.providers.ollama().close();
        self.events.close();
        self.turns.stop_all(limit)
    }

    /// Kelpy is stopping, after [`Agent::stop_all`]: waits for the work in the background to end, then closes the
    /// database once the writes already sent are done, all until `deadline`. Returns whether everything ended in time;
    /// if not, the database is left to the process's exit, which its journal survives.
    pub async fn close(&self, deadline: tokio::time::Instant) -> bool {
        self.providers.ollama().close();
        self.tasks.close();
        let ended = async {
            self.tasks.wait().await;
            self.store.stop().await;
        };
        tokio::time::timeout_at(deadline, ended).await.is_ok()
    }

    /// Follows the answer being written in a conversation, from where it is now. `None` when there is none.
    pub fn follow(&self, session_id: i64) -> Option<AnswerEvents> {
        self.turns.follow(session_id)
    }

    /// Stores `text` as the user's message and starts the answer. `guard` is kept alive until the answer ends,
    /// for example to keep the runtime from stopping for inactivity. A ChatGPT session is renewed first if needed,
    /// so that an ended one is reported before anything is stored.
    pub async fn send(
        &self,
        session_id: i64,
        text: &str,
        guard: impl Send + 'static,
    ) -> Result<AnswerEvents, AgentError> {
        let text = turns::message(text)?;
        let settings = self.providers.ready().await?;
        self.turns.start(session_id, text, settings, guard).await
    }
}

#[cfg(test)]
mod tests {
    use super::turns::{
        CHANGING, CONCISE, EXPLAINED, LOOKING, Looking, MAX_ANSWER, MAX_MEMORY_INDEX, MAX_MESSAGE,
        TITLE_LENGTH, instructions, title_from,
    };
    use super::*;
    use crate::config::Config;
    use crate::events::Event;
    use crate::services;
    use kelpy_common::lock;
    use kelpy_providers::Kind;
    use kelpy_storage::Status;
    use kelpy_test_support::{FakeServer, chat_completions};
    use serde_json::json;
    use std::fs;

    fn agent(dir: &tempfile::TempDir, base_url: String) -> Arc<Agent> {
        let home = Home::init(dir.path()).unwrap();
        let store = Store::in_memory().unwrap();
        let provider = ProviderConfig {
            kind: Kind::Ollama,
            model: "test".into(),
            base_url: Some(base_url),
            service: None,
        };
        Arc::new(Agent::new(&home, store, Some(provider), Endpoints::default()).unwrap())
    }

    async fn collect(mut answer: AnswerEvents) -> Vec<ChatEvent> {
        let mut events = Vec::new();
        while let Some(event) = answer.next().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn answers_are_streamed_and_stored() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(&dir, chat_completions(&["Hel", "lo"], Duration::ZERO).await);
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        let events = collect(
            agent
                .send(session.id, "Install nginx please", ())
                .await
                .unwrap(),
        )
        .await;

        assert!(
            matches!(&events[0], ChatEvent::User { message } if message.content == "Install nginx please")
        );
        assert!(
            matches!(&events[1], ChatEvent::Started { message } if message.status == Status::Streaming)
        );
        let streamed: String = events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "Hello");
        assert!(
            matches!(events.last(), Some(ChatEvent::Done { message, error: None, .. })
            if message.content == "Hello" && message.status == Status::Complete)
        );

        let stored = agent.messages(session.id).await.unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1].content, "Hello");
        assert_eq!(
            agent.sessions().await.unwrap()[0].title,
            "Install nginx please"
        );
        assert!(!agent.is_running(session.id));
    }

    #[tokio::test]
    async fn every_change_is_published_to_the_interfaces() {
        let dir = tempfile::tempdir().unwrap();
        let base_url = chat_completions(&["Hi"], Duration::ZERO).await;
        let agent = agent(&dir, base_url.clone());
        let mut events = agent.subscribe();

        let session = agent.create_session(None, Modes::default()).await.unwrap();
        assert_eq!(
            events.next().await,
            Some(Event::SessionCreated {
                session: session.clone()
            })
        );
        let explained = agent.set_explanations(session.id, true).await.unwrap();
        assert_eq!(
            events.next().await,
            Some(Event::SessionChanged { session: explained })
        );

        let answer = collect(agent.send(session.id, "Install nginx", ()).await.unwrap()).await;
        let Some(ChatEvent::Done { message: done, .. }) = answer.last() else {
            panic!("no end in {answer:?}");
        };
        // The first message titles the conversation.
        assert!(
            matches!(events.next().await, Some(Event::SessionChanged { session: titled })
            if titled.id == session.id && titled.title == "Install nginx")
        );
        assert!(
            matches!(events.next().await, Some(Event::AnswerStarted { session_id, user, answer })
            if session_id == session.id && user.content == "Install nginx" && answer.content.is_empty())
        );
        assert_eq!(
            events.next().await,
            Some(Event::AnswerEnded {
                session_id: session.id,
                message: done.clone(),
                error: None,
                summarised_through: None,
            })
        );
        // Told that the answer ended, an interface can send the next message at once.
        assert!(!agent.is_running(session.id));

        let renamed = agent.rename_session(session.id, "Nginx").await.unwrap();
        assert_eq!(
            events.next().await,
            Some(Event::SessionChanged { session: renamed })
        );
        agent.delete_session(session.id).await.unwrap();
        assert_eq!(
            events.next().await,
            Some(Event::SessionDeleted { id: session.id })
        );
        agent
            .configure_provider(ProviderUpdate {
                kind: Kind::Ollama,
                model: "other".into(),
                base_url: Some(base_url),
                api_key: None,
                service: None,
            })
            .unwrap();
        assert_eq!(events.next().await, Some(Event::ProviderChanged));

        assert!(agent.stop_all(Duration::from_secs(1)).await);
        assert_eq!(events.next().await, Some(Event::Stopping));
        assert_eq!(events.next().await, None);
    }

    #[tokio::test]
    async fn answers_can_be_stopped_and_keep_their_partial_text() {
        let dir = tempfile::tempdir().unwrap();
        let pieces: &'static [&'static str] = &["one ", "two ", "three"];
        let agent = agent(
            &dir,
            chat_completions(pieces, Duration::from_millis(300)).await,
        );
        let session = agent
            .create_session(Some("Stop test"), Modes::default())
            .await
            .unwrap();
        let mut answer = agent.send(session.id, "Count", ()).await.unwrap();
        assert!(matches!(
            agent.send(session.id, "Again", ()).await,
            Err(AgentError::Busy)
        ));

        while let Some(event) = answer.next().await {
            if matches!(event, ChatEvent::Delta { .. }) {
                assert!(agent.cancel(session.id));
                break;
            }
        }
        let rest = collect(answer).await;
        assert!(matches!(rest.last(), Some(ChatEvent::Done { message, .. })
            if message.status == Status::Interrupted && message.content == "one "));
        assert_eq!(agent.messages(session.id).await.unwrap()[1].content, "one ");
        assert_eq!(agent.sessions().await.unwrap()[0].title, "Stop test");
    }

    #[tokio::test]
    async fn sending_needs_a_provider_and_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let agent = Arc::new(
            Agent::new(
                &home,
                Store::in_memory().unwrap(),
                None,
                Endpoints::default(),
            )
            .unwrap(),
        );
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        assert!(matches!(
            agent.send(session.id, "Hi", ()).await,
            Err(AgentError::NotConfigured)
        ));
        assert!(matches!(
            agent.send(session.id, "   ", ()).await,
            Err(AgentError::EmptyMessage)
        ));
    }

    #[test]
    fn configuring_saves_the_provider_but_never_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let agent = Agent::new(
            &home,
            Store::in_memory().unwrap(),
            None,
            Endpoints::default(),
        )
        .unwrap();
        let info = agent
            .configure_provider(ProviderUpdate {
                kind: Kind::OpenAiCompatible,
                model: " llama3 ".into(),
                base_url: Some("http://10.0.0.5:8000/v1/".into()),
                api_key: Some("secret-key".into()),
                service: None,
            })
            .unwrap();
        assert_eq!(info.key, Some(KeySource::Session));
        let configured = info.configured.unwrap();
        assert_eq!(configured.model, "llama3");
        assert_eq!(
            configured.base_url.as_deref(),
            Some("http://10.0.0.5:8000/v1")
        );
        let text = fs::read_to_string(home.config_path()).unwrap();
        assert!(text.contains("llama3"));
        assert!(!text.contains("secret-key"));

        // Switching provider without a new key forgets the old one.
        let info = agent
            .configure_provider(ProviderUpdate {
                kind: Kind::Ollama,
                model: "qwen".into(),
                base_url: None,
                api_key: None,
                service: None,
            })
            .unwrap();
        assert_ne!(info.key, Some(KeySource::Session));

        assert!(
            agent
                .configure_provider(ProviderUpdate {
                    kind: Kind::OpenAiCompatible,
                    model: "x".into(),
                    base_url: None,
                    api_key: None,
                    service: None,
                })
                .is_err()
        );
    }

    #[test]
    fn a_configuration_without_its_service_is_recognised_by_its_address() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let service_of = |kind, base_url: Option<&str>| {
            let saved = ProviderConfig {
                kind,
                model: "model".into(),
                base_url: base_url.map(str::to_owned),
                service: None,
            };
            let agent = Agent::new(
                &home,
                Store::in_memory().unwrap(),
                Some(saved),
                Endpoints::default(),
            )
            .unwrap();
            agent.provider_info().configured.unwrap().service
        };
        let groq = services::find("groq").unwrap().base_url;
        assert_eq!(
            service_of(Kind::OpenAiCompatible, groq).as_deref(),
            Some("groq")
        );
        // Any other address keeps its destination: the service of the kind with an address of the user's.
        assert_eq!(
            service_of(Kind::OpenAiCompatible, Some("http://10.0.0.5:8000/v1")).as_deref(),
            Some("openai-compatible")
        );
        assert_eq!(
            service_of(Kind::Anthropic, None).as_deref(),
            Some("anthropic")
        );
    }

    #[test]
    fn a_service_brings_its_address_and_must_match_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let agent = Agent::new(
            &home,
            Store::in_memory().unwrap(),
            None,
            Endpoints::default(),
        )
        .unwrap();
        let update = |kind, service: &str, base_url: Option<&str>| ProviderUpdate {
            kind,
            model: "model".into(),
            base_url: base_url.map(str::to_owned),
            service: Some(service.into()),
            api_key: None,
        };

        let configured = agent
            .configure_provider(update(Kind::OpenAiCompatible, "groq", None))
            .unwrap()
            .configured
            .unwrap();
        assert_eq!(
            configured.base_url.as_deref(),
            services::find("groq").unwrap().base_url
        );
        assert_eq!(configured.service.as_deref(), Some("groq"));
        // A free service needs a key, although its kind does not always.
        assert!(matches!(
            agent.providers.settings(),
            Err(AgentError::Provider(ProviderError::MissingKey { .. }))
        ));
        let saved = Config::load(&home.config_path()).unwrap().provider;
        assert_eq!(saved, Some(configured));
        // Interfaces learn it before the first message, and that a key entered for it is enough.
        assert_eq!(agent.provider_info().needs, Some(Needed::Key));
        let with_key = ProviderUpdate {
            api_key: Some("key".into()),
            ..update(Kind::OpenAiCompatible, "groq", None)
        };
        assert_eq!(agent.configure_provider(with_key).unwrap().needs, None);

        // Ollama's answers come from under /v1, however the address was typed.
        let configured = agent
            .configure_provider(update(
                Kind::Ollama,
                "ollama",
                Some("http://10.0.0.5:11434/"),
            ))
            .unwrap()
            .configured
            .unwrap();
        assert_eq!(
            configured.base_url.as_deref(),
            Some("http://10.0.0.5:11434/v1")
        );
        assert_eq!(agent.provider_info().needs, None);

        for refused in [
            update(Kind::OpenAi, "groq", None),
            update(Kind::OpenAiCompatible, "nonexistent", None),
        ] {
            assert!(matches!(
                agent.configure_provider(refused),
                Err(AgentError::Invalid(_))
            ));
        }
    }

    #[tokio::test]
    async fn explanations_change_the_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(&dir, "http://127.0.0.1:9/v1".into());
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        assert!(!session.explanations);
        let explained = Modes {
            explanations: true,
            ..Modes::default()
        };
        assert!(
            instructions(
                Modes::default(),
                Looking::Yes,
                &kelpy_tools::Account::current(),
                None
            )
            .contains(CONCISE)
        );
        assert!(
            !instructions(
                Modes::default(),
                Looking::Yes,
                &kelpy_tools::Account::current(),
                None
            )
            .contains(EXPLAINED)
        );
        assert!(
            instructions(
                explained,
                Looking::Yes,
                &kelpy_tools::Account::current(),
                None
            )
            .contains(EXPLAINED)
        );
        // Without explanations the model is never shown how to write the panels.
        assert!(
            !instructions(
                Modes::default(),
                Looking::Yes,
                &kelpy_tools::Account::current(),
                None
            )
            .contains("```explanation")
        );

        let session = agent.set_explanations(session.id, true).await.unwrap();
        assert!(session.explanations);
        assert!(agent.sessions().await.unwrap()[0].explanations);
    }

    async fn first_delta(answer: &mut AnswerEvents) {
        while let Some(event) = answer.next().await {
            if matches!(event, ChatEvent::Delta { .. }) {
                return;
            }
        }
        panic!("the answer ended without text");
    }

    fn done(events: &[ChatEvent]) -> (&Message, Option<&str>) {
        match events.last() {
            Some(ChatEvent::Done { message, error, .. }) => (message, error.as_deref()),
            other => panic!("expected the end of the answer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_answer_under_way_can_be_followed_from_where_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let pieces: &'static [&'static str] = &["one ", "two ", "three"];
        let agent = agent(
            &dir,
            chat_completions(pieces, Duration::from_millis(200)).await,
        );
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        assert!(agent.follow(session.id).is_none());
        let mut answer = agent.send(session.id, "Count", ()).await.unwrap();
        first_delta(&mut answer).await;

        let followed = collect(agent.follow(session.id).unwrap()).await;
        let ChatEvent::Started { message } = &followed[0] else {
            panic!("a follower starts from the answer so far, got {followed:?}");
        };
        assert!(message.content.starts_with("one "));
        let rebuilt: String = followed
            .iter()
            .filter_map(|event| match event {
                ChatEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .fold(message.content.clone(), |text, piece| text + piece);
        assert_eq!(rebuilt, "one two three");
        assert_eq!(done(&followed).0.content, "one two three");
        assert_eq!(done(&collect(answer).await).0.status, Status::Complete);
    }

    #[tokio::test]
    async fn an_answer_stops_at_its_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let piece: &'static str = Box::leak("x".repeat(MAX_ANSWER / 2 + 1).into_boxed_str());
        let pieces: &'static [&'static str] = Box::leak(vec![piece; 3].into_boxed_slice());
        let agent = agent(&dir, chat_completions(pieces, Duration::ZERO).await);
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        let events = collect(agent.send(session.id, "Write a lot", ()).await.unwrap()).await;
        let (message, error) = done(&events);
        assert_eq!(message.status, Status::Interrupted);
        assert_eq!(message.content.len(), MAX_ANSWER / 2 + 1);
        assert!(
            error.is_some_and(|error| error.contains("limit")),
            "{error:?}"
        );
    }

    /// An Ollama that runs its model `test` with a context window of `window` tokens, and answers "Answered",
    /// reporting the tokens of the request.
    async fn ollama(window: u64) -> FakeServer {
        let running =
            serde_json::json!({ "models": [{ "name": "test", "context_length": window }] });
        FakeServer::start(vec![
            ("GET /api/ps", "200 OK", running.to_string()),
            (CHAT, "200 OK", stream("Answered", Some(900))),
        ])
        .await
    }

    const CHAT: &str = "POST /v1/chat/completions";

    /// A streamed chat completion of `text`, reporting `input_tokens` if given.
    fn stream(text: &str, input_tokens: Option<u64>) -> String {
        let mut events = format!(
            "data: {}\n\ndata: {}\n\n",
            serde_json::json!({ "choices": [{ "delta": { "content": text } }] }),
            serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
        );
        if let Some(tokens) = input_tokens {
            let usage = serde_json::json!({ "choices": [], "usage": { "prompt_tokens": tokens } });
            events.push_str(&format!("data: {usage}\n\n"));
        }
        events + "data: [DONE]\n\n"
    }

    /// A streamed chat completion calling `tool` with `arguments`.
    fn calling(tool: &str, arguments: &serde_json::Value) -> String {
        let call = json!({ "choices": [{ "delta": { "tool_calls": [{
            "index": 0, "id": "call_1", "type": "function",
            "function": { "name": tool, "arguments": arguments.to_string() },
        }]}}]});
        let end = json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        format!("data: {call}\n\ndata: {end}\n\ndata: [DONE]\n\n")
    }

    /// A model on Ollama with a large window, answering `text` once its queued answers are used up.
    async fn model(text: &str) -> FakeServer {
        let running = json!({ "models": [{ "name": "test", "context_length": 32_768 }] });
        FakeServer::start(vec![
            ("GET /api/ps", "200 OK", running.to_string()),
            (CHAT, "200 OK", stream(text, None)),
        ])
        .await
    }

    /// The events of `answer` up to its first action in `state`.
    async fn until(answer: &mut AnswerEvents, state: ActionState) -> Action {
        loop {
            match answer.next().await {
                Some(ChatEvent::Action { action }) if action.state == state => return *action,
                Some(_) => {}
                None => panic!("the answer ended without a {state:?} action"),
            }
        }
    }

    #[tokio::test]
    async fn an_action_waits_for_approval_and_what_it_gave_goes_back_to_the_model() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fs::write(folder.path().join("nginx.conf"), "worker_processes 2;").unwrap();
        let server = model("The folder holds nginx.conf.").await;
        let listing = json!({ "reason": "To see the files", "path": folder.path() });
        server.respond_once(CHAT, "200 OK", calling("list_directory", &listing));
        let agent = agent(&dir, format!("{}/v1", server.url));
        // Read-only / Ask, the defaults.
        let session = agent.create_session(None, Modes::default()).await.unwrap();

        let mut answer = agent
            .send(session.id, "What is in the folder?", ())
            .await
            .unwrap();
        let waiting = until(&mut answer, ActionState::Waiting).await;
        assert_eq!(waiting.summary, format!("List {}", folder.path().display()));
        assert_eq!(waiting.reason, "To see the files");
        assert_eq!(waiting.command, None);
        assert!(matches!(
            agent.decide(session.id, waiting.id + 1, true, None).await,
            Err(AgentError::NotWaiting)
        ));
        agent
            .decide(session.id, waiting.id, true, None)
            .await
            .unwrap();
        let done = until(&mut answer, ActionState::Done).await;
        assert!(
            done.output.contains("nginx.conf  19 bytes"),
            "{}",
            done.output
        );
        let rest = collect(answer).await;
        assert!(matches!(rest.last(), Some(ChatEvent::Done { message, .. })
            if message.content == "The folder holds nginx.conf."
                && message.status == Status::Complete
                && message.actions.len() == 1));

        let requests = chat_requests(&server);
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("\"tools\":[") && requests[0].contains("list_directory"));
        assert!(requests[1].contains("\"tool_call_id\":\"call_1\""));
        assert!(requests[1].contains("nginx.conf  19 bytes"));
        // The Command log stays with the conversation.
        let stored = agent.messages(session.id).await.unwrap();
        assert_eq!(stored[1].actions[0].state, ActionState::Done);
        assert_eq!(stored[1].actions[0].output, done.output);
    }

    #[tokio::test]
    async fn a_denied_action_is_reported_and_switching_to_auto_stops_the_questions() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let server = model("Looked again.").await;
        let listing = calling(
            "list_directory",
            &json!({ "reason": "Files", "path": folder.path() }),
        );
        for queued in [
            listing.clone(),
            stream("I could not look.", None),
            listing.clone(),
            stream("Looked.", None),
            listing,
        ] {
            server.respond_once(CHAT, "200 OK", queued);
        }
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent.create_session(None, Modes::default()).await.unwrap();

        let mut answer = agent.send(session.id, "Look", ()).await.unwrap();
        let waiting = until(&mut answer, ActionState::Waiting).await;
        agent
            .decide(session.id, waiting.id, false, None)
            .await
            .unwrap();
        until(&mut answer, ActionState::Denied).await;
        collect(answer).await;
        assert!(chat_requests(&server)[1].contains("The administrator denied this action."));

        let mut answer = agent.send(session.id, "Look now", ()).await.unwrap();
        let waiting = until(&mut answer, ActionState::Waiting).await;
        // Switching to Auto from the question approves it, and the next looks do not ask.
        agent
            .decide(session.id, waiting.id, true, Some(Approval::Auto))
            .await
            .unwrap();
        until(&mut answer, ActionState::Done).await;
        collect(answer).await;
        assert_eq!(agent.sessions().await.unwrap()[0].approval, Approval::Auto);

        let events = collect(agent.send(session.id, "Again", ()).await.unwrap()).await;
        let states: Vec<ActionState> = events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::Action { action } => Some(action.state),
                _ => None,
            })
            .collect();
        assert!(!states.contains(&ActionState::Waiting), "{states:?}");
        assert_eq!(states.last(), Some(&ActionState::Done));
    }

    #[tokio::test]
    async fn in_auto_a_file_change_runs_backed_up_and_its_result_goes_to_the_model() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = folder.path().join("app.ini");
        fs::write(&path, "debug = true\n").unwrap();
        let server = model("Debugging is off.").await;
        let edit = json!({ "reason": "To turn debugging off", "path": path, "old": "true", "new": "false" });
        server.respond_once(CHAT, "200 OK", calling("edit_file", &edit));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let modes = Modes {
            access: Access::Full,
            approval: Approval::Auto,
            ..Modes::default()
        };
        let session = agent.create_session(None, modes).await.unwrap();

        let events = collect(
            agent
                .send(session.id, "Turn debugging off", ())
                .await
                .unwrap(),
        )
        .await;
        let actions: Vec<&Action> = events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::Action { action } => Some(action.as_ref()),
                _ => None,
            })
            .collect();
        assert!(
            actions
                .iter()
                .all(|action| action.state != ActionState::Waiting)
        );
        let done = actions.last().unwrap();
        assert_eq!(done.state, ActionState::Done, "{:?}", done.error);
        let change = done.change.as_ref().unwrap();
        assert!(
            change.diff.contains("-debug = true\n+debug = false"),
            "{}",
            change.diff
        );
        assert_eq!(change.floor, None);
        assert_eq!(fs::read_to_string(&path).unwrap(), "debug = false\n");
        let backups = dir.path().join("data").join("backups");
        assert_eq!(fs::read_dir(backups).unwrap().count(), 1);
        let requests = chat_requests(&server);
        assert!(requests[0].contains("\"edit_file\""));
        assert!(requests[1].contains("Backed up to"));
    }

    #[tokio::test]
    async fn read_only_offers_no_change_and_refuses_one_asked_for() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = folder.path().join("new.txt");
        let server = model("I cannot.").await;
        let write = json!({ "reason": "To create it", "path": path, "content": "hello" });
        server.respond_once(CHAT, "200 OK", calling("write_file", &write));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let modes = Modes {
            approval: Approval::Auto,
            ..Modes::default()
        };
        let session = agent.create_session(None, modes).await.unwrap();

        let mut answer = agent.send(session.id, "Create it", ()).await.unwrap();
        let denied = until(&mut answer, ActionState::Denied).await;
        assert!(
            denied
                .error
                .unwrap()
                .contains("changing the system needs Full")
        );
        collect(answer).await;
        assert!(!path.exists());
        let request = &chat_requests(&server)[0];
        assert!(request.contains("\"read_file\"") && !request.contains("\"write_file\""));
    }

    #[tokio::test]
    async fn a_change_where_a_mistake_could_lock_the_administrator_out_asks_even_in_auto() {
        let dir = tempfile::tempdir().unwrap();
        let path = if cfg!(windows) {
            std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("kelpy-test.txt")
        } else {
            std::path::PathBuf::from("/etc/fstab")
        };
        let server = model("Left as it is.").await;
        let write = json!({ "reason": "A test", "path": path, "content": "kelpy test\n" });
        server.respond_once(CHAT, "200 OK", calling("write_file", &write));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let modes = Modes {
            access: Access::Full,
            approval: Approval::Auto,
            ..Modes::default()
        };
        let session = agent.create_session(None, modes).await.unwrap();

        let mut answer = agent.send(session.id, "Change it", ()).await.unwrap();
        let waiting = until(&mut answer, ActionState::Waiting).await;
        assert!(
            waiting
                .change
                .unwrap()
                .floor
                .unwrap()
                .contains("always asks")
        );
        agent
            .decide(session.id, waiting.id, false, None)
            .await
            .unwrap();
        until(&mut answer, ActionState::Denied).await;
        collect(answer).await;
        if cfg!(windows) {
            assert!(!path.exists());
        }
    }

    #[tokio::test]
    async fn a_command_the_model_wrote_waits_for_approval_even_in_auto() {
        let dir = tempfile::tempdir().unwrap();
        let server = model("It printed hello.").await;
        let command = json!({ "reason": "To check the shell", "command": "echo hello", "directory": dir.path() });
        server.respond_once(CHAT, "200 OK", calling("run_command", &command));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let modes = Modes {
            access: Access::Full,
            approval: Approval::Auto,
            ..Modes::default()
        };
        let session = agent.create_session(None, modes).await.unwrap();

        let mut answer = agent.send(session.id, "Say hello", ()).await.unwrap();
        let waiting = until(&mut answer, ActionState::Waiting).await;
        assert_eq!(
            (waiting.summary, waiting.command.as_deref()),
            (
                format!("Run a command in {}", dir.path().display()),
                Some("echo hello")
            )
        );
        agent
            .decide(session.id, waiting.id, true, None)
            .await
            .unwrap();
        let done = until(&mut answer, ActionState::Done).await;
        assert_eq!((done.exit_code, done.output.as_str()), (Some(0), "hello"));
        collect(answer).await;
        let requests = chat_requests(&server);
        assert!(requests[0].contains(CHANGING.split(". ").next().unwrap()));
        assert!(requests[1].contains("hello"));
    }

    #[tokio::test]
    async fn the_administrator_stops_one_command_and_the_answer_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let server = model("It was stopped.").await;
        let long = if cfg!(windows) {
            "'Copying'; Start-Sleep -Seconds 120"
        } else {
            "echo Copying; sleep 120"
        };
        let command =
            json!({ "reason": "A long backup", "command": long, "directory": dir.path() });
        server.respond_once(CHAT, "200 OK", calling("run_command", &command));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent.create_session(None, Modes::default()).await.unwrap();

        let mut answer = agent.send(session.id, "Back it up", ()).await.unwrap();
        let waiting = until(&mut answer, ActionState::Waiting).await;
        assert!(matches!(
            agent.stop_action(session.id, waiting.id),
            Err(AgentError::NotRunning)
        ));
        agent
            .decide(session.id, waiting.id, true, None)
            .await
            .unwrap();
        let mut running = until(&mut answer, ActionState::Running).await;
        assert!(running.started_at.is_some());
        // What it prints shows while it runs.
        while running.output.is_empty() {
            running = until(&mut answer, ActionState::Running).await;
        }
        assert_eq!(running.output, "Copying");
        agent.stop_action(session.id, running.id).unwrap();
        let failed = until(&mut answer, ActionState::Failed).await;
        assert_eq!(
            (failed.error.as_deref(), failed.output.as_str()),
            (Some("stopped by the administrator"), "Copying")
        );
        let rest = collect(answer).await;
        assert!(matches!(rest.last(), Some(ChatEvent::Done { message, .. })
            if message.content == "It was stopped." && message.status == Status::Complete));
        assert!(chat_requests(&server)[1].contains(
            "It did not complete: stopped by the administrator. Its output until then:\\nCopying"
        ));
    }

    #[tokio::test]
    async fn without_access_no_tools_are_offered() {
        let dir = tempfile::tempdir().unwrap();
        let server = model("Run `df -h`.").await;
        let agent = agent(&dir, format!("{}/v1", server.url));
        let modes = Modes {
            access: Access::None,
            ..Modes::default()
        };
        let session = agent.create_session(None, modes).await.unwrap();
        collect(agent.send(session.id, "Disks?", ()).await.unwrap()).await;
        let request = &chat_requests(&server)[0];
        assert!(!request.contains("\"tools\""));
        assert!(request.contains("has not let you look at the system"));
    }

    #[tokio::test]
    async fn stopping_while_an_action_waits_ends_the_answer() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let server = model("Never asked for.").await;
        let listing = json!({ "reason": "Files", "path": folder.path() });
        server.respond_once(CHAT, "200 OK", calling("list_directory", &listing));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent.create_session(None, Modes::default()).await.unwrap();

        let mut answer = agent.send(session.id, "Look", ()).await.unwrap();
        until(&mut answer, ActionState::Waiting).await;
        assert!(agent.cancel(session.id));
        let rest = collect(answer).await;
        assert!(matches!(rest.last(), Some(ChatEvent::Done { message, .. })
            if message.status == Status::Interrupted));
        let action = &agent.messages(session.id).await.unwrap()[1].actions[0];
        assert_eq!(action.state, ActionState::Failed);
        assert_eq!(action.error.as_deref(), Some("stopped before it ran"));
        assert_eq!(chat_requests(&server).len(), 1);
    }

    #[tokio::test]
    async fn setting_access_to_none_denies_the_action_waiting() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fs::write(folder.path().join("secret.txt"), "the content").unwrap();
        let server = model("Nothing read.").await;
        let reading = json!({ "reason": "To read it", "path": folder.path().join("secret.txt") });
        server.respond_once(CHAT, "200 OK", calling("read_file", &reading));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent.create_session(None, Modes::default()).await.unwrap();

        let mut answer = agent.send(session.id, "Read it", ()).await.unwrap();
        let waiting = until(&mut answer, ActionState::Waiting).await;
        agent.set_access(session.id, Access::None).await.unwrap();
        // A tab that still shows the question.
        assert!(matches!(
            agent.decide(session.id, waiting.id, true, None).await,
            Err(AgentError::NotWaiting)
        ));
        let denied = until(&mut answer, ActionState::Denied).await;
        assert_eq!(
            denied.error.as_deref(),
            Some("System access is None in this conversation")
        );
        collect(answer).await;
        assert!(!chat_requests(&server)[1].contains("the content"));
    }

    #[tokio::test]
    async fn an_approval_runs_nothing_once_access_is_none() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fs::write(folder.path().join("secret.txt"), "the content").unwrap();
        let server = model("Nothing read.").await;
        let reading = json!({ "reason": "To read it", "path": folder.path().join("secret.txt") });
        server.respond_once(CHAT, "200 OK", calling("read_file", &reading));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent.create_session(None, Modes::default()).await.unwrap();

        let mut answer = agent.send(session.id, "Read it", ()).await.unwrap();
        let waiting = until(&mut answer, ActionState::Waiting).await;
        // Access changed while the approval was on its way: what is stored decides.
        let id = session.id;
        agent
            .store
            .call(move |store| store.set_access(id, Access::None))
            .await
            .unwrap();
        agent
            .decide(session.id, waiting.id, true, None)
            .await
            .unwrap();
        let denied = until(&mut answer, ActionState::Denied).await;
        assert_eq!(denied.output, "");
        collect(answer).await;
        assert!(!chat_requests(&server)[1].contains("the content"));
        let stored = &agent.messages(session.id).await.unwrap()[1].actions[0];
        assert_eq!(stored.state, ActionState::Denied);
    }

    #[tokio::test]
    async fn a_decision_refused_changes_no_mode() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(&dir, "http://127.0.0.1:9/v1".into());
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        assert!(matches!(
            agent
                .decide(session.id, 1, true, Some(Approval::Auto))
                .await,
            Err(AgentError::NotWaiting)
        ));
        assert_eq!(agent.sessions().await.unwrap()[0].approval, Approval::Ask);
    }

    #[tokio::test]
    async fn the_question_stands_if_the_mode_chosen_with_it_cannot_be_saved() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let home = Home::init(dir.path()).unwrap();
        let path = home.data_dir().join("kelpy.db");
        let server = model("Looked.").await;
        let listing = json!({ "reason": "Files", "path": folder.path() });
        server.respond_once(CHAT, "200 OK", calling("list_directory", &listing));
        let provider = ProviderConfig {
            kind: Kind::Ollama,
            model: "test".into(),
            base_url: Some(format!("{}/v1", server.url)),
            service: None,
        };
        let store = Store::open(&path).unwrap();
        let agent = Agent::new(&home, store, Some(provider), Endpoints::default()).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER no_mode_save BEFORE UPDATE OF approval ON sessions
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        let session = agent.create_session(None, Modes::default()).await.unwrap();

        let mut answer = agent.send(session.id, "Look", ()).await.unwrap();
        let waiting = until(&mut answer, ActionState::Waiting).await;
        assert!(matches!(
            agent
                .decide(session.id, waiting.id, true, Some(Approval::Auto))
                .await,
            Err(AgentError::Store(_))
        ));
        agent
            .decide(session.id, waiting.id, true, None)
            .await
            .unwrap();
        until(&mut answer, ActionState::Done).await;
        collect(answer).await;
        assert_eq!(agent.sessions().await.unwrap()[0].approval, Approval::Ask);
    }

    #[tokio::test]
    async fn an_action_that_cannot_be_saved_ends_the_answer_as_failed() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let home = Home::init(dir.path()).unwrap();
        let path = home.data_dir().join("kelpy.db");
        let server = model("Never written.").await;
        let listing = json!({ "reason": "Files", "path": folder.path() });
        server.respond_once(CHAT, "200 OK", calling("list_directory", &listing));
        let provider = ProviderConfig {
            kind: Kind::Ollama,
            model: "test".into(),
            base_url: Some(format!("{}/v1", server.url)),
            service: None,
        };
        let store = Store::open(&path).unwrap();
        let agent = Agent::new(&home, store, Some(provider), Endpoints::default()).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER no_action_save BEFORE UPDATE ON actions
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        let modes = Modes {
            approval: Approval::Auto,
            ..Modes::default()
        };
        let session = agent.create_session(None, modes).await.unwrap();

        let events = collect(agent.send(session.id, "Look", ()).await.unwrap()).await;
        let (message, error) = done(&events);
        assert_eq!(message.status, Status::Failed);
        assert!(
            error.is_some_and(|error| error.contains("could not be saved")),
            "{error:?}"
        );
        assert!(!events.iter().any(|event| matches!(event,
            ChatEvent::Action { action } if action.state == ActionState::Done)));
        // The model is not given a result Kelpy could not record.
        assert_eq!(chat_requests(&server).len(), 1);
    }

    #[test]
    fn each_access_tells_the_model_what_it_may_do() {
        assert!(
            instructions(
                Modes::default(),
                Looking::Yes,
                &kelpy_tools::Account::current(),
                None
            )
            .contains(LOOKING)
        );
        let full = Modes {
            access: Access::Full,
            ..Modes::default()
        };
        let told = instructions(full, Looking::Yes, &kelpy_tools::Account::current(), None);
        assert!(told.contains(CHANGING) && !told.contains(LOOKING));
        for (looking, words) in [
            (Looking::NotAllowed, "System access is None"),
            (Looking::Unsupported, "This model cannot use tools"),
            (Looking::Exhausted, "answer now with what you found"),
        ] {
            assert!(
                instructions(
                    Modes::default(),
                    looking,
                    &kelpy_tools::Account::current(),
                    None
                )
                .contains(words),
                "{looking:?}"
            );
        }
    }

    fn chat_requests(server: &FakeServer) -> Vec<String> {
        let requests = server.requests().into_iter();
        requests
            .filter(|request| request.starts_with(CHAT))
            .collect()
    }

    /// Stores finished turns in a conversation: each question with its answer.
    async fn seed(agent: &Agent, session_id: i64, turns: &[(&str, &str)]) {
        let turns: Vec<(String, String)> = turns
            .iter()
            .map(|(question, answer)| ((*question).to_owned(), (*answer).to_owned()))
            .collect();
        agent
            .store
            .call(move |store| {
                for (question, answer) in &turns {
                    let turn = store.start_turn(session_id, question, "x", "test")?;
                    store.update_message(turn.answer.id, answer, Status::Complete)?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    /// Three turns of about 1,500 tokens each, beginning with their names: too much for a window of 4,096.
    async fn long_conversation(agent: &Agent, session_id: i64) {
        let long = |name: &str| format!("{name} {}", "x".repeat(3_000));
        let turns = ["FIRST", "SECOND", "THIRD"].map(|name| {
            (
                long(&format!("{name}-QUESTION")),
                long(&format!("{name}-ANSWER")),
            )
        });
        let turns: Vec<(&str, &str)> = turns
            .iter()
            .map(|(question, answer)| (question.as_str(), answer.as_str()))
            .collect();
        seed(agent, session_id, &turns).await;
    }

    #[tokio::test]
    async fn a_long_conversation_is_summarised_for_the_model_and_kept_whole() {
        let dir = tempfile::tempdir().unwrap();
        let server = ollama(4_096).await;
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        long_conversation(&agent, session.id).await;
        // Each older message fills a request to summarise: four parts, each folded into the summary so far.
        for part in 1..=4 {
            let summary = format!("## Server\nDebian 13, part {part}");
            server.respond_once(CHAT, "200 OK", stream(&summary, None));
        }

        let events = collect(agent.send(session.id, "What next?", ()).await.unwrap()).await;
        assert_eq!(
            done(&events),
            (&agent.messages(session.id).await.unwrap()[7], None)
        );
        assert_eq!(done(&events).0.content, "Answered");

        let requests = chat_requests(&server);
        let [summarising @ .., answering] = &requests[..] else {
            unreachable!()
        };
        assert_eq!(summarising.len(), 4);
        assert!(
            summarising
                .iter()
                .all(|request| request.contains("You keep the record"))
        );
        assert!(
            summarising[0].contains("FIRST-QUESTION")
                && !summarising[0].contains("previous-summary")
        );
        assert!(summarising[1].contains(r"<previous-summary>\n## Server\nDebian 13, part 1"));
        assert!(summarising[3].contains("SECOND-ANSWER"));
        assert!(!summarising.concat().contains("THIRD-QUESTION"));
        // The model reads the summary, then the recent turn and the question as they are.
        assert!(
            answering.contains("## Earlier in this conversation") && answering.contains("part 4")
        );
        assert!(!answering.contains("FIRST-QUESTION") && !answering.contains("SECOND-ANSWER"));
        assert!(answering.contains("THIRD-QUESTION") && answering.contains("What next?"));

        let messages = agent.messages(session.id).await.unwrap();
        assert_eq!(messages.len(), 8);
        assert!(
            matches!(events.last(), Some(ChatEvent::Done { summarised_through: Some(through), .. })
            if *through == messages[3].id)
        );
        assert_eq!(
            agent.summarised_through(session.id).await.unwrap(),
            Some(messages[3].id)
        );
        let summary = agent
            .store
            .call(move |store| store.summary(session.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary.through, messages[3].id);
        assert_eq!(summary.content, "## Server\nDebian 13, part 4");
        // What the request took is kept, to correct the next estimates.
        assert_eq!(messages[7].input_tokens, Some(900));
        assert!(
            messages[7]
                .context_estimate
                .is_some_and(|estimate| estimate > 0)
        );

        // The next answer reads the same summary, without writing it again.
        let events = collect(agent.send(session.id, "And then?", ()).await.unwrap()).await;
        assert_eq!(done(&events).0.status, Status::Complete);
        let requests = chat_requests(&server);
        assert_eq!(requests.len(), 6);
        assert!(requests[5].contains("part 4") && !requests[5].contains("You keep the record"));
    }

    #[tokio::test]
    async fn without_a_summary_the_oldest_whole_turns_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let server = ollama(4_096).await;
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        long_conversation(&agent, session.id).await;
        server.respond_once(CHAT, "500 Internal Server Error", "{}".to_owned());

        let events = collect(agent.send(session.id, "What next?", ()).await.unwrap()).await;
        assert_eq!(done(&events).0.content, "Answered");
        let answering = &chat_requests(&server)[1];
        assert!(answering.contains("were left out"));
        assert!(!answering.contains("FIRST-QUESTION") && !answering.contains("SECOND-ANSWER"));
        assert!(answering.contains("What next?"));
        let summary = agent
            .store
            .call(move |store| store.summary(session.id))
            .await;
        assert_eq!(summary.unwrap(), None);
    }

    #[tokio::test]
    async fn a_request_refused_as_too_long_is_fitted_again_a_few_times() {
        let dir = tempfile::tempdir().unwrap();
        let server = ollama(100_000).await;
        let agent = agent(&dir, format!("{}/v1", server.url));
        let too_long = serde_json::json!({ "error": { "message":
            "This model's maximum context length is 2048 tokens. However, you requested 2100 tokens." } })
        .to_string();
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        for _ in 0..2 {
            server.respond_once(CHAT, "400 Bad Request", too_long.clone());
        }
        let events = collect(agent.send(session.id, "Hi", ()).await.unwrap()).await;
        assert_eq!(
            done(&events),
            (&agent.messages(session.id).await.unwrap()[1], None)
        );
        assert_eq!(chat_requests(&server).len(), 3);
        // The smaller window is remembered.
        let settings = agent.providers.settings().unwrap();
        assert_eq!(agent.turns.window(&settings).await, 44_444);

        server.respond(CHAT, "400 Bad Request", too_long);
        let events = collect(agent.send(session.id, "Again", ()).await.unwrap()).await;
        let (message, error) = done(&events);
        assert_eq!(message.status, Status::Failed);
        assert!(error.is_some_and(|error| error.contains("maximum context length")));
        assert_eq!(chat_requests(&server).len(), 3 + 4);
    }

    #[tokio::test]
    async fn a_message_too_long_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(&dir, chat_completions(&["Hi"], Duration::ZERO).await);
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        let long = "x".repeat(MAX_MESSAGE + 1);
        assert!(matches!(
            agent.send(session.id, &long, ()).await,
            Err(AgentError::Invalid(_))
        ));
        assert!(agent.messages(session.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_memory_index_is_read_up_to_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(&dir, "http://127.0.0.1:9/v1".into());
        // A character cut by the limit is left out.
        fs::write(
            &agent.turns.memory_index,
            format!("a{}", "è".repeat(MAX_MEMORY_INDEX)),
        )
        .unwrap();
        let memory = agent.turns.memory().await.unwrap();
        assert_eq!(memory.len(), MAX_MEMORY_INDEX - 1);
        assert!(
            instructions(
                Modes::default(),
                Looking::Yes,
                &kelpy_tools::Account::current(),
                Some(&memory)
            )
            .ends_with(&memory)
        );
    }

    #[tokio::test]
    async fn closing_stops_downloads_waits_for_the_work_under_way_then_closes_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeServer::start(vec![("POST /api/pull", "200 OK", String::new())]).await;
        let _held = server.hold("POST /api/pull");
        let agent = agent(&dir, server.url.clone());
        let download = agent
            .ollama()
            .download(&server.url, "qwen3:8b", ())
            .unwrap();
        server.received("/api/pull").await;

        assert!(agent.stop_all(Duration::from_secs(1)).await);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let closed = tokio::time::timeout(Duration::from_secs(2), agent.close(deadline)).await;
        assert_eq!(closed.ok(), Some(true), "the download was not stopped");
        let ended = download.borrow().clone();
        assert!(ended.done && ended.error.is_some(), "{ended:?}");
        assert!(matches!(
            agent.sessions().await,
            Err(AgentError::Store(StoreError::Unavailable))
        ));
    }

    #[tokio::test]
    async fn no_download_starts_once_kelpy_is_stopping() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeServer::start(vec![("POST /api/pull", "200 OK", String::new())]).await;
        let agent = agent(&dir, server.url.clone());
        let _stopped = agent.stop_all(Duration::from_secs(1));
        let refused = agent.ollama().download(&server.url, "qwen3:8b", ());
        assert!(matches!(refused, Err(crate::ollama::Stopping)));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(server.requests().is_empty(), "a download started");
    }

    #[tokio::test]
    async fn nothing_starts_once_kelpy_is_stopping() {
        let dir = tempfile::tempdir().unwrap();
        let pieces: &'static [&'static str] = &["one ", "two ", "three"];
        let agent = agent(
            &dir,
            chat_completions(pieces, Duration::from_millis(300)).await,
        );
        let running = agent.create_session(None, Modes::default()).await.unwrap();
        let mut answer = agent.send(running.id, "Count", ()).await.unwrap();
        first_delta(&mut answer).await;

        // Refusing starts when `stop_all` is called, before its future runs.
        let stopped = agent.stop_all(Duration::from_secs(5));
        let other = agent.create_session(None, Modes::default()).await.unwrap();
        assert!(matches!(
            agent.send(other.id, "Hi", ()).await,
            Err(AgentError::Stopping)
        ));
        assert!(stopped.await);
        assert!(!agent.is_running(running.id));
        assert_eq!(
            agent.messages(running.id).await.unwrap()[1].status,
            Status::Interrupted
        );
        assert!(agent.messages(other.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_conversation_waits_for_its_answer_and_spares_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let pieces: &'static [&'static str] = &["one ", "two ", "three"];
        let agent = agent(
            &dir,
            chat_completions(pieces, Duration::from_millis(200)).await,
        );
        let first = agent.create_session(None, Modes::default()).await.unwrap();
        let mut answer = agent.send(first.id, "A", ()).await.unwrap();
        first_delta(&mut answer).await;
        agent.delete_session(first.id).await.unwrap();
        assert!(!agent.is_running(first.id));
        let rest = collect(answer).await;
        assert_eq!(done(&rest).0.status, Status::Interrupted);

        let second = agent.create_session(None, Modes::default()).await.unwrap();
        let events = collect(agent.send(second.id, "B", ()).await.unwrap()).await;
        let (message, error) = done(&events);
        assert_eq!((message.status, error), (Status::Complete, None));
        let stored = agent.messages(second.id).await.unwrap();
        assert_eq!(stored[0].content, "B");
        assert_eq!(stored[1].content, "one two three");
        assert_eq!(stored[1].status, Status::Complete);
    }

    /// Polls a future once: `None` if it is still pending.
    async fn poll_once<F: Future + Unpin>(future: &mut F) -> Option<F::Output> {
        tokio::select! {
            biased;
            output = future => Some(output),
            () = std::future::ready(()) => None,
        }
    }

    #[tokio::test]
    async fn no_answer_starts_while_its_conversation_is_being_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let pieces: &'static [&'static str] = &["one ", "two ", "three"];
        let agent = agent(
            &dir,
            chat_completions(pieces, Duration::from_millis(200)).await,
        );
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        let mut answer = agent.send(session.id, "A", ()).await.unwrap();
        first_delta(&mut answer).await;
        let answer_end = lock(&agent.turns.answers).running[&session.id].done.clone();

        // The deletion stops the answer and waits for it; it is polled again only at the end.
        let mut deletion = Box::pin(agent.delete_session(session.id));
        assert!(poll_once(&mut deletion).await.is_none());
        assert!(matches!(
            agent.send(session.id, "B", ()).await,
            Err(AgentError::Deleting)
        ));
        // The old answer has ended, the conversation is not deleted yet: still no new answer.
        answer_end.cancelled().await;
        assert!(matches!(
            agent.send(session.id, "B", ()).await,
            Err(AgentError::Deleting)
        ));
        assert!(matches!(
            agent.delete_session(session.id).await,
            Err(AgentError::Deleting)
        ));

        deletion.await.unwrap();
        assert!(agent.sessions().await.unwrap().is_empty());
        assert!(lock(&agent.turns.answers).deleting.is_empty());
    }

    #[tokio::test]
    async fn an_abandoned_deletion_frees_the_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let pieces: &'static [&'static str] = &["one ", "two ", "three"];
        let agent = agent(
            &dir,
            chat_completions(pieces, Duration::from_millis(200)).await,
        );
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        let mut answer = agent.send(session.id, "A", ()).await.unwrap();
        first_delta(&mut answer).await;

        let mut deletion = Box::pin(agent.delete_session(session.id));
        assert!(poll_once(&mut deletion).await.is_none());
        drop(deletion);
        assert_eq!(done(&collect(answer).await).0.status, Status::Interrupted);

        let events = collect(agent.send(session.id, "B", ()).await.unwrap()).await;
        assert_eq!(done(&events).0.status, Status::Complete);
        assert_eq!(agent.messages(session.id).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn chatgpt_answers_are_paid_by_the_session_and_need_one() {
        use crate::chatgpt::tests::{fake_openai, sign_in};
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let (server, endpoints) = fake_openai().await;
        server.respond(
            "POST /v1/responses",
            "200 OK",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Done\"}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
                .to_owned(),
        );
        let provider = ProviderConfig {
            kind: Kind::ChatGpt,
            model: "gpt-test".into(),
            base_url: Some(endpoints.api.clone()),
            service: None,
        };
        let agent = Arc::new(
            Agent::new(
                &home,
                Store::in_memory().unwrap(),
                Some(provider),
                endpoints,
            )
            .unwrap(),
        );
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        assert_eq!(agent.provider_info().needs, Some(Needed::SignIn));
        assert!(matches!(
            agent.send(session.id, "Hi", ()).await,
            Err(AgentError::ChatGpt(ChatGptError::NotSignedIn))
        ));
        assert!(agent.messages(session.id).await.unwrap().is_empty());

        sign_in(agent.chatgpt(), &server, "user-1", 3600)
            .await
            .unwrap();
        assert_eq!(agent.provider_info().needs, None);
        let events = collect(agent.send(session.id, "Hi", ()).await.unwrap()).await;
        assert_eq!(done(&events).0.content, "Done");
        let inference = server
            .requests()
            .into_iter()
            .find(|request| request.starts_with("POST /v1/responses"))
            .unwrap();
        assert!(
            inference
                .to_ascii_lowercase()
                .contains("authorization: bearer access-user-1")
        );
        assert!(inference.contains("\"store\":false"));
    }

    #[tokio::test]
    async fn an_answer_that_cannot_be_saved_is_reported_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let path = home.data_dir().join("kelpy.db");
        let provider = ProviderConfig {
            kind: Kind::Ollama,
            model: "test".into(),
            base_url: Some(chat_completions(&["Hello"], Duration::ZERO).await),
            service: None,
        };
        let agent = Arc::new(
            Agent::new(
                &home,
                Store::open(&path).unwrap(),
                Some(provider),
                Endpoints::default(),
            )
            .unwrap(),
        );
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER no_final_save BEFORE UPDATE OF status ON messages
                 WHEN NEW.status <> 'streaming' BEGIN SELECT RAISE(FAIL, 'injected failure'); END;",
            )
            .unwrap();

        let session = agent.create_session(None, Modes::default()).await.unwrap();
        let events = collect(agent.send(session.id, "Hi", ()).await.unwrap()).await;
        let (message, error) = done(&events);
        assert_eq!(message.status, Status::Failed);
        assert!(error.is_some_and(|error| error.contains("could not be saved")));
        assert_eq!(
            agent.messages(session.id).await.unwrap()[1].status,
            Status::Streaming
        );
    }

    #[test]
    fn concurrent_provider_changes_stay_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let agent = Arc::new(
            Agent::new(
                &home,
                Store::in_memory().unwrap(),
                None,
                Endpoints::default(),
            )
            .unwrap(),
        );
        let threads: Vec<_> = (0..16)
            .map(|n| {
                let agent = Arc::clone(&agent);
                std::thread::spawn(move || {
                    agent.configure_provider(ProviderUpdate {
                        kind: Kind::Ollama,
                        model: format!("model-{n}"),
                        base_url: None,
                        api_key: None,
                        service: None,
                    })
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let in_memory = agent.provider_info().configured.unwrap().model;
        let on_disk = crate::config::Config::load(&home.config_path())
            .unwrap()
            .provider
            .unwrap()
            .model;
        assert_eq!(in_memory, on_disk);
    }

    #[test]
    fn a_key_is_never_sent_to_another_address() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let agent = Agent::new(
            &home,
            Store::in_memory().unwrap(),
            None,
            Endpoints::default(),
        )
        .unwrap();
        let update = |model: &str, base_url: &str, api_key: Option<&str>| ProviderUpdate {
            kind: Kind::OpenAiCompatible,
            model: model.into(),
            base_url: Some(base_url.into()),
            api_key: api_key.map(str::to_owned),
            service: None,
        };
        let info = agent
            .configure_provider(update("a", "http://10.0.0.5:8000/v1", Some("secret")))
            .unwrap();
        assert_eq!(info.key, Some(KeySource::Session));
        // Same address, another model: the key stays.
        let info = agent
            .configure_provider(update("b", "http://10.0.0.5:8000/v1/", None))
            .unwrap();
        assert_eq!(info.key, Some(KeySource::Session));
        // Another address: the key is forgotten.
        let info = agent
            .configure_provider(update("b", "http://10.0.0.6:8000/v1", None))
            .unwrap();
        assert_ne!(info.key, Some(KeySource::Session));
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(
            title_from("\n  Install WordPress\nthen nginx"),
            "Install WordPress"
        );
        let long =
            "Configure nginx as a reverse proxy for the application running on port 8080 please";
        let title = title_from(long);
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= TITLE_LENGTH + 1);
        assert!(!title.contains("  "));
    }

    #[test]
    fn recommended_models_come_first_and_the_rest_in_name_order() {
        let listed = Ok(vec![
            "b".to_owned(),
            "rec-2".to_owned(),
            "a".to_owned(),
            "b".to_owned(),
        ]);
        let list = ModelList::new(&["rec-1", "rec-2"], listed);
        let ids: Vec<(&str, bool)> = list
            .models
            .iter()
            .map(|choice| (choice.id.as_str(), choice.recommended))
            .collect();
        assert_eq!(
            ids,
            [("rec-1", true), ("rec-2", true), ("a", false), ("b", false)]
        );
        assert_eq!(list.error, None);

        let refused = ModelList::new(&["rec-1"], Err(ProviderError::Network("down".into())));
        assert_eq!(refused.models.len(), 1);
        assert!(refused.error.unwrap().contains("down"));
    }

    #[tokio::test]
    async fn a_session_key_lists_models_only_at_its_own_endpoint() {
        let list = serde_json::json!({ "data": [{ "id": "m" }] }).to_string();
        let own = FakeServer::start(vec![("GET /v1/models", "200 OK", list.clone())]).await;
        let other = FakeServer::start(vec![("GET /v1/models", "200 OK", list)]).await;
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(&dir, format!("{}/v1", own.url));
        let compatible = |url: &str, api_key: Option<&str>| ModelQuery {
            kind: Kind::OpenAiCompatible,
            service: Some("openai-compatible".into()),
            base_url: Some(format!("{url}/v1")),
            api_key: api_key.map(str::to_owned),
        };
        agent
            .configure_provider(ProviderUpdate {
                kind: Kind::OpenAiCompatible,
                model: "m".into(),
                base_url: Some(format!("{}/v1", own.url)),
                service: Some("openai-compatible".into()),
                api_key: Some("session-key".into()),
            })
            .unwrap();

        let listed = agent.models(compatible(&own.url, None)).await.unwrap();
        assert_eq!(listed.models[0].id, "m");
        assert!(own.requests()[0].contains("Bearer session-key"));

        agent.models(compatible(&other.url, None)).await.unwrap();
        assert!(!other.requests()[0].contains("authorization"));
        agent
            .models(compatible(&other.url, Some("typed-key")))
            .await
            .unwrap();
        assert!(other.requests()[1].contains("Bearer typed-key"));
    }

    #[tokio::test]
    async fn a_send_given_up_while_the_database_is_busy_still_ends_its_answer() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(&dir, chat_completions(&["Hello"], Duration::ZERO).await);
        let session = agent.create_session(None, Modes::default()).await.unwrap();
        // The database is busy: the send waits for it, and its caller gives up meanwhile.
        assert!(
            agent
                .store
                .submit(|_| std::thread::sleep(Duration::from_millis(300)))
        );
        let send = agent.send(session.id, "Hi", ());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), send)
                .await
                .is_err()
        );

        let ended = async {
            while agent.is_running(session.id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), ended)
            .await
            .expect("the conversation stayed busy");
        let stored = agent.messages(session.id).await.unwrap();
        assert_eq!(
            stored
                .last()
                .map(|message| (message.content.as_str(), message.status)),
            Some(("Hello", Status::Complete))
        );
    }

    /// The actions of `events`, in order.
    fn actions_of(events: &[ChatEvent]) -> Vec<Action> {
        events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::Action { action } => Some(action.as_ref().clone()),
                _ => None,
            })
            .collect()
    }

    fn full(approval: Approval) -> Modes {
        Modes {
            access: Access::Full,
            approval,
            ..Modes::default()
        }
    }

    #[tokio::test]
    async fn a_plan_approved_once_lets_its_steps_run_and_a_change_outside_it_ask() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (site, other) = (
            folder.path().join("site.conf"),
            folder.path().join("other.conf"),
        );
        let write = json!({ "path": site, "content": "listen 80;\n" });
        let server = model("Done.").await;
        let plan = json!({
            "reason": "To set up the site",
            "goal": "Set up the site",
            "steps": [{ "tool": "write_file", "arguments": write }],
        });
        let mut step = write.clone();
        step["reason"] = json!("The site's configuration");
        for queued in [
            calling("propose_plan", &plan),
            calling("write_file", &step),
            calling(
                "write_file",
                &json!({ "reason": "Also this", "path": other, "content": "x\n" }),
            ),
            // The same step again: carried out already, so it asks.
            calling("write_file", &step),
        ] {
            server.respond_once(CHAT, "200 OK", queued);
        }
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent
            .create_session(None, full(Approval::Plan))
            .await
            .unwrap();

        let mut answer = agent.send(session.id, "Set it up", ()).await.unwrap();
        let proposed = until(&mut answer, ActionState::Waiting).await;
        assert_eq!(proposed.tool, "propose_plan");
        let shown = proposed.plan.as_ref().unwrap();
        assert_eq!(shown.goal, "Set up the site");
        assert_eq!(shown.steps[0].summary, format!("Write {}", site.display()));
        assert_eq!(shown.steps[0].detail.as_deref(), Some("+listen 80;"));
        agent
            .decide(session.id, proposed.id, true, None)
            .await
            .unwrap();
        // The step runs without asking.
        let written = until(&mut answer, ActionState::Done).await;
        assert_eq!(written.tool, "propose_plan");
        let written = until(&mut answer, ActionState::Done).await;
        assert_eq!(written.plan_step.unwrap().plan_id, proposed.id);
        assert_eq!(fs::read_to_string(&site).unwrap(), "listen 80;\n");
        // Outside the plan: it asks, and says why.
        let outside = until(&mut answer, ActionState::Waiting).await;
        assert!(
            outside
                .change
                .as_ref()
                .unwrap()
                .notes
                .iter()
                .any(|note| note.starts_with("Not in the plan approved"))
        );
        agent
            .decide(session.id, outside.id, false, None)
            .await
            .unwrap();
        // The step again: the system is as asked already, so it ends without running.
        let again = until(&mut answer, ActionState::Done).await;
        assert!(
            again.output.contains("nothing to change"),
            "{}",
            again.output
        );
        assert_eq!(again.plan_step, None);
        collect(answer).await;
        // propose_plan is offered in Plan only.
        assert!(chat_requests(&server)[0].contains("\"propose_plan\""));
    }

    #[tokio::test]
    async fn a_secret_is_written_where_it_goes_and_never_reaches_the_model() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let config = folder.path().join("wp-config.php");
        let server = model("Configured.").await;
        for queued in [
            calling(
                "generate_secret",
                &json!({ "reason": "Database password", "name": "wp_db" }),
            ),
            calling(
                "write_file",
                &json!({
                    "reason": "WordPress's configuration",
                    "path": config,
                    "content": "<?php define('DB_PASSWORD', '{{secret:wp_db}}');\n",
                }),
            ),
            calling(
                "edit_file",
                &json!({ "reason": "x", "path": config, "old": "DB_", "new": "X_" }),
            ),
        ] {
            server.respond_once(CHAT, "200 OK", queued);
        }
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent
            .create_session(None, full(Approval::Auto))
            .await
            .unwrap();
        let events = collect(agent.send(session.id, "Configure", ()).await.unwrap()).await;
        let actions = actions_of(&events);
        let written = fs::read_to_string(&config).unwrap();
        let value = &written["<?php define('DB_PASSWORD', '".len()..written.len() - "');\n".len()];
        assert_eq!(value.len(), 32);
        assert!(
            actions
                .iter()
                .all(|action| action.state != ActionState::Waiting)
        );
        // A secret file is written whole, never edited.
        let edit = actions.last().unwrap();
        assert_eq!(edit.state, ActionState::Failed);
        assert!(edit.error.as_deref().unwrap().contains("may hold secrets"));
        let stored = serde_json::to_string(&agent.messages(session.id).await.unwrap()).unwrap();
        assert!(!stored.contains(value));
        for request in chat_requests(&server) {
            assert!(!request.contains(value));
        }
        // The report says where it went, not what it is.
        let report = agent.report(session.id).await.unwrap();
        assert!(
            report.contains("- wp_db: in ") && report.contains("wp-config.php\n"),
            "{report}"
        );
        assert!(!report.contains(value));
    }

    #[tokio::test]
    async fn a_change_is_restored_from_the_command_log_and_the_model_is_told() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = folder.path().join("app.ini");
        fs::write(&path, "debug = true\n").unwrap();
        let server = model("Done.").await;
        let edit = json!({ "reason": "Off", "path": path, "old": "true", "new": "false" });
        server.respond_once(CHAT, "200 OK", calling("edit_file", &edit));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent
            .create_session(None, full(Approval::Auto))
            .await
            .unwrap();
        let events = collect(agent.send(session.id, "Turn it off", ()).await.unwrap()).await;
        let edited = actions_of(&events).pop().unwrap();
        assert!(edited.change.as_ref().unwrap().backup.is_some());

        let Operated::Preview { change, .. } =
            agent.restore(session.id, edited.id, true).await.unwrap()
        else {
            panic!("not a preview");
        };
        assert!(
            change.diff.contains("-debug = false\n+debug = true"),
            "{}",
            change.diff
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "debug = false\n");
        let mut events = agent.subscribe();
        let Operated::Done { action } = agent.restore(session.id, edited.id, false).await.unwrap()
        else {
            panic!("not done");
        };
        assert_eq!(action.state, ActionState::Done, "{:?}", action.error);
        assert_eq!(action.message_id, None);
        assert_eq!(fs::read_to_string(&path).unwrap(), "debug = true\n");
        // The version it replaced was backed up too.
        assert!(action.change.as_ref().unwrap().backup.is_some());
        assert!(matches!(
            events.next().await,
            Some(Event::ActionChanged { .. })
        ));
        assert_eq!(
            agent.administrator_actions(session.id).await.unwrap(),
            [*action]
        );
        // Told to the model with the next message.
        collect(agent.send(session.id, "Is it on?", ()).await.unwrap()).await;
        let last = chat_requests(&server).pop().unwrap();
        assert!(
            last.contains("the administrator did this through Kelpy:\\n- Put back"),
            "{last}"
        );
    }

    #[tokio::test]
    async fn a_change_made_outside_kelpy_is_found_shown_told_and_accepted() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = folder.path().join("site.conf");
        let server = model("Noted.").await;
        let write = json!({ "reason": "Site", "path": path, "content": "listen 80;\n" });
        server.respond_once(CHAT, "200 OK", calling("write_file", &write));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent
            .create_session(None, full(Approval::Auto))
            .await
            .unwrap();
        collect(agent.send(session.id, "Write it", ()).await.unwrap()).await;
        agent.check_server().await.unwrap();
        assert!(agent.differences().await.unwrap().is_empty());

        fs::write(&path, "listen 8080;\n").unwrap();
        agent.check_server().await.unwrap();
        let differences = agent.differences().await.unwrap();
        assert_eq!(differences.len(), 1);
        let diff = differences[0].diff.as_deref().unwrap();
        assert!(diff.contains("-listen 80;\n+listen 8080;"), "{diff}");
        assert!(differences[0].can_put_back);
        // Checked again: noted once.
        agent.check_server().await.unwrap();
        assert_eq!(agent.differences().await.unwrap()[0].id, differences[0].id);

        collect(agent.send(session.id, "Anything new?", ()).await.unwrap()).await;
        let last = chat_requests(&server).pop().unwrap();
        assert!(last.contains("Changes made outside Kelpy"), "{last}");
        assert!(last.contains("+listen 8080;"));

        // Put back as Kelpy wrote it, then changed again and accepted.
        let Operated::Done { action } = agent
            .put_back(session.id, differences[0].id, false)
            .await
            .unwrap()
        else {
            panic!("not done");
        };
        assert_eq!(action.state, ActionState::Done, "{:?}", action.error);
        assert_eq!(fs::read_to_string(&path).unwrap(), "listen 80;\n");
        assert!(agent.differences().await.unwrap().is_empty());
        fs::write(&path, "listen 81;\n").unwrap();
        agent.check_server().await.unwrap();
        let id = agent.differences().await.unwrap()[0].id;
        agent.accept_difference(id).await.unwrap();
        agent.check_server().await.unwrap();
        assert!(agent.differences().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_export_lists_the_operations_that_succeeded_with_their_arguments() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = folder.path().join("motd");
        let server = model("Done.").await;
        let write = json!({ "reason": "Greeting", "path": path, "content": "Hello\n" });
        server.respond_once(CHAT, "200 OK", calling("write_file", &write));
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent
            .create_session(None, full(Approval::Auto))
            .await
            .unwrap();
        collect(agent.send(session.id, "Greet", ()).await.unwrap()).await;
        let export = agent.export(session.id).await.unwrap();
        let operations = export["operations"].as_array().unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0]["tool"], "write_file");
        assert_eq!(operations[0]["arguments"]["content"], "Hello\n");
        assert_eq!(operations[0]["arguments"].get("reason"), None);
        assert!(export["note"].as_str().unwrap().contains("Not repeatable"));
    }

    #[tokio::test]
    async fn facts_are_remembered_and_kelpys_own_page_lists_its_changes() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = folder.path().join("site.conf");
        let server = model("Noted.").await;
        for queued in [
            calling(
                "write_file",
                &json!({ "reason": "Site", "path": path, "content": "listen 80;\n" }),
            ),
            calling(
                "remember",
                &json!({ "reason": "For next time", "page": "web", "note": "The site is served by nginx." }),
            ),
            calling(
                "read_memory",
                &json!({ "reason": "What changed?", "page": "server" }),
            ),
        ] {
            server.respond_once(CHAT, "200 OK", queued);
        }
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent
            .create_session(None, full(Approval::Auto))
            .await
            .unwrap();
        collect(agent.send(session.id, "Set it up", ()).await.unwrap()).await;
        let actions = agent.messages(session.id).await.unwrap()[1].actions.clone();
        assert!(
            actions
                .iter()
                .all(|action| action.state == ActionState::Done),
            "{actions:?}"
        );
        let memory = dir.path().join("memory");
        let web = fs::read_to_string(memory.join("web.md")).unwrap();
        assert!(
            web.starts_with("# Web\n\n- 20") && web.ends_with(": The site is served by nginx.\n"),
            "{web}"
        );
        assert!(
            fs::read_to_string(memory.join("MEMORY.md"))
                .unwrap()
                .contains("- [Web](web.md)")
        );
        // Kelpy's page lists the file it wrote, and the model read it.
        let page = fs::read_to_string(memory.join("server.md")).unwrap();
        assert!(
            page.contains("## Files") && page.contains("site.conf (since "),
            "{page}"
        );
        assert!(actions.last().unwrap().output.contains("## Files"));
        // The index goes to the model with each answer, with how to use the memory.
        let first = chat_requests(&server).remove(0);
        assert!(first.contains("read_memory") && first.contains("The user's memory index"));

        // Edited outside Kelpy: a difference, and the page is not written over it.
        fs::write(memory.join("server.md"), "# Mine\n").unwrap();
        agent.check_server().await.unwrap();
        let differences = agent.differences().await.unwrap();
        assert!(
            differences
                .iter()
                .any(|difference| difference.subject.ends_with("server.md")),
            "{differences:?}"
        );
        assert_eq!(
            fs::read_to_string(memory.join("server.md")).unwrap(),
            "# Mine\n"
        );
    }

    #[tokio::test]
    async fn a_file_kelpy_wrote_a_secret_into_is_kept_from_the_model() {
        let (dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let config = folder.path().join("app.yml");
        let server = model("Done.").await;
        for queued in [
            calling(
                "generate_secret",
                &json!({ "reason": "Password", "name": "app_db" }),
            ),
            calling(
                "write_file",
                &json!({ "reason": "Configuration", "path": config, "content": "password: {{secret:app_db}}\n" }),
            ),
            calling("read_file", &json!({ "reason": "Check", "path": config })),
        ] {
            server.respond_once(CHAT, "200 OK", queued);
        }
        let agent = agent(&dir, format!("{}/v1", server.url));
        let session = agent
            .create_session(None, full(Approval::Auto))
            .await
            .unwrap();
        collect(agent.send(session.id, "Configure", ()).await.unwrap()).await;
        let actions = agent.messages(session.id).await.unwrap()[1].actions.clone();
        let read = actions.last().unwrap();
        assert_eq!(read.state, ActionState::Failed, "{read:?}");
        assert!(read.error.as_deref().unwrap().contains("may hold secrets"));
        let written = fs::read_to_string(&config).unwrap();
        assert!(!written.contains("{{secret"));
        for request in chat_requests(&server) {
            assert!(!request.contains(written.trim_start_matches("password: ").trim()));
        }
    }
}
