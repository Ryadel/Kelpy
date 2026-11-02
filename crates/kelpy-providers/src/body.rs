//! Response bodies read within a size: a service that keeps sending must not fill Kelpy's memory.

use futures_util::StreamExt;

use crate::{ProviderError, network_error};

/// The most a whole response body may hold. Model lists, the largest, are well under it.
pub const MAX_BODY: usize = 8 << 20;
/// The most read of an error body, for its message.
const MAX_ERROR_BODY: usize = 64 << 10;

/// Reads the body of `response`, failing once it passes `limit` bytes.
pub async fn read(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let too_large =
        || ProviderError::Protocol(format!("the response is larger than {limit} bytes"));
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(network_error)?;
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The start of an error body, for its message; empty if it cannot be read.
pub async fn error_text(response: reqwest::Response) -> String {
    let mut text = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(Ok(chunk)) = chunks.next().await {
        let room = MAX_ERROR_BODY - text.len();
        text.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if text.len() == MAX_ERROR_BODY {
            break;
        }
    }
    String::from_utf8_lossy(&text).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_test_support::{FakeServer, read_request};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    async fn get(body: String) -> reqwest::Response {
        crate::install_tls();
        let server = FakeServer::start(vec![("GET /body", "200 OK", body)]).await;
        reqwest::get(format!("{}/body", server.url)).await.unwrap()
    }

    /// A response that does not say its length, so only reading it tells.
    async fn get_unsized(body: String) -> reqwest::Response {
        crate::install_tls();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            let head = "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n";
            let _ = socket.write_all(format!("{head}{body}").as_bytes()).await;
        });
        reqwest::get(format!("http://{address}/body"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_body_is_read_up_to_its_limit_only() {
        let body = "x".repeat(1000);
        assert_eq!(
            read(get(body.clone()).await, 1000).await.unwrap().len(),
            1000
        );
        assert!(matches!(
            read(get(body.clone()).await, 999).await,
            Err(ProviderError::Protocol(_))
        ));
        assert_eq!(
            read(get_unsized(body.clone()).await, 1000)
                .await
                .unwrap()
                .len(),
            1000
        );
        assert!(matches!(
            read(get_unsized(body).await, 999).await,
            Err(ProviderError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn an_error_body_is_cut_to_its_start() {
        let text = error_text(get("e".repeat(MAX_ERROR_BODY * 2)).await).await;
        assert_eq!(text.len(), MAX_ERROR_BODY);
    }
}
