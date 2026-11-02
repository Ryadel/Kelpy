//! Sign in with ChatGPT: the OAuth flows by which the user's ChatGPT plan pays for Kelpy's answers.
//!
//! This module is OpenAI's program for apps, where Kelpy signs in as an agent of the account: authorization code with
//! PKCE and a loopback redirect, no client secret. The first sign-in registers Kelpy for the account
//! (`dynamic_agent_client`) and OpenAI issues a client id, reused by later sign-ins. Answers then go through the
//! Responses API with the access token. [`codex`] is the other way, the Codex CLI's sign-in. These modules speak the
//! protocols; keeping the session is up to the caller.

pub mod codex;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{digest, signature};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Provider, ProviderError, network_error};

/// The fixed path of the redirect: only the port may change between sign-ins.
pub const CALLBACK_PATH: &str = "/auth/callback";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
/// Shown to the user by OpenAI during the first sign-in.
const AGENT_NAME: &str = "Kelpy";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The scope that lets the plan pay: a valid sign-in without it does not.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const ISSUER: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const CODEX_BACKEND: &str = "https://chatgpt.com/backend-api/codex";
/// Where OpenAI puts its own claims in its tokens.
const AUTH_CLAIMS: &str = "https://api.openai.com/auth";
/// Clock difference tolerated when checking that the ID token has not expired.
const CLOCK_SKEW: i64 = 60;
/// Refresh errors after which only a new sign-in helps.
const TERMINAL_REFRESH_ERRORS: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    #[error("the sign-in with ChatGPT was cancelled")]
    Denied,
    #[error("this sign-in is not the one Kelpy started: start it again")]
    UnknownAttempt,
    #[error("OpenAI answered for another client than the one registered: start the sign-in again")]
    ClientMismatch,
    #[error("the registration with OpenAI did not complete: start the sign-in again")]
    NotRegistered,
    #[error("this ChatGPT account did not allow its plan to be used by Kelpy")]
    NoPlanUse,
    #[error("the sign-in could not be verified: {0}")]
    InvalidIdToken(&'static str),
    #[error("the ChatGPT session has ended: sign in again")]
    Ended,
    #[error(
        "sign-in with a code is not available: enable it in ChatGPT's security settings, or ask the workspace admin"
    )]
    NoDeviceCode,
    #[error("OpenAI refused the request: {0}")]
    Refused(String),
    #[error("this system has no secure random numbers")]
    NoRandom,
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// Where the flow's requests go: OpenAI, or fakes in tests.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// The authorization server, such as `https://auth.openai.com`.
    pub auth: String,
    /// The API, such as `https://api.openai.com/v1`.
    pub api: String,
    /// ChatGPT's Codex backend, which answers sessions signed in as the Codex CLI.
    pub codex: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            auth: ISSUER.to_owned(),
            api: RESOURCE.to_owned(),
            codex: CODEX_BACKEND.to_owned(),
        }
    }
}

/// The verified identity of a ChatGPT account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// OpenAI's identifier of the account.
    pub sub: String,
    /// Helps the user recognise the account; not an identity.
    pub email: Option<String>,
}

/// What the browser brings back to the redirect address.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Callback {
    pub code: Option<String>,
    pub state: Option<String>,
    pub client_id: Option<String>,
    pub error: Option<String>,
}

/// The tokens of a session. Not `Debug`, so they cannot end up in a log.
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds the access token stays valid.
    pub expires_in: u64,
}

/// A model the account may use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
}

/// A host identifier for `ext_agent_host_id`: a random UUID, kept for as long as the Kelpy home exists.
pub fn new_host_id() -> Result<String, SignInError> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| SignInError::NoRandom)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// A sign-in under way: what must match when the browser comes back.
pub struct Attempt {
    state: String,
    nonce: String,
    verifier: String,
    redirect_uri: String,
    /// The issued client id sent, or `None` while registering.
    client_id: Option<String>,
}

/// A callback that matches its attempt: the code to exchange, and the client id to keep before exchanging it.
pub struct Accepted {
    pub client_id: String,
    code: String,
}

