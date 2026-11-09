use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::request::Builder;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use kelpy_core::home::Home;
use kelpy_core::providers::chatgpt::Endpoints;
use kelpy_core::storage::Store;
use kelpy_gateway::{Activity, Agent, Auth, RuntimeStatus, StopRequest, bind};
use kelpy_test_support::{FakeServer, chat_completions, id_token};
use tower::ServiceExt;

const PORT: u16 = 54321;
const HOST: &str = "127.0.0.1:54321";

/// An agent on a throwaway home, with an empty database and no provider.
fn agent() -> Arc<Agent> {
    agent_signing_in_at(Endpoints::default())
}

/// An agent as [`agent`] makes, signing in with ChatGPT at `endpoints`.
fn agent_signing_in_at(endpoints: Endpoints) -> Arc<Agent> {
    let dir = tempfile::tempdir().unwrap().keep();
    let home = Home::init(dir).unwrap();
    Arc::new(Agent::new(&home, Store::in_memory().unwrap(), None, endpoints).unwrap())
}

fn router(status: RuntimeStatus, port: u16, activity: Arc<Activity>, auth: Arc<Auth>) -> Router {
    kelpy_gateway::router(
        status,
        port,
        activity,
        auth,
        agent(),
        Arc::new(StopRequest::new()),
    )
}

fn status() -> RuntimeStatus {
    RuntimeStatus {
        version: "0.0.0-test".into(),
        pid: 42,
        home: PathBuf::from("/tmp/kelpy-home"),
        started_at: 0,
        account: "admin (not elevated)".into(),
        account_limits: None,
    }
}

fn auth() -> Arc<Auth> {
    Arc::new(Auth::new().unwrap())
}

fn authorized(request: Builder, auth: &Auth) -> Builder {
    request.header(header::AUTHORIZATION, format!("Bearer {}", auth.token()))
}

