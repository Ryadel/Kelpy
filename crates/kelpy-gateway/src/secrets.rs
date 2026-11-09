//! The secrets API: the administrator's secrets, those the conversations generated, and the vault.
//!
//! | Route | |
//! |---|---|
//! | `GET /api/secrets` | every secret by name, the vault's state and the SSH agent's keys; never a value |
//! | `POST /api/secrets` | add a secret: `name`, `value`, `keep` in the vault |
//! | `DELETE /api/secrets/{name}` | remove one of the administrator's secrets, from the vault too |
//! | `POST /api/secrets/{name}/value` | a value, for the administrator: theirs, or a conversation's with `session_id` |
//! | `POST /api/secrets/{name}/keep` | keep a secret in the vault: theirs, or a conversation's with `session_id` |
//! | `POST /api/vault`, `DELETE /api/vault` | create the vault with a first way to unlock it; delete it |
//! | `POST /api/vault/unlock`, `POST /api/vault/lock` | unlock with a passphrase or an SSH key; lock |
//! | `POST /api/vault/unlocks`, `DELETE /api/vault/unlocks/{index}` | add or remove a way to unlock it |

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use kelpy_core::agent::{Agent, AgentError};
use kelpy_core::ssh_agent::AgentError as SshAgentError;
use kelpy_core::vault::{Unlock, VaultError};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::chat::error;

pub fn routes() -> Router<Arc<Agent>> {
    Router::new()
        .route("/api/secrets", get(secrets).post(add))
        .route("/api/secrets/{name}", delete(remove))
        .route("/api/secrets/{name}/value", post(value))
        .route("/api/secrets/{name}/keep", post(keep))
        .route("/api/vault", post(create).delete(delete_vault))
        .route("/api/vault/unlock", post(unlock))
        .route("/api/vault/lock", post(lock))
        .route("/api/vault/unlocks", post(add_unlock))
        .route("/api/vault/unlocks/{index}", delete(remove_unlock))
}

#[derive(Deserialize)]
struct NewSecret {
    name: String,
    value: Zeroizing<String>,
    #[serde(default)]
    keep: bool,
}

/// Which secret: one of the administrator's, or one a conversation generated.
#[derive(Deserialize, Default)]
struct Whose {
    #[serde(default)]
    session_id: Option<i64>,
}

#[derive(Serialize)]
struct Value {
    value: String,
}

/// How to open the vault, as the interfaces send it.
#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
enum UnlockRequest {
    Passphrase {
        passphrase: Zeroizing<String>,
    },
    /// The agent's key with this fingerprint; to unlock, none tries every key.
    SshKey {
        #[serde(default)]
        fingerprint: Option<String>,
    },
}

impl From<UnlockRequest> for Unlock {
    fn from(request: UnlockRequest) -> Self {
        match request {
            UnlockRequest::Passphrase { passphrase } => Unlock::Passphrase(passphrase),
            UnlockRequest::SshKey { fingerprint } => Unlock::SshKey(fingerprint),
        }
    }
}

async fn secrets(State(agent): State<Arc<Agent>>) -> Response {
    Json(agent.secrets().await).into_response()
}

async fn add(State(agent): State<Arc<Agent>>, Json(secret): Json<NewSecret>) -> Response {
    done(
        agent
            .vault()
            .add(&secret.name, secret.value, secret.keep)
            .await,
    )
}

async fn remove(State(agent): State<Arc<Agent>>, Path(name): Path<String>) -> Response {
    done(agent.vault().remove(&name).await)
}

async fn value(
    State(agent): State<Arc<Agent>>,
    Path(name): Path<String>,
    Json(whose): Json<Whose>,
) -> Response {
    match agent.secret_value(whose.session_id, &name) {
        // Never kept by the browser or anything between.
        Some(value) => {
            ([(header::CACHE_CONTROL, "no-store")], Json(Value { value })).into_response()
        }
        None => error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("there is no secret named `{name}`"),
        ),
    }
}

async fn keep(
    State(agent): State<Arc<Agent>>,
    Path(name): Path<String>,
    Json(whose): Json<Whose>,
) -> Response {
    match whose.session_id {
        Some(id) => match agent.keep_generated_secret(id, &name).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(AgentError::Vault(failure)) => vault_failure(&failure),
            Err(failure) => error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                failure.to_string(),
            ),
        },
        None => done(agent.vault().keep(&name).await),
    }
}

async fn create(State(agent): State<Arc<Agent>>, Json(unlock): Json<UnlockRequest>) -> Response {
    done(agent.vault().create(unlock.into()).await)
}

async fn unlock(State(agent): State<Arc<Agent>>, Json(unlock): Json<UnlockRequest>) -> Response {
    done(agent.vault().unlock(unlock.into()).await)
}

async fn lock(State(agent): State<Arc<Agent>>) -> Response {
    agent.vault().lock();
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_vault(State(agent): State<Arc<Agent>>) -> Response {
    done(agent.vault().delete().await)
}

async fn add_unlock(
    State(agent): State<Arc<Agent>>,
    Json(unlock): Json<UnlockRequest>,
) -> Response {
    done(agent.vault().add_unlock(unlock.into()).await)
}

async fn remove_unlock(State(agent): State<Arc<Agent>>, Path(index): Path<usize>) -> Response {
    done(agent.vault().remove_unlock(index).await)
}

fn done(result: Result<(), VaultError>) -> Response {
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => vault_failure(&failure),
    }
}

fn vault_failure(failure: &VaultError) -> Response {
    let (status, code) = match failure {
        VaultError::Missing | VaultError::Exists | VaultError::Locked | VaultError::Cancelled => {
            (StatusCode::CONFLICT, "vault_state")
        }
        VaultError::Stopping => (StatusCode::SERVICE_UNAVAILABLE, "stopping"),
        VaultError::Unreadable(_) | VaultError::Damaged(_) => {
            (StatusCode::CONFLICT, "vault_damaged")
        }
        // Not 401: that would mean the Web UI's own session ended.
        VaultError::WrongPassphrase => (StatusCode::BAD_REQUEST, "wrong_passphrase"),
        VaultError::NoAgentKey
        | VaultError::UnknownKey(_)
        | VaultError::Unsuitable(_)
        | VaultError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid"),
        VaultError::Agent(SshAgentError::Missing) => (StatusCode::CONFLICT, "no_agent"),
        VaultError::Agent(_) => (StatusCode::BAD_GATEWAY, "agent"),
        VaultError::Write(_) | VaultError::Random(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    };
    error(status, code, failure.to_string())
}
