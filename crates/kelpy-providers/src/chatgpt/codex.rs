//! Signing in as the Codex CLI: the device code sign-in of OpenAI's Codex CLI, whose answers come from ChatGPT's
//! Codex backend and count as the plan's Codex usage.
//!
//! No app is registered: Kelpy uses the Codex CLI's client. Kelpy asks for a short code, the user approves it on any
//! device at OpenAI's address, Kelpy asks until the approval comes, then exchanges the authorization it receives.
//! Requests to the backend name Kelpy and the ChatGPT account the tokens belong to.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    AUTH_CLAIMS, Account, Endpoints, Model, SignInError, Tokens, claims, ended, models_in,
    oauth_error, read_json, read_tokens,
};
use crate::{Provider, network_error};

/// The Codex CLI's OAuth client.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Names Kelpy to the backend: OpenAI asks tools other than its own to identify themselves.
const ORIGINATOR: &str = "kelpy";
/// The Codex CLI version the model list is asked for. The backend hides the models that need a newer client, and
/// answers `0.0.0` with an older, fixed list (observed by other tools, 2026-09).
const CLIENT_VERSION: &str = "99.0.0";
/// How long a code can be approved, as the Codex CLI waits for it.
pub const CODE_LIFETIME: Duration = Duration::from_secs(15 * 60);
/// The wait between two questions to OpenAI when it does not set one.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// A code for the user to approve.
pub struct DeviceCode {
    /// The page where the code is entered.
    pub verification_url: String,
    pub user_code: String,
    /// How often OpenAI may be asked whether the code was approved.
    pub interval: Duration,
    device_auth_id: String,
}

/// An approved code: what the exchange needs. Not `Debug`, so it cannot end up in a log.
pub struct Approval {
    authorization_code: String,
    code_verifier: String,
}

/// The sign-in's requests, with Kelpy's HTTP client.
pub struct Codex<'a> {
    client: &'a reqwest::Client,
    endpoints: &'a Endpoints,
}

impl Provider {
    pub fn codex<'a>(&'a self, endpoints: &'a Endpoints) -> Codex<'a> {
        Codex {
            client: &self.client,
            endpoints,
        }
    }
}

impl Codex<'_> {
    /// Asks OpenAI for a code to approve.
    pub async fn device_code(&self) -> Result<DeviceCode, SignInError> {
        #[derive(Deserialize)]
        struct Issued {
            device_auth_id: String,
            #[serde(alias = "usercode")]
            user_code: String,
            /// Seconds, as a number or as text.
            interval: Option<Value>,
        }

        let response = self
            .client
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                self.endpoints.auth
            ))
            .json(&json!({ "client_id": CLIENT_ID }))
            .send()
            .await
            .map_err(network_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(SignInError::NoDeviceCode);
        }
        if !response.status().is_success() {
            return Err(oauth_error(response).await);
        }
        let body = crate::body::read(response, crate::body::MAX_BODY).await?;
        let issued: Issued = serde_json::from_slice(&body)
            .map_err(|error| crate::ProviderError::Protocol(error.to_string()))?;
        let interval = issued
            .interval
            .and_then(|seconds| {
                seconds
                    .as_u64()
                    .or_else(|| seconds.as_str()?.trim().parse().ok())
            })
            .filter(|seconds| *seconds > 0)
            .map_or(DEFAULT_INTERVAL, Duration::from_secs);
        Ok(DeviceCode {
            verification_url: format!("{}/codex/device", self.endpoints.auth),
            user_code: issued.user_code,
            interval,
            device_auth_id: issued.device_auth_id,
        })
    }

    /// Asks once whether `code` was approved: `None` while it is not.
    pub async fn poll(&self, code: &DeviceCode) -> Result<Option<Approval>, SignInError> {
        #[derive(Deserialize)]
        struct Approved {
            authorization_code: String,
            code_verifier: String,
        }

        let response = self
            .client
            .post(format!(
                "{}/api/accounts/deviceauth/token",
                self.endpoints.auth
            ))
            .json(&json!({ "device_auth_id": code.device_auth_id, "user_code": code.user_code }))
            .send()
            .await
            .map_err(network_error)?;
        match response.status() {
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => {
                let body = crate::body::read(response, crate::body::MAX_BODY).await?;
                let approved: Approved = serde_json::from_slice(&body)
                    .map_err(|error| crate::ProviderError::Protocol(error.to_string()))?;
                Ok(Some(Approval {
                    authorization_code: approved.authorization_code,
                    code_verifier: approved.code_verifier,
                }))
            }
            _ => Err(oauth_error(response).await),
        }
    }

    /// Exchanges an approval for the session's tokens, at the Unix time `now`, and reads the account from its ID
    /// token.
    pub async fn exchange(
        &self,
        approval: &Approval,
        now: i64,
    ) -> Result<(Account, Tokens), SignInError> {
        let redirect_uri = format!("{}/deviceauth/callback", self.endpoints.auth);
        let response = self
            .client
            .post(self.token_url())
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", &approval.authorization_code),
                ("redirect_uri", &redirect_uri),
                ("code_verifier", &approval.code_verifier),
            ])
            .send()
            .await
            .map_err(network_error)?;
        let response = read_tokens(response).await?;
        // Read, not verified: it comes from OpenAI's token endpoint over TLS, in answer to this request (OpenID
        // Connect Core, 3.1.3.7).
        let id = response
            .id_token
            .as_deref()
            .and_then(claims)
            .ok_or(SignInError::InvalidIdToken("no ID token"))?;
        let sub = id["sub"]
            .as_str()
            .filter(|sub| !sub.is_empty())
            .ok_or(SignInError::InvalidIdToken("no subject"))?;
        let account = Account {
            sub: sub.to_owned(),
            email: id["email"].as_str().map(str::to_owned),
        };
        let refresh_token = response
            .refresh_token
            .clone()
            .ok_or(SignInError::Refused("no refresh token".into()))?;
        Ok((account, response.renewed(&refresh_token, now)))
    }

    /// New tokens for a session, at the Unix time `now`. The refresh token rotates: the old one must not be used
    /// again.
    pub async fn refresh(&self, refresh_token: &str, now: i64) -> Result<Tokens, SignInError> {
        let response = self
            .client
            .post(self.token_url())
            .json(&json!({
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": refresh_token,
            }))
            .send()
            .await
            .map_err(network_error)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(SignInError::Ended);
        }
        let response = read_tokens(response).await.map_err(ended)?;
        Ok(response.renewed(refresh_token, now))
    }

    /// Revokes a session's refresh token.
    pub async fn revoke(&self, refresh_token: &str) -> Result<(), SignInError> {
        let response = self
            .client
            .post(format!("{}/oauth/revoke", self.endpoints.auth))
            .json(&json!({
                "token": refresh_token,
                "token_type_hint": "refresh_token",
                "client_id": CLIENT_ID,
            }))
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
        let mut request = self
            .client
            .get(format!(
                "{}/models?client_version={CLIENT_VERSION}",
                self.endpoints.codex
            ))
            .bearer_auth(access_token);
        for (name, value) in headers(access_token) {
            request = request.header(name, value);
        }
        Ok(models_in(&read_json(request).await?))
    }

    fn token_url(&self) -> String {
        format!("{}/oauth/token", self.endpoints.auth)
    }
}

