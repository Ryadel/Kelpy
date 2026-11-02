//! Protects the loopback server from other web pages open in the user's browser.
//!
//! - DNS rebinding: a hostile domain resolving to 127.0.0.1 still sends its own name in `Host`, so only the
//!   loopback names of this exact port are accepted.
//! - Cross-site requests: a state-changing request sent by a browser carries `Origin`, which must be this server.
//!   Local clients that send no `Origin`, such as the CLI, are accepted.
//! - Responses forbid framing and MIME sniffing, and restrict where the page may load resources from.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'";

pub struct AllowedHosts {
    hosts: Vec<String>,
}

impl AllowedHosts {
    pub fn for_port(port: u16) -> Self {
        Self {
            hosts: ["127.0.0.1", "localhost", "[::1]"]
                .iter()
                .map(|host| format!("{host}:{port}"))
                .collect(),
        }
    }

    fn allows_host(&self, host: &str) -> bool {
        self.hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
    }

    fn allows_origin(&self, origin: &str) -> bool {
        origin
            .strip_prefix("http://")
            .is_some_and(|host| self.allows_host(host))
    }
}

pub async fn guard(
    State(allowed): State<Arc<AllowedHosts>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            request
                .uri()
                .authority()
                .map(|authority| authority.as_str())
        });
    if !host.is_some_and(|host| allowed.allows_host(host)) {
        return (StatusCode::FORBIDDEN, "Forbidden: unexpected Host header").into_response();
    }

    let safe_method = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let origin_ok = request.headers().get(header::ORIGIN).is_none_or(|origin| {
        origin
            .to_str()
            .is_ok_and(|origin| allowed.allows_origin(origin))
    });
    if !safe_method && !origin_ok {
        return (StatusCode::FORBIDDEN, "Forbidden: cross-origin request").into_response();
    }

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    response
}
