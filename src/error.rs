//! Error types.

use crate::verify::VerifyError;

/// Error from a handler. Any error type converts with `?`.
pub type HandlerError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Result of a handler.
pub type HandlerResult<T> = Result<T, HandlerError>;

/// Plugin and client errors. Messages never hold a token or a secret.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SlackError {
    /// Bad config or bad input from the app.
    #[error("slack config: {0}")]
    Config(String),
    /// Slack replied `ok: false`.
    #[error("slack {method} failed: {code}")]
    Api {
        /// Web API method.
        method: String,
        /// Slack error code, for example `channel_not_found`.
        code: String,
        /// Scope that the call needs, if Slack names one.
        needed: Option<String>,
    },
    /// Slack replied 429 and the client stopped.
    #[error("slack rate limited (retry after {retry_after_secs:?} s)")]
    RateLimited {
        /// The `Retry-After` value, if any.
        retry_after_secs: Option<u64>,
    },
    /// Slack replied with an HTTP error status.
    #[error("slack http {status}")]
    Http {
        /// HTTP status.
        status: u16,
        /// First bytes of the body.
        body: String,
    },
    /// No HTTP reply: connect error or timeout.
    #[error("slack transport: {0}")]
    Transport(String),
    /// The reply is not the expected JSON.
    #[error("slack reply decode: {0}")]
    Decode(String),
    /// The `response_url` is not an allowed Slack URL.
    #[error("response_url is not an allowed Slack URL")]
    BadResponseUrl,
    /// Request verification failed.
    #[error(transparent)]
    Verify(#[from] VerifyError),
}

impl SlackError {
    /// Returns the Slack error code for an [`SlackError::Api`] error.
    #[must_use]
    pub fn slack_code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }
}