/// The headers of a request to the Codex backend besides the access token: Kelpy, and the ChatGPT account and data
/// residency the token names. Without the account, the backend answers for none.
pub fn headers(access_token: &str) -> Vec<(&'static str, String)> {
    let auth = claims(access_token)
        .map(|claims| claims[AUTH_CLAIMS].clone())
        .unwrap_or_default();
    let named = |claim: &str| auth[claim].as_str().filter(|value| !value.is_empty());
    let mut headers = vec![("originator", ORIGINATOR.to_owned())];
    if let Some(account) = named("chatgpt_account_id") {
        headers.push(("chatgpt-account-id", account.to_owned()));
    }
    if let Some(residency) =
        named("chatgpt_data_residency").or_else(|| named("chatgpt_compute_residency"))
    {
        headers.push(("x-openai-internal-codex-residency", residency.to_owned()));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_test_support::{FakeServer, id_token};

    const NOW: i64 = 1_793_000_000;
    const TOKEN: &str = "POST /oauth/token";

    fn access_token(account: &str) -> String {
        id_token(&json!({
            "exp": NOW + 3600,
            AUTH_CLAIMS: { "chatgpt_account_id": account, "chatgpt_data_residency": "eu" },
        }))
    }

    async fn openai(routes: Vec<(&'static str, &'static str, String)>) -> (FakeServer, Endpoints) {
        let server = FakeServer::start(routes).await;
        let endpoints = Endpoints {
            auth: server.url.clone(),
            api: format!("{}/v1", server.url),
            codex: format!("{}/codex", server.url),
        };
        (server, endpoints)
    }

    #[tokio::test]
    async fn a_code_is_issued_polled_and_exchanged_for_the_account() {
        let tokens = json!({
            "access_token": access_token("workspace-1"), "refresh_token": "refresh-1",
            "id_token": id_token(&json!({ "sub": "user-1", "email": "admin@example.com" })),
        });
        let (server, endpoints) = openai(vec![
            (
                "POST /api/accounts/deviceauth/usercode",
                "200 OK",
                json!({ "device_auth_id": "device-1", "user_code": "ABCD-1234", "interval": "3" })
                    .to_string(),
            ),
            (
                "POST /api/accounts/deviceauth/token",
                "403 Forbidden",
                String::new(),
            ),
            (TOKEN, "200 OK", tokens.to_string()),
        ])
        .await;
        let provider = Provider::new().unwrap();
        let codex = provider.codex(&endpoints);

        let code = codex.device_code().await.unwrap();
        assert_eq!(code.user_code, "ABCD-1234");
        assert_eq!(code.interval, Duration::from_secs(3));
        assert_eq!(
            code.verification_url,
            format!("{}/codex/device", server.url)
        );
        assert!(server.requests()[0].contains(CLIENT_ID));

        assert!(
            codex.poll(&code).await.unwrap().is_none(),
            "not approved yet"
        );
        server.respond(
            "POST /api/accounts/deviceauth/token",
            "200 OK",
            json!({ "authorization_code": "auth-code", "code_verifier": "verifier", "code_challenge": "c" })
                .to_string(),
        );
        let approval = codex.poll(&code).await.unwrap().unwrap();
        assert!(server.requests()[2].contains("\"device_auth_id\":\"device-1\""));

        let (account, tokens) = codex.exchange(&approval, NOW).await.unwrap();
        assert_eq!(account.sub, "user-1");
        assert_eq!(account.email.as_deref(), Some("admin@example.com"));
        assert_eq!(tokens.refresh_token, "refresh-1");
        assert_eq!(tokens.expires_in, 3600, "from the access token's expiry");
        let exchange = &server.requests()[3];
        for field in [
            "grant_type=authorization_code",
            "code=auth-code",
            "code_verifier=verifier",
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "deviceauth%2Fcallback",
        ] {
            assert!(exchange.contains(field), "{field} missing from {exchange}");
        }
    }

    #[tokio::test]
    async fn code_sign_in_turned_off_is_explained() {
        let (_server, endpoints) = openai(vec![]).await;
        let provider = Provider::new().unwrap();
        assert!(matches!(
            provider.codex(&endpoints).device_code().await,
            Err(SignInError::NoDeviceCode)
        ));
    }

    #[tokio::test]
    async fn sessions_are_renewed_revoked_and_recognised_as_ended() {
        let renewed = json!({ "access_token": access_token("workspace-1"), "expires_in": 600 });
        let (server, endpoints) = openai(vec![
            (TOKEN, "200 OK", renewed.to_string()),
            ("POST /oauth/revoke", "200 OK", String::new()),
        ])
        .await;
        let provider = Provider::new().unwrap();
        let codex = provider.codex(&endpoints);

        let tokens = codex.refresh("refresh-1", NOW).await.unwrap();
        assert_eq!(tokens.refresh_token, "refresh-1", "kept when none came");
        assert_eq!(tokens.expires_in, 600);
        assert!(server.requests()[0].contains("\"grant_type\":\"refresh_token\""));
        codex.revoke("refresh-1").await.unwrap();
        assert!(server.requests()[1].contains("\"token_type_hint\":\"refresh_token\""));

        server.respond(
            TOKEN,
            "400 Bad Request",
            json!({ "error": "refresh_token_reused" }).to_string(),
        );
        assert!(matches!(
            codex.refresh("refresh-1", NOW).await,
            Err(SignInError::Ended)
        ));
        server.respond(TOKEN, "401 Unauthorized", String::new());
        assert!(matches!(
            codex.refresh("refresh-1", NOW).await,
            Err(SignInError::Ended)
        ));
    }

    #[tokio::test]
    async fn requests_name_kelpy_and_the_account() {
        let token = access_token("workspace-1");
        let named = headers(&token);
        assert!(named.contains(&("originator", "kelpy".into())));
        assert!(named.contains(&("chatgpt-account-id", "workspace-1".into())));
        assert!(named.contains(&("x-openai-internal-codex-residency", "eu".into())));
        assert_eq!(headers("not a token"), [("originator", "kelpy".into())]);

        let (server, endpoints) = openai(vec![(
            "GET /codex/models",
            "200 OK",
            json!({ "models": [
                { "slug": "gpt-a", "display_name": "GPT A", "visibility": "list" },
                { "slug": "gpt-hidden", "display_name": "Hidden", "visibility": "hide" },
            ]})
            .to_string(),
        )])
        .await;
        let provider = Provider::new().unwrap();
        let models = provider.codex(&endpoints).models(&token).await.unwrap();
        assert_eq!(
            models,
            [Model {
                slug: "gpt-a".into(),
                display_name: "GPT A".into()
            }]
        );
        let request = server.requests()[0].to_ascii_lowercase();
        assert!(request.contains("client_version=99.0.0"));
        assert!(request.contains("chatgpt-account-id: workspace-1"));
        assert!(request.contains(&format!(
            "authorization: bearer {}",
            token.to_ascii_lowercase()
        )));
    }
}