impl Attempt {
    /// Starts a sign-in redirected to `redirect_uri`. `returning` is the client id issued earlier, with the
    /// account's email as a hint; `None` registers Kelpy for the account. Returns the attempt and the address to
    /// open in the browser.
    pub fn start(
        endpoints: &Endpoints,
        redirect_uri: String,
        host_id: &str,
        returning: Option<(&str, Option<&str>)>,
    ) -> Result<(Self, Url), SignInError> {
        let attempt = Self {
            state: random_text()?,
            nonce: random_text()?,
            verifier: random_text()?,
            redirect_uri,
            client_id: returning.map(|(client_id, _)| client_id.to_owned()),
        };
        let challenge =
            URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, attempt.verifier.as_bytes()));
        let mut params = vec![
            ("response_type", "code"),
            (
                "client_id",
                attempt.client_id.as_deref().unwrap_or(DYNAMIC_CLIENT),
            ),
            ("ext_agent_host_id", host_id),
            ("redirect_uri", &attempt.redirect_uri),
            ("scope", SCOPE),
            ("resource", RESOURCE),
            ("state", &attempt.state),
            ("nonce", &attempt.nonce),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ];
        match returning {
            None => params.push(("agent_name_hint", AGENT_NAME)),
            Some((_, Some(email))) => params.push(("login_hint", email)),
            Some((_, None)) => {}
        }
        let url = Url::parse_with_params(
            &format!("{}/api/accounts/authorize", endpoints.auth),
            &params,
        )
        .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        Ok((attempt, url))
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    /// Checks the callback against this attempt.
    pub fn accept(&self, callback: &Callback) -> Result<Accepted, SignInError> {
        if callback.state.as_deref() != Some(self.state.as_str()) {
            return Err(SignInError::UnknownAttempt);
        }
        match callback.error.as_deref() {
            Some("access_denied") => return Err(SignInError::Denied),
            Some(error) => return Err(SignInError::Refused(error.to_owned())),
            None => {}
        }
        let client_id = match (&self.client_id, &callback.client_id) {
            (Some(sent), Some(returned)) if sent != returned => {
                return Err(SignInError::ClientMismatch);
            }
            (Some(sent), _) => sent.clone(),
            (None, Some(issued)) if issued != DYNAMIC_CLIENT => issued.clone(),
            (None, _) => return Err(SignInError::NotRegistered),
        };
        let code = callback.code.clone().ok_or(SignInError::Denied)?;
        Ok(Accepted { client_id, code })
    }
}

/// The token endpoint's answer.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    scope: Option<String>,
    expires_in: Option<u64>,
}

impl TokenResponse {
    /// Seconds the access token stays valid at the Unix time `now`: as the answer says, else until the token's own
    /// expiry; zero, so renewed before its first use, if neither tells.
    fn lifetime(&self, now: i64) -> u64 {
        self.expires_in
            .or_else(|| {
                let expiry = claims(&self.access_token)?["exp"].as_i64()?;
                u64::try_from(expiry - now).ok()
            })
            .unwrap_or(0)
    }

    /// The tokens of a session renewed with `refresh_token`, which stays in use unless a new one came.
    fn renewed(self, refresh_token: &str, now: i64) -> Tokens {
        Tokens {
            expires_in: self.lifetime(now),
            refresh_token: self
                .refresh_token
                .unwrap_or_else(|| refresh_token.to_owned()),
            access_token: self.access_token,
        }
    }
}

/// The flow's requests, with Kelpy's HTTP client.
pub struct ChatGpt<'a> {
    client: &'a reqwest::Client,
    endpoints: &'a Endpoints,
}

impl Provider {
    pub fn chatgpt<'a>(&'a self, endpoints: &'a Endpoints) -> ChatGpt<'a> {
        ChatGpt {
            client: &self.client,
            endpoints,
        }
    }
}

