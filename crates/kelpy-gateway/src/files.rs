//! The files API: what the administrator uploads for Kelpy to use.
//!
//! | Route | |
//! |---|---|
//! | `GET /api/files` | the files, the most recent first |
//! | `PUT /api/files/{name}` | upload a file, streamed as the request's body; a name taken gets a number |
//! | `GET /api/files/{name}` | download it |
//! | `DELETE /api/files/{name}` | delete it |

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use kelpy_core::agent::Agent;
use kelpy_core::files::{Access, FilesError};
use tokio_util::io::ReaderStream;

use crate::chat::error;

pub fn routes() -> Router<Arc<Agent>> {
    Router::new().route("/api/files", get(list)).route(
        "/api/files/{name}",
        get(download)
            .put(upload)
            .delete(remove)
            // The upload stops at its own limit, as it is written.
            .layer(DefaultBodyLimit::disable()),
    )
}

async fn list(State(agent): State<Arc<Agent>>) -> Response {
    let files = Arc::clone(agent.files());
    match tokio::task::spawn_blocking(move || files.list()).await {
        Ok(Ok(files)) => Json(files).into_response(),
        Ok(Err(failure)) => files_failure(&failure),
        Err(failure) => files_failure(&FilesError::Io(std::io::Error::other(failure))),
    }
}

async fn upload(State(agent): State<Arc<Agent>>, Path(name): Path<String>, body: Body) -> Response {
    match agent.files().receive(&name, body.into_data_stream()).await {
        Ok(file) => (StatusCode::CREATED, Json(file)).into_response(),
        Err(failure) => files_failure(&failure),
    }
}

async fn download(State(agent): State<Arc<Agent>>, Path(name): Path<String>) -> Response {
    match agent.files().open(&name, Access::READ) {
        Ok(file) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::CONTENT_DISPOSITION, attachment(&name)),
                (header::CACHE_CONTROL, "no-store".to_owned()),
            ],
            Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file))),
        )
            .into_response(),
        Err(failure) => files_failure(&failure),
    }
}

async fn remove(State(agent): State<Arc<Agent>>, Path(name): Path<String>) -> Response {
    match agent.files().remove(&name) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => files_failure(&failure),
    }
}

/// `Content-Disposition` for a download named `name`, in UTF-8 as RFC 6266 allows.
fn attachment(name: &str) -> String {
    let encoded: String = name
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect();
    format!("attachment; filename*=UTF-8''{encoded}")
}

fn files_failure(failure: &FilesError) -> Response {
    let (status, code) = match failure {
        FilesError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
        FilesError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
        FilesError::InvalidName | FilesError::Interrupted(_) => {
            (StatusCode::BAD_REQUEST, "invalid")
        }
        FilesError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    error(status, code, failure.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_keeps_its_name() {
        assert_eq!(
            attachment("site (2).conf"),
            "attachment; filename*=UTF-8''site%20%282%29.conf"
        );
        assert_eq!(
            attachment("é.txt"),
            "attachment; filename*=UTF-8''%C3%A9.txt"
        );
    }
}
