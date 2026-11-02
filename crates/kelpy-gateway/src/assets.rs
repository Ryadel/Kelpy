//! Web UI files embedded at build time (see `build.rs`), served as a single-page app.

use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

const NOT_BUILT: &str = "<!doctype html>
<html lang=\"en\">
<head><meta charset=\"utf-8\"><title>Kelpy</title></head>
<body>
<h1>Kelpy</h1>
<p>The runtime is running, but this executable was built without the Web UI.</p>
<p>Run <code>bun install</code> and <code>bun run build</code> in <code>web/</code>, then build Kelpy again.</p>
</body>
</html>
";

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path == "api" || path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(bytes) = find(path) {
        return file(path, bytes);
    }
    // Any other path belongs to the single-page app's client-side routes.
    match find("index.html") {
        Some(bytes) => file("index.html", bytes),
        None => Html(NOT_BUILT).into_response(),
    }
}

fn find(path: &str) -> Option<&'static [u8]> {
    ASSETS
        .iter()
        .find(|(asset, _)| *asset == path)
        .map(|(_, bytes)| *bytes)
}

fn file(path: &str, bytes: &'static [u8]) -> Response {
    // Bundled assets have content hashes in their names, so they never change; index.html must be revalidated.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, content_type(path)),
            (header::CACHE_CONTROL, cache),
        ],
        bytes,
    )
        .into_response()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