impl ChatGpt<'_> {
    /// Exchanges the code of an accepted callback, and verifies the account and its permission to use the plan.
    /// `now` is the Unix time, in seconds.
    pub async fn exchange(
        &self,
        attempt: &Attempt,
        accepted: &Accepted,
        now: i64,
    ) -> Result<(Account, Tokens), SignInError> {
        let response = self
            .token(&[
                ("grant_type", "authorization_code"),
                ("client_id", &accepted.client_id),
                ("code", &accepted.code),
                ("code_verifier", &attempt.verifier),
                ("redirect_uri", &attempt.redirect_uri),
                ("resource", RESOURCE),
            ])
            .await?;
        let granted = response.scope.as_deref().unwrap_or_default();
        if !granted.split(' ').any(|scope| scope == PLAN_SCOPE) {
            return Err(SignInError::NoPlanUse);
        }
        let id_token = response
            .id_token
            .as_deref()
            .ok_or(SignInError::InvalidIdToken("no ID token"))?;
        let jwks = self
            .get(
                &format!("{}/.well-known/jwks.json", self.endpoints.auth),
                None,
            )
            .await?;
        let account = verify_id_token(id_token, &jwks, &accepted.client_id, &attempt.nonce, now)?;
        let refresh_token = response
            .refresh_token
            .clone()
            .ok_or(SignInError::Refused("no refresh token".into()))?;
        Ok((account, response.renewed(&refresh_token, now)))
    }

    /// New tokens for a session, at the Unix time `now`. The refresh token rotates: the old one must not be used
    /// again.
    pub async fn refresh(
        &self,
        client_id: &str,
        refresh_token: &str,
        now: i64,
    ) -> Result<Tokens, SignInError> {
        let response = self
            .token(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("refresh_token", refresh_token),
                ("resource", RESOURCE),
            ])
            .await
            .map_err(ended)?;
        Ok(response.renewed(refresh_token, now))
    }

    /// Revokes a session's refresh token. An already invalid token counts as revoked.
    pub async fn revoke(&self, client_id: &str, refresh_token: &str) -> Result<(), SignInError> {
        let response = self
            .client
            .post(format!("{}/api/accounts/oauth/revoke", self.endpoints.auth))
            .form(&[
                ("token", refresh_token),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ])
            .send()
            .await
            .map_err(network_error)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(oauth_error(response).await)
        }
    }

    /// The models the account may choose, in OpenAI's order.
    pub async fn models(&self, access_token: &str) -> Result<Vec<Model>, SignInError> {
        let list = self
            .get(
                &format!("{}/models", self.endpoints.api),
                Some(access_token),
            )
            .await?;
        Ok(models_in(&list))
    }

    async fn token(&self, form: &[(&str, &str)]) -> Result<TokenResponse, SignInError> {
        let response = self
            .client
            .post(format!("{}/api/accounts/oauth/token", self.endpoints.auth))
            .form(form)
            .send()
            .await
            .map_err(network_error)?;
        read_tokens(response).await
    }

    async fn get(&self, url: &str, bearer: Option<&str>) -> Result<Value, SignInError> {
        let mut request = self.client.get(url);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        read_json(request).await
    }
}

/// Reads the answer of a token endpoint: the tokens, or the OAuth error.
async fn read_tokens(response: reqwest::Response) -> Result<TokenResponse, SignInError> {
    if !response.status().is_success() {
        return Err(oauth_error(response).await);
    }
    let body = crate::body::read(response, crate::body::MAX_BODY).await?;
    serde_json::from_slice(&body)
        .map_err(|error| ProviderError::Protocol(format!("invalid token response: {error}")).into())
}

/// Sends `request` and reads its JSON answer; an error answer becomes its message.
async fn read_json(request: reqwest::RequestBuilder) -> Result<Value, SignInError> {
    let response = request.send().await.map_err(network_error)?;
    if !response.status().is_success() {
        return Err(crate::api_error(response).await.into());
    }
    let body = crate::body::read(response, crate::body::MAX_BODY).await?;
    serde_json::from_slice(&body).map_err(|error| ProviderError::Protocol(error.to_string()).into())
}

/// The models of a model list offered for choice, in OpenAI's order.
fn models_in(list: &Value) -> Vec<Model> {
    list["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|model| model["visibility"] == "list")
        .filter_map(|model| {
            let slug = model["slug"].as_str()?.to_owned();
            let display_name = model["display_name"].as_str().unwrap_or(&slug).to_owned();
            Some(Model { slug, display_name })
        })
        .collect()
}

/// A renewal refused for good means that the session has ended.
fn ended(error: SignInError) -> SignInError {
    match error {
        SignInError::Refused(code) if TERMINAL_REFRESH_ERRORS.contains(&code.as_str()) => {
            SignInError::Ended
        }
        other => other,
    }
}

/// The claims of a JWT, unverified: only for tokens that came straight from OpenAI over TLS.
fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

/// An OAuth error answer: its `error` code, which callers match on.
async fn oauth_error(response: reqwest::Response) -> SignInError {
    let status = response.status().as_u16();
    let body = crate::body::error_text(response).await;
    match serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|json| json["error"].as_str().map(str::to_owned))
    {
        Some(code) => SignInError::Refused(code),
        None => ProviderError::Api {
            status: Some(status),
            message: crate::error_message(&body),
        }
        .into(),
    }
}

