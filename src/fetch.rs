//! One HTTP fetch for the dashboard's pollers: the body is captured as text
//! before anything parses it, so a response this build cannot decode — the
//! one most worth keeping — rides along on the error.

use std::time::Duration;

use reqwest::StatusCode;

/// Whether a client keeps its connections for the next request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    KeepAlive,
    PerRequest,
}

/// The one place an HTTP client's connection policy and timeout are set, so a
/// new adapter has to state both. A builder, so an adapter with extra needs
/// adds them before `build()`.
pub fn client(connection: Connection, timeout: Duration) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder().timeout(timeout);
    match connection {
        Connection::KeepAlive => builder,
        Connection::PerRequest => builder.pool_max_idle_per_host(0),
    }
}

/// `error` and every cause beneath it, joined with `": "`; reqwest's own
/// message alone ("error sending request") hides whether it was a timeout, a
/// reset, DNS or TLS.
pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// What can go wrong fetching a body and parsing it.
#[derive(Debug)]
pub enum FetchError {
    Request(String),
    /// The server answered, but not with a success: a rate limit or an
    /// outage, never a body worth parsing.
    Status(StatusCode),
    Parse {
        body: String,
        error: String,
    },
}

impl FetchError {
    /// Whether trying again could get a different answer: a transport failure
    /// or a server-side outage, never a rejected request or an undecodable body.
    /// A `Request` error that no retry can fix (a malformed URL, a redirect
    /// loop) still reads as transient; the retry cap bounds what that costs.
    pub fn is_transient(&self) -> bool {
        match self {
            FetchError::Request(_) => true,
            FetchError::Status(status) => status.is_server_error(),
            FetchError::Parse { .. } => false,
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Request(e) => write!(f, "request failed: {e}"),
            FetchError::Status(status) => write!(f, "HTTP {status}"),
            FetchError::Parse { error, .. } => write!(f, "parse error: {error}"),
        }
    }
}

/// Sends `request` and parses its body with `parse`.
pub async fn fetch_parsed<T>(
    request: reqwest::RequestBuilder,
    parse: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, FetchError> {
    let response = request
        .send()
        .await
        .map_err(|e| FetchError::Request(error_chain(&e)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(FetchError::Status(status));
    }
    let body = response
        .text()
        .await
        .map_err(|e| FetchError::Request(error_chain(&e)))?;
    parse(&body).map_err(|error| FetchError::Parse { body, error })
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A server that answers one request with `response`, verbatim.
    async fn serve_once(response: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn a_rate_limit_is_an_http_error_not_a_parse_error() {
        let url = serve_once(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 9\r\nConnection: close\r\n\r\nslow down",
        )
        .await;
        let parsed = fetch_parsed(reqwest::Client::new().get(url), |_| Ok(())).await;

        let error = parsed.unwrap_err();
        assert!(matches!(
            error,
            FetchError::Status(StatusCode::TOO_MANY_REQUESTS)
        ));
        assert_eq!(error.to_string(), "HTTP 429 Too Many Requests");
    }

    #[tokio::test]
    async fn a_success_is_parsed() {
        let url =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n42").await;
        let parsed = fetch_parsed(reqwest::Client::new().get(url), |body| {
            body.parse::<u32>().map_err(|e| e.to_string())
        })
        .await;

        assert_eq!(parsed.unwrap(), 42);
    }

    /// A server that counts accepted connections and answers every request
    /// on each with an empty 200.
    async fn serve_counting(connections: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut request = [0u8; 1024];
                    while socket.read(&mut request).await.is_ok_and(|n| n > 0) {
                        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n42";
                        if socket.write_all(ok).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        format!("http://{addr}/")
    }

    async fn connections_for_two_requests(connection: Connection) -> usize {
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let url = serve_counting(connections.clone()).await;
        let http = client(connection, Duration::from_secs(5)).build().unwrap();
        for _ in 0..2 {
            fetch_parsed(http.get(&url), |_| Ok(())).await.unwrap();
        }
        connections.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn a_per_request_client_opens_a_connection_for_each_request() {
        assert_eq!(
            connections_for_two_requests(Connection::PerRequest).await,
            2
        );
    }

    #[tokio::test]
    async fn a_keep_alive_client_reuses_its_connection() {
        assert_eq!(connections_for_two_requests(Connection::KeepAlive).await, 1);
    }

    #[tokio::test]
    async fn a_timeout_error_names_its_cause() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let http = client(Connection::PerRequest, Duration::from_millis(100))
            .build()
            .unwrap();

        let error = fetch_parsed(http.get(url), |_| Ok(())).await.unwrap_err();

        assert!(matches!(error, FetchError::Request(_)));
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[test]
    fn only_transport_failures_and_server_errors_are_transient() {
        let status = |code| FetchError::Status(StatusCode::from_u16(code).unwrap());
        assert!(FetchError::Request("reset".into()).is_transient());
        assert!(status(500).is_transient());
        assert!(status(503).is_transient());
        assert!(!status(400).is_transient());
        assert!(!status(401).is_transient());
        assert!(!status(429).is_transient());
        assert!(
            !FetchError::Parse {
                body: String::new(),
                error: String::new()
            }
            .is_transient()
        );
    }
}
