//! Local API authentication: nothing under `/api/` answers without credentials, except the health check, the
//! sign-in itself and the identity proof.
//!
//! - Browsers sign in with the **access code** printed in the terminal, typed in or carried by the one-click link,
//!   and get a session valid until the runtime stops.
//! - Local clients (CLI, terminal UI) use the token in `runtime.json`, which only the owner can open: the operating
//!   system decides who the current user is.
//!
//! Both send `Authorization: Bearer <credential>`. Sessions are never cookies: browsers send a host's cookies to all
//! its ports (RFC 6265 §8.5), so any other local service could replay one. The Web UI keeps its session in the
//! storage of its own origin, which includes the port.
//!
//! Before it sends its session again, after the connection dropped or when a page opens, the Web UI asks the server
//! to prove it knows that session: an SSH tunnel may drop and another program take its local port, and the session
//! must not reach it. `POST /api/session/proof` answers an HMAC of the page's random challenge, keyed by the session
//! named by its public id; only Kelpy can compute it, and the session never travels for it.
//!
//! The secrets are random and new at every start. The code has 40 bits, so failed sign-ins are rate limited.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::post};
use kelpy_common::lock;
use ring::hmac;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

/// Crockford's base32: no I, L, O or U, so the code is easy to read aloud and to type.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LENGTH: usize = 8;
/// Failed sign-ins allowed in a row, then one more every `REFILL`: guessing a 40-bit code would take millennia.
const BURST: u32 = 5;
const REFILL: Duration = Duration::from_secs(10);
/// What the identity proof signs before the challenge, so that the HMAC of a session serves no other purpose.
const PROOF_CONTEXT: &str = "kelpy-session-proof\n";
/// The longest challenge accepted, in bytes: the Web UI sends 64 hexadecimal digits.
const MAX_CHALLENGE: usize = 256;

/// Who signed a request: added to the requests [`require`] lets through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    /// The CLI or the terminal UI, with the token.
    Local,
    /// The Web UI, with a session.
    Browser,
}

pub struct Auth {
    /// Without the separator, upper case.
    code: String,
    token: String,
    /// Browser sessions, by their public id.
    sessions: Mutex<HashMap<String, String>>,
    limiter: Mutex<Limiter>,
}

impl Auth {
    /// Creates fresh credentials.
    pub fn new() -> io::Result<Self> {
        let mut code = [0u8; CODE_LENGTH];
        fill_random(&mut code)?;
        Ok(Self {
            // 32 divides 256, so masking a random byte picks every symbol with the same probability.
            code: code
                .iter()
                .map(|byte| char::from(CODE_ALPHABET[usize::from(byte & 31)]))
                .collect(),
            token: random_hex()?,
            sessions: Mutex::new(HashMap::new()),
            limiter: Mutex::new(Limiter::new(Instant::now())),
        })
    }

    /// The access code for browsers, formatted for reading, such as `K7QM-4ZP2`.
    pub fn access_code(&self) -> String {
        let (first, second) = self.code.split_at(CODE_LENGTH / 2);
        format!("{first}-{second}")
    }

    /// The bearer token for local clients.
    pub fn token(&self) -> &str {
        &self.token
    }

    fn sign_in(&self, code: &str, now: Instant) -> Result<SignedIn, SignInError> {
        // Held across the check and the count, so parallel attempts cannot exceed the limit.
        let mut limiter = lock(&self.limiter);
        limiter.check(now).map_err(SignInError::Limited)?;
        if !bool::from(normalize(code).as_bytes().ct_eq(self.code.as_bytes())) {
            limiter.fail();
            return Err(SignInError::WrongCode);
        }
        let session = random_hex().map_err(|_| SignInError::Unavailable)?;
        let id = random_hex().map_err(|_| SignInError::Unavailable)?;
        lock(&self.sessions).insert(id.clone(), session.clone());
        Ok(SignedIn { id, session })
    }

    /// The proof that this server knows the session `id`: none when it has no such session.
    fn prove(&self, id: &str, challenge: &str) -> Option<String> {
        let session = lock(&self.sessions).get(id)?.clone();
        Some(proof(&session, challenge))
    }

    fn client(&self, headers: &HeaderMap) -> Option<Client> {
        let credential = bearer(headers)?;
        if bool::from(credential.as_bytes().ct_eq(self.token.as_bytes())) {
            return Some(Client::Local);
        }
        lock(&self.sessions)
            .values()
            .any(|session| bool::from(credential.as_bytes().ct_eq(session.as_bytes())))
            .then_some(Client::Browser)
    }
}

/// HMAC-SHA256 of the challenge, keyed by the session, in hexadecimal.
fn proof(session: &str, challenge: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, session.as_bytes());
    let tag = hmac::sign(&key, format!("{PROOF_CONTEXT}{challenge}").as_bytes());
    tag.as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

#[derive(Debug, PartialEq, Eq)]
enum SignInError {
    WrongCode,
    /// Too many failures: try again after this long.
    Limited(Duration),
    Unavailable,
}

/// A token bucket of failed sign-ins: `BURST` available, one more every `REFILL`.
#[derive(Debug)]
struct Limiter {
    available: u32,
    refilled_at: Instant,
}

impl Limiter {
    fn new(now: Instant) -> Self {
        Self {
            available: BURST,
            refilled_at: now,
        }
    }

    fn check(&mut self, now: Instant) -> Result<(), Duration> {
        while self.available < BURST && now.saturating_duration_since(self.refilled_at) >= REFILL {
            self.available += 1;
            self.refilled_at += REFILL;
        }
        if self.available == BURST {
            self.refilled_at = now;
        }
        if self.available > 0 {
            Ok(())
        } else {
            Err(REFILL.saturating_sub(now.saturating_duration_since(self.refilled_at)))
        }
    }

