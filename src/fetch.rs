//! One HTTP fetch for the dashboard's pollers: the body is captured as text
//! before anything parses it, so a response this build cannot decode — the
//! one most worth keeping — rides along on the error.

/// What can go wrong fetching a body and parsing it.
#[derive(Debug)]
pub enum FetchError {
    Request(String),
    Parse { body: String, error: String },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Request(e) => write!(f, "request failed: {e}"),
            FetchError::Parse { error, .. } => write!(f, "parse error: {error}"),
        }
    }
}

/// Sends `request` and parses its body with `parse`.
pub async fn fetch_parsed<T>(
    request: reqwest::RequestBuilder,
    parse: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, FetchError> {
    let body = request
        .send()
        .await
        .map_err(|e| FetchError::Request(e.to_string()))?
        .text()
        .await
        .map_err(|e| FetchError::Request(e.to_string()))?;
    parse(&body).map_err(|error| FetchError::Parse { body, error })
}