async fn send(method: Method, path: &str, host: Option<&str>, origin: Option<&str>) -> Response {
    let auth = auth();
    let mut request = authorized(Request::builder().method(method).uri(path), &auth);
    if let Some(host) = host {
        request = request.header(header::HOST, host);
    }
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    router(status(), PORT, Arc::new(Activity::new()), auth)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn accepted_requests_count_as_activity_but_health_checks_do_not() {
    async fn idle_after(path: &str, host: &str) -> Duration {
        let activity = Arc::new(Activity::new());
        let auth = auth();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let request = authorized(Request::get(path), &auth)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap();
        router(status(), PORT, Arc::clone(&activity), auth)
            .oneshot(request)
            .await
            .unwrap();
        activity.idle_for()
    }
    assert!(idle_after("/api/status", "127.0.0.1:54321").await < Duration::from_millis(40));
    assert!(idle_after("/api/health", "127.0.0.1:54321").await >= Duration::from_millis(70));
    // A page subscribes to the events again by itself: that is not someone using Kelpy.
    assert!(idle_after("/api/events", "127.0.0.1:54321").await >= Duration::from_millis(70));
    assert!(idle_after("/api/status", "evil.example").await >= Duration::from_millis(70));

    // Requests without credentials do not keep the runtime alive either.
    let activity = Arc::new(Activity::new());
    tokio::time::sleep(Duration::from_millis(80)).await;
    let anonymous = Request::get("/api/status")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    router(status(), PORT, Arc::clone(&activity), auth())
        .oneshot(anonymous)
        .await
        .unwrap();
    assert!(activity.idle_for() >= Duration::from_millis(70));
}

async fn get(path: &str, host: &str) -> Response {
    send(Method::GET, path, Some(host), None).await
}

#[tokio::test]
async fn loopback_hosts_of_this_port_are_accepted() {
    for host in [
        "127.0.0.1:54321",
        "localhost:54321",
        "LOCALHOST:54321",
        "[::1]:54321",
    ] {
        assert_eq!(
            get("/api/health", host).await.status(),
            StatusCode::OK,
            "{host}"
        );
    }
}

#[tokio::test]
async fn foreign_or_missing_hosts_are_rejected() {
    for host in [
        "evil.example",
        "evil.example:54321",
        "127.0.0.1:1234",
        "127.0.0.1",
    ] {
        assert_eq!(
            get("/api/health", host).await.status(),
            StatusCode::FORBIDDEN,
            "{host}"
        );
    }
    let response = send(Method::GET, "/api/health", None, None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cross_origin_state_changes_are_rejected() {
    let host = Some("127.0.0.1:54321");
    let foreign = send(
        Method::POST,
        "/api/health",
        host,
        Some("http://evil.example"),
    )
    .await;
    assert_eq!(foreign.status(), StatusCode::FORBIDDEN);

    // Same origin, or no origin at all (a local client), reaches the router: the route itself only allows GET.
    let same = send(
        Method::POST,
        "/api/health",
        host,
        Some("http://127.0.0.1:54321"),
    )
    .await;
    assert_eq!(same.status(), StatusCode::METHOD_NOT_ALLOWED);
    let local = send(Method::POST, "/api/health", host, None).await;
    assert_eq!(local.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn status_reports_the_runtime() {
    let response = get("/api/status", "127.0.0.1:54321").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["version"], "0.0.0-test");
    assert_eq!(json["pid"], 42);
}

#[tokio::test]
async fn pages_are_html_with_security_headers() {
    for path in ["/", "/some/client/route"] {
        let response = get(path, "127.0.0.1:54321").await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let headers = response.headers();
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(headers.contains_key(header::CONTENT_SECURITY_POLICY));
    }
}

#[tokio::test]
async fn unknown_api_paths_are_not_found() {
    let response = get("/api/nope", "127.0.0.1:54321").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn only_loopback_addresses_can_be_bound() {
    assert!(
        bind(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0, None)
            .await
            .is_err()
    );
    let listener = bind(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, None)
        .await
        .unwrap();
    assert_ne!(listener.local_addr().unwrap().port(), 0);
}

/// Sends `request` to `app` from this server's own origin.
async fn call(app: &Router, request: Builder, body: Body) -> Response {
    let request = request
        .header(header::HOST, HOST)
        .header(header::ORIGIN, format!("http://{HOST}"))
        .body(body)
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
}

async fn sign_in(app: &Router, code: &str) -> Response {
    let request = Request::post("/api/session").header(header::CONTENT_TYPE, "application/json");
    let body = Body::from(serde_json::json!({ "code": code }).to_string());
    call(app, request, body).await
}

fn app(auth: &Arc<Auth>) -> Router {
    router(status(), PORT, Arc::new(Activity::new()), Arc::clone(auth))
}

#[tokio::test]
async fn the_api_requires_credentials_but_pages_and_health_do_not() {
    let auth = auth();
    let app = app(&auth);

    for path in ["/api/status", "/api/nope", "/api"] {
        let response = call(&app, Request::get(path), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    let wrong = Request::get("/api/status").header(header::AUTHORIZATION, "Bearer 0123");
    let response = call(&app, wrong, Body::empty()).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let right = authorized(Request::get("/api/status"), &auth);
    assert_eq!(
        call(&app, right, Body::empty()).await.status(),
        StatusCode::OK
    );

    for path in ["/", "/api/health"] {
        let response = call(&app, Request::get(path), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn interfaces_receive_every_change_until_kelpy_stops() {
    let auth = auth();
    let agent = agent();
    let app = kelpy_gateway::router(
        status(),
        PORT,
        Arc::new(Activity::new()),
        Arc::clone(&auth),
        Arc::clone(&agent),
        Arc::new(StopRequest::new()),
    );
    let anonymous = call(&app, Request::get("/api/events"), Body::empty()).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let events = call(
        &app,
        authorized(Request::get("/api/events"), &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(events.status(), StatusCode::OK);
    assert!(
        events.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let created = call(
        &app,
        authorized(Request::post("/api/sessions"), &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    // Stopping ends the stream, so that it never holds up the shutdown.
    drop(agent.stop_all(Duration::from_secs(1)));

    let body = axum::body::to_bytes(events.into_body(), usize::MAX)
        .await
        .unwrap();
    let events = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        events.starts_with(
            "event: session_created\ndata: {\"type\":\"session_created\",\"session\":{\"id\":1,"
        ),
        "{events}"
    );
    assert!(
        events.ends_with("event: stopping\ndata: {\"type\":\"stopping\"}\n\n"),
        "{events}"
    );
}

#[tokio::test]
async fn a_stop_needs_credentials_and_says_where_it_came_from() {
    let auth = auth();
    let stop = Arc::new(StopRequest::new());
    let app = kelpy_gateway::router(
        status(),
        PORT,
        Arc::new(Activity::new()),
        Arc::clone(&auth),
        agent(),
        Arc::clone(&stop),
    );
    let anonymous = Request::post("/api/shutdown");
    assert_eq!(
        call(&app, anonymous, Body::empty()).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let signed = authorized(Request::post("/api/shutdown"), &auth);
    assert_eq!(
        call(&app, signed, Body::empty()).await.status(),
        StatusCode::ACCEPTED
    );
    let from = tokio::time::timeout(Duration::from_secs(1), stop.requested())
        .await
        .unwrap();
    assert_eq!(from, "`kelpy stop`");
}

#[tokio::test]
async fn the_access_code_opens_a_session_until_sign_out() {
    let auth = auth();
    let app = app(&auth);

    let response = sign_in(&app, "WRONG-CODE").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let session = session(&app, &auth).await;
    let status = || with_session(Request::get("/api/status"), &session);
    let response = call(&app, status(), Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);

    let sign_out = with_session(Request::delete("/api/session"), &session);
    let response = call(&app, sign_out, Body::empty()).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = call(&app, status(), Body::empty()).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn kelpy_proves_it_knows_a_session_without_receiving_it() {
    let auth = auth();
    let activity = Arc::new(Activity::new());
    let app = router(status(), PORT, Arc::clone(&activity), Arc::clone(&auth));
    let (id, session) = signed_in(&app, &auth).await;
    let prove = |id: &str, challenge: &str| {
        let body = serde_json::json!({ "id": id, "challenge": challenge }).to_string();
        let request =
            Request::post("/api/session/proof").header(header::CONTENT_TYPE, "application/json");
        call(&app, request, Body::from(body))
    };

    tokio::time::sleep(Duration::from_millis(80)).await;
    let response = prove(&id, "a challenge").await;
    assert_eq!(response.status(), StatusCode::OK);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, session.as_bytes());
    let expected: String = ring::hmac::sign(&key, b"kelpy-session-proof\na challenge")
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(json_body(response).await["proof"], expected.as_str());
    // The Web UI asks while it checks the connection: that keeps no Kelpy running.
    assert!(activity.idle_for() >= Duration::from_millis(70));

    assert_eq!(
        prove("unknown", "a challenge").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(prove(&id, "").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        prove(&id, &"x".repeat(257)).await.status(),
        StatusCode::BAD_REQUEST
    );

    let sign_out = with_session(Request::delete("/api/session"), &session);
    call(&app, sign_out, Body::empty()).await;
    assert_eq!(
        prove(&id, "a challenge").await.status(),
        StatusCode::NOT_FOUND
    );
}

/// Signs in with the access code; returns the session the browser keeps.
async fn session(app: &Router, auth: &Auth) -> String {
    signed_in(app, auth).await.1
}

/// Signs in with the access code; returns the public id of the session, and the session.
async fn signed_in(app: &Router, auth: &Auth) -> (String, String) {
    let response = sign_in(app, &auth.access_code()).await;
    assert_eq!(response.status(), StatusCode::OK);
    // Kept by the Web UI in the storage of its own origin, never as a cookie.
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    let body = json_body(response).await;
    let text = |field: &str| body[field].as_str().unwrap().to_owned();
    let (id, session) = (text("id"), text("session"));
    assert_ne!(id, session);
    (id, session)
}

fn with_session(request: Builder, session: &str) -> Builder {
    request.header(header::AUTHORIZATION, format!("Bearer {session}"))
}

#[tokio::test]
async fn a_session_is_never_taken_from_a_cookie() {
    // Browsers send a host's cookies to all its ports (RFC 6265 §8.5): any other local service could replay one.
    let auth = auth();
    let app = app(&auth);
    let session = session(&app, &auth).await;
    let cookie = Request::get("/api/status")
        .header(header::COOKIE, format!("kelpy_session_{PORT}={session}"));
    let response = call(&app, cookie, Body::empty()).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_stop_from_the_web_ui_is_reported_as_such() {
    let auth = auth();
    let stop = Arc::new(StopRequest::new());
    let app = kelpy_gateway::router(
        status(),
        PORT,
        Arc::new(Activity::new()),
        Arc::clone(&auth),
        agent(),
        Arc::clone(&stop),
    );
    let session = session(&app, &auth).await;
    let request = with_session(Request::post("/api/shutdown"), &session);
    assert_eq!(
        call(&app, request, Body::empty()).await.status(),
        StatusCode::ACCEPTED
    );
    let from = tokio::time::timeout(Duration::from_secs(1), stop.requested())
        .await
        .unwrap();
    assert_eq!(from, "the Web UI");
}

#[tokio::test]
async fn sign_in_from_another_origin_is_rejected() {
    let auth = auth();
    let request = Request::post("/api/session")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "http://evil.example")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "code": auth.access_code() }).to_string(),
        ))
        .unwrap();
    let response = app(&auth).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn repeated_wrong_codes_are_throttled() {
    let auth = auth();
    let app = app(&auth);
    for _ in 0..5 {
        let response = sign_in(&app, "WRONG-CODE").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = sign_in(&app, &auth.access_code()).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
}

async fn json_body(response: Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn json_request(method: Method, path: &str, auth: &Auth) -> Builder {
    authorized(Request::builder().method(method).uri(path), auth)
        .header(header::CONTENT_TYPE, "application/json")
}

#[tokio::test]
async fn conversations_carry_their_access_and_approval_mode() {
    let auth = auth();
    let app = app(&auth);
    let body = |json: serde_json::Value| Body::from(json.to_string());

    let created = call(
        &app,
        json_request(Method::POST, "/api/sessions", &auth),
        body(serde_json::json!({ "access": "none" })),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let session = json_body(created).await;
    assert_eq!(
        (session["access"].as_str(), session["approval"].as_str()),
        (Some("none"), Some("ask"))
    );

    let path = format!("/api/sessions/{}", session["id"]);
    let changed = call(
        &app,
        json_request(Method::PATCH, &path, &auth),
        body(serde_json::json!({ "access": "read_only", "approval": "plan" })),
    )
    .await;
    let session = json_body(changed).await;
    assert_eq!(
        (session["access"].as_str(), session["approval"].as_str()),
        (Some("read_only"), Some("plan"))
    );
    let wrong = call(
        &app,
        json_request(Method::PATCH, &path, &auth),
        body(serde_json::json!({ "access": "root" })),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // No answer is being written: nothing waits for a decision.
    let decided = call(
        &app,
        json_request(Method::POST, &format!("{path}/actions/7"), &auth),
        body(serde_json::json!({ "decision": "approve", "switch_to": "auto" })),
    )
    .await;
    assert_eq!(decided.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(decided).await["code"], "not_waiting");
    // Nor runs.
    let stopped = call(
        &app,
        json_request(Method::POST, &format!("{path}/actions/7/stop"), &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(stopped.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(stopped).await["code"], "not_running");
}

#[tokio::test]
async fn a_conversation_streams_its_answer_and_keeps_it() {
    let auth = auth();
    let app = kelpy_gateway::router(
        status(),
        PORT,
        Arc::new(Activity::new()),
        Arc::clone(&auth),
        agent(),
        Arc::new(StopRequest::new()),
    );

    let response = call(
        &app,
        json_request(Method::POST, "/api/sessions", &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = json_body(response).await["id"].as_i64().unwrap();
    let path = format!("/api/sessions/{id}/messages");
    let message = || Body::from(r#"{"content":"Install nginx"}"#);

    let response = call(&app, json_request(Method::POST, &path, &auth), message()).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(response).await["code"], "not_configured");

    let provider = serde_json::json!({
        "kind": "ollama", "model": "test", "base_url": chat_completions(&["Hel", "lo"], Duration::ZERO).await, "api_key": "not-returned"
    });
    let response = call(
        &app,
        json_request(Method::PUT, "/api/provider", &auth),
        Body::from(provider.to_string()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let info = json_body(response).await;
    assert_eq!(info["configured"]["model"], "test");
    assert_eq!(info["key"], "session");
    assert!(!info.to_string().contains("not-returned"));

    let response = call(&app, json_request(Method::POST, &path, &auth), message()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let events = String::from_utf8(body.to_vec()).unwrap();
    for name in [
        "event: user",
        "event: started",
        "event: delta",
        "event: done",
    ] {
        assert!(events.contains(name), "{name} missing in {events}");
    }

    let response = call(&app, json_request(Method::GET, &path, &auth), Body::empty()).await;
    let conversation = json_body(response).await;
    assert_eq!(conversation["running"], false);
    assert_eq!(conversation["messages"][1]["content"], "Hello");
    assert_eq!(conversation["messages"][1]["status"], "complete");
    // A short conversation reaches the model whole; what each request took stays inside Kelpy.
    assert!(conversation["summarised_through"].is_null());
    assert!(events.contains("\"summarised_through\":null"));
    assert!(conversation["messages"][1].get("input_tokens").is_none());
    // Only what the latest request took of the model's window, for the interfaces to show.
    let (used, window) = (
        &conversation["context"]["used"],
        &conversation["context"]["window"],
    );
    assert!(
        used.as_u64().unwrap() > 0 && window.as_u64().unwrap() > used.as_u64().unwrap(),
        "{conversation}"
    );
    assert!(events.contains("\"context\":{\"used\":"), "{events}");

    let response = call(
        &app,
        json_request(Method::GET, "/api/sessions", &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(json_body(response).await[0]["title"], "Install nginx");

    let delete = json_request(Method::DELETE, &format!("/api/sessions/{id}"), &auth);
    assert_eq!(
        call(&app, delete, Body::empty()).await.status(),
        StatusCode::NO_CONTENT
    );
    let response = call(&app, json_request(Method::GET, &path, &auth), Body::empty()).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_conversation_can_be_created_with_explanations() {
    let auth = auth();
    let app = kelpy_gateway::router(
        status(),
        PORT,
        Arc::new(Activity::new()),
        Arc::clone(&auth),
        agent(),
        Arc::new(StopRequest::new()),
    );

    let response = call(
        &app,
        json_request(Method::POST, "/api/sessions", &auth),
        Body::from(r#"{"explanations":true}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(json_body(response).await["explanations"], true);
    let response = call(
        &app,
        json_request(Method::GET, "/api/sessions", &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(json_body(response).await[0]["explanations"], true);
}

#[tokio::test]
async fn ollama_reports_its_models_and_streams_a_download_to_its_end() {
    let tags =
        serde_json::json!({ "models": [{ "name": "qwen3.5:9b", "size": 6_600_000_000_u64 }] });
    let pull = [
        serde_json::json!({ "status": "pulling abc", "total": 10, "completed": 5 }),
        serde_json::json!({ "status": "success" }),
    ]
    .map(|line| {
        format!(
            "{line}
"
        )
    })
    .concat();
    let ollama = FakeServer::start(vec![
        ("GET /api/tags", "200 OK", tags.to_string()),
        ("POST /api/pull", "200 OK", pull),
    ])
    .await;
    let auth = auth();
    let activity = Arc::new(Activity::new());
    let app = kelpy_gateway::router(
        status(),
        PORT,
        Arc::clone(&activity),
        Arc::clone(&auth),
        agent(),
        Arc::new(StopRequest::new()),
    );

    let path = format!("/api/ollama?base_url={}/v1", ollama.url);
    let response = call(&app, json_request(Method::GET, &path, &auth), Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let found = json_body(response).await;
    assert_eq!(found["running"], true);
    assert_eq!(found["models"][0]["name"], "qwen3.5:9b");

    let download = serde_json::json!({ "base_url": ollama.url, "model": "qwen3.5:4b" });
    let response = call(
        &app,
        json_request(Method::POST, "/api/ollama/download", &auth),
        Body::from(download.to_string()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&body)
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data.trim()).unwrap())
        .collect();
    let last = events.last().unwrap();
    assert_eq!(
        (last["done"].clone(), last["error"].clone()),
        (true.into(), serde_json::Value::Null)
    );
    assert_eq!(last["status"], "success");
    assert!(
        ollama
            .requests()
            .iter()
            .any(|request| request.contains("\"model\":\"qwen3.5:4b\""))
    );
}

#[tokio::test]
async fn a_code_sign_in_streams_the_code_then_the_account() {
    let tokens = serde_json::json!({
        "access_token": id_token(&serde_json::json!({ "exp": 4_000_000_000_i64 })),
        "refresh_token": "refresh-1", "expires_in": 3600,
        "id_token": id_token(&serde_json::json!({ "sub": "user-1", "email": "admin@example.com" })),
    });
    let openai = FakeServer::start(vec![
        (
            "POST /api/accounts/deviceauth/usercode",
            "200 OK",
            serde_json::json!({ "device_auth_id": "d", "user_code": "ABCD-1234", "interval": 1 })
                .to_string(),
        ),
        (
            "POST /api/accounts/deviceauth/token",
            "200 OK",
            serde_json::json!({ "authorization_code": "a", "code_verifier": "v" }).to_string(),
        ),
        ("POST /oauth/token", "200 OK", tokens.to_string()),
    ])
    .await;
    let endpoints = Endpoints {
        auth: openai.url.clone(),
        api: format!("{}/v1", openai.url),
        codex: format!("{}/codex", openai.url),
    };
    let auth = auth();
    let app = kelpy_gateway::router(
        status(),
        PORT,
        Arc::new(Activity::new()),
        Arc::clone(&auth),
        agent_signing_in_at(endpoints),
        Arc::new(StopRequest::new()),
    );
    let response = call(
        &app,
        json_request(Method::POST, "/api/chatgpt/code", &auth),
        Body::from("{}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&body)
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data.trim()).unwrap())
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["type"], "code");
    assert_eq!(events[0]["user_code"], "ABCD-1234");
    assert!(events[0]["qr"].as_str().unwrap().contains("<svg"));
    assert_eq!(events[1]["type"], "signed_in");
    assert_eq!(events[1]["account"]["email"], "admin@example.com");

    let status = call(
        &app,
        json_request(Method::GET, "/api/chatgpt", &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(json_body(status).await["method"], "codex");
}

#[tokio::test]
async fn an_agent_registration_file_can_be_used_but_not_made_before_registering() {
    let auth = auth();
    let app = app(&auth);
    let missing = call(
        &app,
        json_request(Method::GET, "/api/chatgpt/agent", &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::CONFLICT);

    let file = "[client]\nid = \"oaiapp_1\"\n\n[client.account]\nsub = \"user-1\"\nemail = \"admin@example.com\"\n";
    let used = call(
        &app,
        json_request(Method::PUT, "/api/chatgpt/agent", &auth),
        Body::from(serde_json::json!({ "text": file }).to_string()),
    )
    .await;
    assert_eq!(used.status(), StatusCode::OK);
    assert_eq!(json_body(used).await["registered"], "admin@example.com");
    let made = call(
        &app,
        json_request(Method::GET, "/api/chatgpt/agent", &auth),
        Body::empty(),
    )
    .await;
    assert!(
        json_body(made).await["text"]
            .as_str()
            .unwrap()
            .contains("oaiapp_1")
    );

    let invalid = call(
        &app,
        json_request(Method::PUT, "/api/chatgpt/agent", &auth),
        Body::from(serde_json::json!({ "text": "not toml [" }).to_string()),
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn secrets_are_added_kept_in_the_vault_and_shown_only_on_request() {
    let auth = auth();
    let app = router(status(), PORT, Arc::new(Activity::new()), Arc::clone(&auth));
    let send = |method: Method, path: &str, body: serde_json::Value| {
        let request = json_request(method, path, &auth);
        call(&app, request, Body::from(body.to_string()))
    };
    let view = json_body(send(Method::GET, "/api/secrets", serde_json::json!({})).await).await;
    assert_eq!(view["vault"]["state"], "none");

    let added = send(
        Method::POST,
        "/api/secrets",
        serde_json::json!({ "name": "smtp", "value": "smtp-password", "keep": false }),
    )
    .await;
    assert_eq!(added.status(), StatusCode::NO_CONTENT);
    // Keeping needs a vault.
    let keep = send(
        Method::POST,
        "/api/secrets/smtp/keep",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(keep.status(), StatusCode::CONFLICT);
    let created = send(
        Method::POST,
        "/api/vault",
        serde_json::json!({ "method": "passphrase", "passphrase": "correct horse battery" }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::NO_CONTENT);
    let keep = send(
        Method::POST,
        "/api/secrets/smtp/keep",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(keep.status(), StatusCode::NO_CONTENT);

    let view = send(Method::GET, "/api/secrets", serde_json::json!({})).await;
    let text = String::from_utf8(
        axum::body::to_bytes(view.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!text.contains("smtp-password"), "{text}");
    let view: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(view["vault"]["state"], "unlocked");
    assert_eq!(
        view["shared"],
        serde_json::json!([{ "name": "smtp", "kept": true }])
    );

    let value = send(
        Method::POST,
        "/api/secrets/smtp/value",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(value.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(json_body(value).await["value"], "smtp-password");

    send(Method::POST, "/api/vault/lock", serde_json::json!({})).await;
    let value = send(
        Method::POST,
        "/api/secrets/smtp/value",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(value.status(), StatusCode::NOT_FOUND);
    let wrong = send(
        Method::POST,
        "/api/vault/unlock",
        serde_json::json!({ "method": "passphrase", "passphrase": "wrong horse battery" }),
    )
    .await;
    // Not 401, which would end the Web UI's session.
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(wrong).await["code"], "wrong_passphrase");
}

#[tokio::test]
async fn files_are_uploaded_listed_downloaded_and_deleted() {
    let auth = auth();
    let app = router(status(), PORT, Arc::new(Activity::new()), Arc::clone(&auth));
    let upload = call(
        &app,
        authorized(Request::put("/api/files/site%20one.conf"), &auth),
        Body::from("server {}\n"),
    )
    .await;
    assert_eq!(upload.status(), StatusCode::CREATED);
    let file = json_body(upload).await;
    assert_eq!(file["name"], "site one.conf");
    assert_eq!(file["text"], true);

    let list = call(
        &app,
        authorized(Request::get("/api/files"), &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(json_body(list).await.as_array().unwrap().len(), 1);
    let download = call(
        &app,
        authorized(Request::get("/api/files/site%20one.conf"), &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(download.status(), StatusCode::OK);
    assert!(
        download.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    let content = axum::body::to_bytes(download.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&content[..], b"server {}\n");

    let deleted = call(
        &app,
        authorized(Request::delete("/api/files/site%20one.conf"), &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let missing = call(
        &app,
        authorized(Request::get("/api/files/..%2Fconfig.toml"), &auth),
        Body::empty(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
}