    fn fail(&mut self) {
        self.available = self.available.saturating_sub(1);
    }
}

/// Accepts the code as people type it: any case, with or without the dash or spaces, and O, I or L for 0 and 1.
fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    getrandom::fill(bytes).map_err(|error| io::Error::other(error.to_string()))
}

/// 256 random bits, as hexadecimal.
fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `POST /api/session` signs in with the access code; `DELETE /api/session` signs out; `POST /api/session/proof`
/// proves the server's identity to a page holding a session.
pub fn routes() -> Router<Arc<Auth>> {
    Router::new()
        .route("/api/session", post(sign_in).delete(sign_out))
        .route(PROOF_PATH, post(prove))
}

/// Public, like the health check, and not activity: the Web UI calls it while it checks the connection.
pub const PROOF_PATH: &str = "/api/session/proof";

#[derive(Deserialize)]
struct SignInRequest {
    code: String,
}

/// A browser session: `session` is the credential, `id` names it in identity proofs and may be shown to anyone.
#[derive(Serialize)]
struct SignedIn {
    id: String,
    session: String,
}

#[derive(Deserialize)]
struct ProofRequest {
    id: String,
    challenge: String,
}

#[derive(Serialize)]
struct ProofResponse {
    proof: String,
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

async fn sign_in(State(auth): State<Arc<Auth>>, Json(request): Json<SignInRequest>) -> Response {
    match auth.sign_in(&request.code, Instant::now()) {
        Ok(signed_in) => Json(signed_in).into_response(),
        Err(SignInError::WrongCode) => error(StatusCode::UNAUTHORIZED, "wrong access code"),
        Err(SignInError::Limited(wait)) => {
            let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            let mut response = error(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds.max(1)));
            response
        }
        Err(SignInError::Unavailable) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no randomness available, try again",
        ),
    }
}

async fn sign_out(State(auth): State<Arc<Auth>>, headers: HeaderMap) -> StatusCode {
    if let Some(credential) = bearer(&headers) {
        lock(&auth.sessions).retain(|_, session| session != credential);
    }
    StatusCode::NO_CONTENT
}

/// 404 for a session this server does not have: it ended, or Kelpy started again.
async fn prove(State(auth): State<Arc<Auth>>, Json(request): Json<ProofRequest>) -> Response {
    if request.challenge.is_empty() || request.challenge.len() > MAX_CHALLENGE {
        return error(
            StatusCode::BAD_REQUEST,
            "the challenge must have 1 to 256 bytes",
        );
    }
    match auth.prove(&request.id, &request.challenge) {
        Some(proof) => Json(ProofResponse { proof }).into_response(),
        None => error(StatusCode::NOT_FOUND, "no such session"),
    }
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(ErrorBody { error: message })).into_response()
}

/// Rejects `/api/` requests without valid credentials, and tells the others which [`Client`] signed them. The Web UI
/// files stay public: they hold no data and include the sign-in page.
pub async fn require(State(auth): State<Arc<Auth>>, mut request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let is_api = path == "/api" || path.starts_with("/api/");
    let is_public = !is_api
        || path == "/api/health"
        || path == PROOF_PATH
        || (path == "/api/session" && request.method() == Method::POST);
    let client = auth.client(request.headers());
    if let Some(client) = client {
        request.extensions_mut().insert(client);
    }
    if is_public || client.is_some() {
        return next.run(request).await;
    }
    error(StatusCode::UNAUTHORIZED, "sign in with the access code")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_readable_and_random() {
        let auth = Auth::new().unwrap();
        let code = auth.access_code();
        assert_eq!(code.len(), CODE_LENGTH + 1);
        assert_eq!(&code[4..5], "-");
        assert!(
            code.replace('-', "")
                .bytes()
                .all(|byte| CODE_ALPHABET.contains(&byte))
        );
        assert_eq!(auth.token().len(), 64);
        assert_ne!(Auth::new().unwrap().token(), auth.token());
    }

    #[test]
    fn the_proof_matches_the_web_ui() {
        // The same vector is in web/tests/session.test.ts.
        assert_eq!(
            proof("kelpy-test-session", "challenge"),
            "6079b53b72a45385ddc747fdf258937625f9027ec19c78439d1f57ca05846375"
        );
    }

    #[test]
    fn codes_are_accepted_as_people_type_them() {
        let auth = Auth::new().unwrap();
        auth.sign_in(&auth.access_code(), Instant::now()).unwrap();
        let typed = auth
            .access_code()
            .to_lowercase()
            .replace('0', "o")
            .replace('1', "l")
            .replace('-', " ");
        auth.sign_in(&typed, Instant::now()).unwrap();
    }

    #[test]
    fn failed_sign_ins_are_rate_limited() {
        let auth = Auth::new().unwrap();
        let start = Instant::now();
        for _ in 0..BURST {
            assert_eq!(
                auth.sign_in("wrong", start).err(),
                Some(SignInError::WrongCode)
            );
        }
        // Once the limit is reached even the right code waits, or guessing could simply go on.
        assert!(matches!(
            auth.sign_in(&auth.access_code(), start),
            Err(SignInError::Limited(wait)) if wait == REFILL
        ));
        let later = start + REFILL;
        assert_eq!(
            auth.sign_in("wrong", later).err(),
            Some(SignInError::WrongCode)
        );
        assert!(matches!(
            auth.sign_in("wrong", later),
            Err(SignInError::Limited(_))
        ));
        assert!(auth.sign_in(&auth.access_code(), later + REFILL).is_ok());
    }
}