/// 32 random bytes as base64url: state, nonce and PKCE verifier.
fn random_text() -> Result<String, SignInError> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| SignInError::NoRandom)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Verifies an ID token signed with RS256 by a key of `jwks`, issued by OpenAI for `client_id` in the sign-in that
/// used `nonce`, and not expired at `now`.
fn verify_id_token(
    token: &str,
    jwks: &Value,
    client_id: &str,
    nonce: &str,
    now: i64,
) -> Result<Account, SignInError> {
    use SignInError::InvalidIdToken as Invalid;
    let (signed, signature_part) = token.rsplit_once('.').ok_or(Invalid("not a JWT"))?;
    let (header, claims) = signed.split_once('.').ok_or(Invalid("not a JWT"))?;
    let decode = |part: &str| {
        URL_SAFE_NO_PAD
            .decode(part)
            .map_err(|_| Invalid("bad encoding"))
    };
    let json = |part: &str| -> Result<Value, SignInError> {
        serde_json::from_slice(&decode(part)?).map_err(|_| Invalid("bad JSON"))
    };
    let header = json(header)?;
    if header["alg"] != "RS256" {
        return Err(Invalid("unexpected signature algorithm"));
    }
    let key = jwks["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|key| key["kty"] == "RSA" && key["kid"] == header["kid"])
        .ok_or(Invalid("unknown signing key"))?;
    let component = |name: &str| decode(key[name].as_str().unwrap_or_default());
    signature::RsaPublicKeyComponents {
        n: component("n")?,
        e: component("e")?,
    }
    .verify(
        &signature::RSA_PKCS1_2048_8192_SHA256,
        signed.as_bytes(),
        &decode(signature_part)?,
    )
    .map_err(|_| Invalid("bad signature"))?;

    let claims = json(claims)?;
    let audience_ok = match &claims["aud"] {
        Value::String(audience) => audience == client_id,
        Value::Array(audiences) => audiences.iter().any(|audience| audience == client_id),
        _ => false,
    };
    if claims["iss"] != ISSUER {
        return Err(Invalid("wrong issuer"));
    }
    if !audience_ok {
        return Err(Invalid("wrong audience"));
    }
    if claims["nonce"] != nonce {
        return Err(Invalid("wrong nonce"));
    }
    if claims["exp"]
        .as_i64()
        .is_none_or(|expiry| expiry + CLOCK_SKEW < now)
    {
        return Err(Invalid("expired"));
    }
    let sub = claims["sub"]
        .as_str()
        .filter(|sub| !sub.is_empty())
        .ok_or(Invalid("no subject"))?;
    Ok(Account {
        sub: sub.to_owned(),
        email: claims["email"].as_str().map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_test_support::{FakeServer, id_token, jwks};
    use serde_json::json;

    const NOW: i64 = 1_793_000_000;

    fn claims(nonce: &str) -> Value {
        json!({ "iss": ISSUER, "aud": "oaiapp_123", "sub": "user-1", "email": "admin@example.com", "nonce": nonce, "exp": NOW + 3600 })
    }

    fn query(url: &Url, name: &str) -> Option<String> {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    #[test]
    fn the_first_sign_in_registers_and_later_ones_reuse_the_client() {
        let endpoints = Endpoints::default();
        let redirect = "http://127.0.0.1:4000/auth/callback".to_owned();
        let (attempt, url) =
            Attempt::start(&endpoints, redirect.clone(), "urn:uuid:host", None).unwrap();
        assert!(
            url.as_str()
                .starts_with("https://auth.openai.com/api/accounts/authorize?")
        );
        assert_eq!(query(&url, "client_id").as_deref(), Some(DYNAMIC_CLIENT));
        assert_eq!(query(&url, "agent_name_hint").as_deref(), Some(AGENT_NAME));
        assert_eq!(
            query(&url, "ext_agent_host_id").as_deref(),
            Some("urn:uuid:host")
        );
        assert_eq!(query(&url, "redirect_uri"), Some(redirect.clone()));
        assert_eq!(query(&url, "scope").as_deref(), Some(SCOPE));
        assert_eq!(query(&url, "resource").as_deref(), Some(RESOURCE));
        assert_eq!(
            query(&url, "code_challenge_method").as_deref(),
            Some("S256")
        );
        let challenge =
            URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, attempt.verifier.as_bytes()));
        assert_eq!(query(&url, "code_challenge"), Some(challenge));
        assert_eq!(query(&url, "state").as_deref(), Some(attempt.state()));

        let (again, url) = Attempt::start(
            &endpoints,
            redirect,
            "urn:uuid:host",
            Some(("oaiapp_123", Some("admin@example.com"))),
        )
        .unwrap();
        assert_eq!(query(&url, "client_id").as_deref(), Some("oaiapp_123"));
        assert_eq!(
            query(&url, "login_hint").as_deref(),
            Some("admin@example.com")
        );
        assert_eq!(query(&url, "agent_name_hint"), None);
        assert_ne!(again.state, attempt.state);
        assert_ne!(again.verifier, attempt.verifier);
    }

    #[test]
    fn callbacks_must_match_their_attempt() {
        let endpoints = Endpoints::default();
        let start = |returning| {
            Attempt::start(
                &endpoints,
                "http://127.0.0.1:1/auth/callback".into(),
                "h",
                returning,
            )
            .unwrap()
            .0
        };
        let registering = start(None);
        let callback = |state: &str, client_id: Option<&str>, error: Option<&str>| Callback {
            code: Some("code".into()),
            state: Some(state.into()),
            client_id: client_id.map(Into::into),
            error: error.map(Into::into),
        };
        let state = registering.state().to_owned();
        assert!(matches!(
            registering.accept(&callback("forged", Some("oaiapp_1"), None)),
            Err(SignInError::UnknownAttempt)
        ));
        assert!(matches!(
            registering.accept(&callback(&state, None, Some("access_denied"))),
            Err(SignInError::Denied)
        ));
        assert!(matches!(
            registering.accept(&callback(&state, None, None)),
            Err(SignInError::NotRegistered)
        ));
        assert!(matches!(
            registering.accept(&callback(&state, Some(DYNAMIC_CLIENT), None)),
            Err(SignInError::NotRegistered)
        ));
        assert_eq!(
            registering
                .accept(&callback(&state, Some("oaiapp_1"), None))
                .unwrap()
                .client_id,
            "oaiapp_1"
        );

        let returning = start(Some(("oaiapp_1", None)));
        let state = returning.state().to_owned();
        assert!(matches!(
            returning.accept(&callback(&state, Some("oaiapp_2"), None)),
            Err(SignInError::ClientMismatch)
        ));
        assert_eq!(
            returning
                .accept(&callback(&state, None, None))
                .unwrap()
                .client_id,
            "oaiapp_1"
        );
    }

    #[test]
    fn id_tokens_are_verified() {
        let jwks = jwks();
        let verify = |token: &str| verify_id_token(token, &jwks, "oaiapp_123", "n0nce", NOW);
        let account = verify(&id_token(&claims("n0nce"))).unwrap();
        assert_eq!(account.sub, "user-1");
        assert_eq!(account.email.as_deref(), Some("admin@example.com"));

        let changed = |field: &str, value: Value| {
            let mut claims = claims("n0nce");
            claims[field] = value;
            verify(&id_token(&claims)).unwrap_err().to_string()
        };
        assert!(changed("nonce", json!("other")).contains("wrong nonce"));
        assert!(changed("aud", json!("oaiapp_999")).contains("wrong audience"));
        assert!(changed("iss", json!("https://evil.example")).contains("wrong issuer"));
        assert!(changed("exp", json!(NOW - 3600)).contains("expired"));
        let mut listed = claims("n0nce");
        listed["aud"] = json!(["another", "oaiapp_123"]);
        assert!(verify(&id_token(&listed)).is_ok());

        // Claims changed after signing: another account, same signature.
        let token = id_token(&claims("n0nce"));
        let parts: Vec<&str> = token.split('.').collect();
        let mut other = claims("n0nce");
        other["sub"] = json!("user-2");
        let forged = format!(
            "{}.{}.{}",
            parts[0],
            URL_SAFE_NO_PAD.encode(other.to_string()),
            parts[2]
        );
        assert!(
            verify(&forged)
                .unwrap_err()
                .to_string()
                .contains("bad signature")
        );
    }

    #[tokio::test]
    async fn a_sign_in_is_exchanged_verified_refreshed_and_revoked() {
        let endpoints_for = |server: &FakeServer| Endpoints {
            auth: server.url.clone(),
            api: format!("{}/v1", server.url),
            codex: format!("{}/codex", server.url),
        };
        let provider = Provider::new().unwrap();
        let start = |endpoints: &Endpoints| {
            Attempt::start(
                endpoints,
                "http://127.0.0.1:1/auth/callback".into(),
                "h",
                None,
            )
            .unwrap()
            .0
        };
        // Learn the nonce of an attempt before the server's answer is fixed.
        let placeholder = Endpoints::default();
        let attempt = start(&placeholder);
        let tokens = json!({
            "access_token": "access-1", "refresh_token": "refresh-1", "token_type": "Bearer",
            "expires_in": 3600, "scope": SCOPE, "id_token": id_token(&{
                let mut claims = claims(&attempt.nonce);
                claims["aud"] = json!("oaiapp_1");
                claims
            }),
        });
        let server = FakeServer::start(vec![
            (
                "POST /api/accounts/oauth/token",
                "200 OK",
                tokens.to_string(),
            ),
            ("GET /.well-known/jwks.json", "200 OK", jwks().to_string()),
            ("POST /api/accounts/oauth/revoke", "200 OK", String::new()),
            (
                "GET /v1/models",
                "200 OK",
                json!({ "models": [
                    { "slug": "gpt-a", "display_name": "GPT A", "visibility": "list" },
                    { "slug": "gpt-hidden", "display_name": "Hidden", "visibility": "hide" },
                ]})
                .to_string(),
            ),
        ])
        .await;
        let endpoints = endpoints_for(&server);
        let chatgpt = provider.chatgpt(&endpoints);
        let accepted = attempt
            .accept(&Callback {
                code: Some("the-code".into()),
                state: Some(attempt.state().into()),
                client_id: Some("oaiapp_1".into()),
                error: None,
            })
            .unwrap();
        let (account, session) = chatgpt.exchange(&attempt, &accepted, NOW).await.unwrap();
        assert_eq!(account.sub, "user-1");
        assert_eq!(session.access_token, "access-1");
        let exchange = &server.requests()[0];
        for field in [
            "grant_type=authorization_code",
            "client_id=oaiapp_1",
            "code=the-code",
            "code_verifier=",
        ] {
            assert!(exchange.contains(field), "{field} missing from {exchange}");
        }

        let refreshed = chatgpt.refresh("oaiapp_1", "refresh-1", NOW).await.unwrap();
        assert_eq!(refreshed.refresh_token, "refresh-1");
        assert!(server.requests()[2].contains("grant_type=refresh_token"));
        chatgpt.revoke("oaiapp_1", "refresh-1").await.unwrap();
        assert!(server.requests()[3].contains("token_type_hint=refresh_token"));
        let models = chatgpt.models("access-1").await.unwrap();
        assert_eq!(
            models,
            [Model {
                slug: "gpt-a".into(),
                display_name: "GPT A".into()
            }]
        );
        assert!(
            server.requests()[4]
                .to_ascii_lowercase()
                .contains("authorization: bearer access-1")
        );
    }

    #[tokio::test]
    async fn plan_use_must_be_granted_and_ended_sessions_are_recognised() {
        let provider = Provider::new().unwrap();
        let no_plan = json!({ "access_token": "a", "refresh_token": "r", "expires_in": 3600, "scope": "openid email" });
        let server = FakeServer::start(vec![(
            "POST /api/accounts/oauth/token",
            "200 OK",
            no_plan.to_string(),
        )])
        .await;
        let endpoints = Endpoints {
            auth: server.url.clone(),
            api: server.url.clone(),
            codex: server.url.clone(),
        };
        let (attempt, _) = Attempt::start(
            &endpoints,
            "http://127.0.0.1:1/auth/callback".into(),
            "h",
            Some(("oaiapp_1", None)),
        )
        .unwrap();
        let accepted = attempt
            .accept(&Callback {
                code: Some("c".into()),
                state: Some(attempt.state().into()),
                ..Callback::default()
            })
            .unwrap();
        assert!(matches!(
            provider
                .chatgpt(&endpoints)
                .exchange(&attempt, &accepted, NOW)
                .await,
            Err(SignInError::NoPlanUse)
        ));

        let server = FakeServer::start(vec![(
            "POST /api/accounts/oauth/token",
            "400 Bad Request",
            json!({ "error": "refresh_token_reused" }).to_string(),
        )])
        .await;
        let endpoints = Endpoints {
            auth: server.url.clone(),
            api: server.url.clone(),
            codex: server.url.clone(),
        };
        assert!(matches!(
            provider
                .chatgpt(&endpoints)
                .refresh("oaiapp_1", "old", NOW)
                .await,
            Err(SignInError::Ended)
        ));
    }
}
