//! Kelpy test support: fake HTTP servers shared by the tests of several crates. Used only as a dev-dependency, so
//! none of it reaches the executable.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{self, RsaKeyPair, RsaPublicKeyComponents};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// Reads a whole HTTP request and returns it, so that closing the socket afterwards does not reset the connection
/// and discard the response still in flight.
pub async fn read_request(socket: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = socket.read(&mut buffer).await.unwrap_or(0);
        if read == 0 {
            return String::from_utf8_lossy(&request).into_owned();
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                return String::from_utf8_lossy(&request).into_owned();
            }
        }
    }
}

/// Serves one HTTP response with `status` and `body` as an event stream, and returns the API address.
pub async fn serve_once(status: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    format!("http://{address}/v1")
}

/// An OpenAI-compatible server that answers every request with `pieces` as a streamed chat completion, pausing
/// `pause` after each one. Returns the API address.
pub async fn chat_completions(pieces: &'static [&'static str], pause: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                read_request(&mut socket).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                for piece in pieces {
                    let chunk = format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{piece}\"}}}}]}}\n\n"
                    );
                    // The client may hang up mid-answer, which is what some tests do on purpose.
                    if socket.write_all(chunk.as_bytes()).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(pause).await;
                }
                let end = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                let _ = socket.write_all(end.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/v1")
}

type Routes = Vec<(&'static str, &'static str, String)>;

/// A JSON server answering each request by its method and path (`"POST /oauth/token"`), and keeping the requests.
pub struct FakeServer {
    /// The server's address, such as `http://127.0.0.1:1234`.
    pub url: String,
    shared: Arc<Shared>,
}

struct Shared {
    routes: Mutex<Routes>,
    /// Answers given once each, before the route's own, in order.
    once: Mutex<Routes>,
    requests: Mutex<Vec<String>>,
    /// Routes whose answers wait until their gate opens.
    held: Mutex<Vec<(&'static str, watch::Receiver<bool>)>>,
}

/// Holds the answers of a route until it is opened, or dropped.
pub struct Gate(watch::Sender<bool>);

impl Gate {
    pub fn open(&self) {
        self.0.send_replace(true);
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.open();
    }
}

impl FakeServer {
    /// Serves `routes`: method and path, status line, body. Any other request gets `404 Not Found`.
    pub async fn start(routes: Routes) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shared = Arc::new(Shared {
            routes: Mutex::new(routes),
            once: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
        });
        let served = Arc::clone(&shared);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                // One task per connection, so a held answer does not hold the others.
                tokio::spawn(Arc::clone(&served).answer(socket));
            }
        });
        Self { url, shared }
    }

    /// Answers `route` from now on with `status` and `body`, instead of what it answered before.
    pub fn respond(&self, route: &'static str, status: &'static str, body: String) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.retain(|(known, _, _)| *known != route);
        routes.push((route, status, body));
    }

    /// Answers the next request to `route` with `status` and `body`, once; later ones get what they got before.
    pub fn respond_once(&self, route: &'static str, status: &'static str, body: String) {
        self.shared.once.lock().unwrap().push((route, status, body));
    }

    /// Holds the answers of `route` until the returned gate opens; the requests are kept as they arrive.
    pub fn hold(&self, route: &'static str) -> Gate {
        let (gate, opened) = watch::channel(false);
        self.shared.held.lock().unwrap().push((route, opened));
        Gate(gate)
    }

    /// The requests received so far, whole.
    pub fn requests(&self) -> Vec<String> {
        self.shared.requests.lock().unwrap().clone()
    }

    /// Waits until a request containing `text` has arrived.
    pub async fn received(&self, text: &str) {
        let arrived = async {
            while !self.requests().iter().any(|request| request.contains(text)) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap_or_else(|_| panic!("no request containing {text:?} arrived"));
    }
}

impl Shared {
    async fn answer(self: Arc<Self>, mut socket: TcpStream) {
        let request = read_request(&mut socket).await;
        let line = request.lines().next().unwrap_or_default();
        let target = line.rsplit_once(' ').map_or(line, |(start, _)| start);
        let target = target
            .split_once('?')
            .map_or(target, |(path, _)| path)
            .to_owned();
        self.requests.lock().unwrap().push(request);
        let held = self
            .held
            .lock()
            .unwrap()
            .iter()
            .find(|(route, _)| *route == target)
            .map(|(_, opened)| opened.clone());
        if let Some(mut opened) = held {
            // An error means the gate was dropped, which opens it too.
            let _ = opened.wait_for(|open| *open).await;
        }
        let once = {
            let mut once = self.once.lock().unwrap();
            let next = once.iter().position(|(route, _, _)| *route == target);
            next.map(|index| once.remove(index))
        };
        let (status, body) = match once {
            Some((_, status, body)) => (status, body),
            None => self
                .routes
                .lock()
                .unwrap()
                .iter()
                .find(|(route, _, _)| *route == target)
                .map_or(("404 Not Found", String::new()), |(_, status, body)| {
                    (*status, body.clone())
                }),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        // The client may already have given up on this request.
        let _ = socket.write_all(response.as_bytes()).await;
    }
}

/// The issuer of OpenAI's ID tokens.
pub const OPENAI_ISSUER: &str = "https://auth.openai.com";
/// A key made for these tests only, never used anywhere else.
const TEST_KEY: &[u8] = include_bytes!("../testdata/id-token-test-key.pk8");

/// The key set publishing the test key, as OpenAI's `jwks.json` publishes its keys.
pub fn jwks() -> Value {
    let key = RsaKeyPair::from_pkcs8(TEST_KEY).unwrap();
    let components: RsaPublicKeyComponents<Vec<u8>> = key.public().into();
    json!({ "keys": [{
        "kty": "RSA", "kid": "test", "alg": "RS256",
        "n": URL_SAFE_NO_PAD.encode(&components.n), "e": URL_SAFE_NO_PAD.encode(&components.e),
    }]})
}

/// An ID token with `claims`, signed with the test key.
pub fn id_token(claims: &Value) -> String {
    let encode = |value: &Value| URL_SAFE_NO_PAD.encode(value.to_string());
    let signed = format!(
        "{}.{}",
        encode(&json!({ "alg": "RS256", "kid": "test", "typ": "JWT" })),
        encode(claims)
    );
    let key = RsaKeyPair::from_pkcs8(TEST_KEY).unwrap();
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &signature::RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signed.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
}
