//! One HTTP fetch for the dashboard's pollers: the body is captured as text
//! before anything parses it, so a response this build cannot decode — the
//! one most worth keeping — rides along on the error.

use reqwest::StatusCode;

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
        .map_err(|e| FetchError::Request(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(FetchError::Status(status));
    }
    let body = response
        .text()
        .await
        .map_err(|e| FetchError::Request(e.to_string()))?;
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
}
