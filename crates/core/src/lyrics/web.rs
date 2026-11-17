//! HTTP plumbing shared by the unofficial lyrics providers: one client
//! setup and one mapping of answers to "found / not found / failed".

use std::time::Duration;

use serde::de::DeserializeOwned;

use super::lrclib::retry_after;
use super::{LyricsError, ProviderError, USER_AGENT};

/// Timeout of one provider request.
const TIMEOUT: Duration = Duration::from_secs(15);

/// HTTP client for a provider (app `User-Agent`, [`TIMEOUT`]).
///
/// # Errors
///
/// Returns an error if the TLS backend cannot be initialised.
pub(crate) fn client() -> Result<reqwest::Client, LyricsError> {
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build()?)
}

/// Sends `request`. `404` / `410` mean "nothing there" (`Ok(None)`); `429`,
/// `5xx`, timeouts and network errors are retryable failures; other error
/// statuses are fatal.
pub(crate) async fn send(
    request: reqwest::RequestBuilder,
) -> Result<Option<reqwest::Response>, ProviderError> {
    let response = request
        .send()
        .await
        .map_err(|e| ProviderError::from_reqwest(&e))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(ProviderError::from_status(
            status.as_u16(),
            retry_after(response.headers()),
        ));
    }
    Ok(Some(response))
}

/// [`send`] and decode the JSON body.
pub(crate) async fn json<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<Option<T>, ProviderError> {
    let Some(response) = send(request).await? else {
        return Ok(None);
    };
    let bytes = response
        .bytes()
        .await
        .map_err(|e| ProviderError::from_reqwest(&e))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| ProviderError::Fatal(format!("unreadable response: {e}")))
}

/// [`send`] and read the body as text.
pub(crate) async fn text(
    request: reqwest::RequestBuilder,
) -> Result<Option<String>, ProviderError> {
    let Some(response) = send(request).await? else {
        return Ok(None);
    };
    response
        .text()
        .await
        .map(Some)
        .map_err(|e| ProviderError::from_reqwest(&e))
}

#[cfg(test)]
pub(crate) mod test_server {
    //! A one-shot local HTTP server for provider tests.

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serves the given `(status, extra headers, body)` replies in order and
    /// returns the base URL.
    pub(crate) async fn serve(replies: Vec<(u16, &'static str, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for (status, headers, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 16 * 1024];
                let _ = socket.read(&mut buf).await.unwrap();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\n{headers}connection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::serve;
    use super::*;

    async fn answer(status: u16, headers: &'static str) -> Result<Option<String>, ProviderError> {
        let base = serve(vec![(status, headers, "x".to_owned())]).await;
        text(client().unwrap().get(base)).await
    }

    #[tokio::test]
    async fn not_found_should_be_none() {
        assert_eq!(answer(404, "").await, Ok(None));
    }

    #[tokio::test]
    async fn rate_limit_should_be_retryable_with_retry_after() {
        assert_eq!(
            answer(429, "retry-after: 7\r\n").await,
            Err(ProviderError::Retryable {
                reason: "rate limited (HTTP 429)".to_owned(),
                retry_after: Some(Duration::from_secs(7)),
            })
        );
    }

    #[tokio::test]
    async fn server_error_should_be_retryable() {
        assert!(answer(503, "").await.unwrap_err().is_retryable());
    }

    #[tokio::test]
    async fn forbidden_should_be_fatal() {
        assert!(!answer(403, "").await.unwrap_err().is_retryable());
    }

    #[tokio::test]
    async fn refused_connection_should_be_retryable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let result = text(client().unwrap().get(format!("http://{addr}"))).await;
        assert!(result.unwrap_err().is_retryable());
    }

    #[tokio::test]
    async fn garbage_json_should_be_fatal() {
        let base = serve(vec![(200, "", "not json".to_owned())]).await;
        let result = json::<serde_json::Value>(client().unwrap().get(base)).await;
        assert!(!result.unwrap_err().is_retryable());
    }
}
