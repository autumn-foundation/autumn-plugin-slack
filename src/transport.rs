//! HTTP transport. Only this module does network I/O.
//!
//! - [`ReqwestTransport`]: real HTTPS.
//! - [`MemoryTransport`]: a fake for tests. It records requests and replies
//!   with set responses.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;

/// One HTTP `POST`.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Full URL.
    pub url: String,
    /// Header names in lower case, and values.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let v = if k == "authorization" {
                    "<redacted>"
                } else {
                    v.as_str()
                };
                (k.as_str(), v)
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl HttpRequest {
    /// Makes a request with no headers and no body.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Adds a header. The name becomes lower case.
    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_ascii_lowercase(), value.into()));
        self
    }

    /// Sets the body.
    #[must_use]
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
}

/// One HTTP reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    /// HTTP status.
    pub status: u16,
    /// The `Retry-After` header, in seconds, if any.
    pub retry_after_secs: Option<u64>,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl HttpReply {
    /// A reply with a JSON body.
    #[must_use]
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            retry_after_secs: None,
            body: body.to_string().into_bytes(),
        }
    }

    /// A reply with a text body.
    #[must_use]
    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            retry_after_secs: None,
            body: body.as_bytes().to_vec(),
        }
    }

    /// Sets `Retry-After`.
    #[must_use]
    pub const fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }
}

/// No HTTP reply: connect error or timeout. The text has no header values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

/// Sends HTTP `POST` requests.
pub trait HttpTransport: Send + Sync + 'static {
    /// Sends `req` and returns the reply.
    fn post(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpReply, TransportError>>;
}

/// Real HTTPS transport (`reqwest`, rustls).
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Makes a transport with this time limit per request.
    ///
    /// # Errors
    /// Returns [`TransportError`] when the TLS setup fails.
    pub fn new(timeout: Duration) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("autumn-plugin-slack/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| TransportError(format!("client setup: {e}")))?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn post(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpReply, TransportError>> {
        Box::pin(async move {
            let mut builder = self.client.post(&req.url).body(req.body);
            for (k, v) in &req.headers {
                builder = builder.header(k, v);
            }
            // `without_url`: the error text has no URL query, and no header.
            let res = builder
                .send()
                .await
                .map_err(|e| TransportError(e.without_url().to_string()))?;
            let status = res.status().as_u16();
            let retry_after_secs = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok());
            let body = res
                .bytes()
                .await
                .map_err(|e| TransportError(e.without_url().to_string()))?
                .to_vec();
            Ok(HttpReply {
                status,
                retry_after_secs,
                body,
            })
        })
    }
}

/// A request that [`MemoryTransport`] got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    /// Full URL.
    pub url: String,
    /// Header names in lower case, and values.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// Returns the first header with this name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    /// Decodes a form body. Returns an empty map for a bad body.
    #[must_use]
    pub fn form(&self) -> BTreeMap<String, String> {
        serde_urlencoded::from_bytes(&self.body).unwrap_or_default()
    }

    /// Decodes a JSON body. Returns `Null` for a bad body.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

#[derive(Default)]
struct MemoryInner {
    requests: Vec<RecordedRequest>,
    /// Pattern to replies. The last reply repeats.
    replies: HashMap<String, VecDeque<HttpReply>>,
    /// Pattern to the count of network errors to give first.
    failures: HashMap<String, u32>,
}

/// In-memory fake transport for tests.
///
/// A pattern is a full URL, or a Web API method name such as
/// `chat.postMessage`. With no set reply, it replies `200 {"ok": true}`.
/// Clones share state.
#[derive(Clone, Default)]
pub struct MemoryTransport {
    inner: Arc<Mutex<MemoryInner>>,
}

impl std::fmt::Debug for MemoryTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTransport").finish_non_exhaustive()
    }
}

impl MemoryTransport {
    /// Makes an empty fake.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replies `reply` to each request that matches `pattern`.
    pub fn respond(&self, pattern: &str, reply: HttpReply) {
        self.respond_seq(pattern, vec![reply]);
    }

    /// Replies with `replies` in order. The last one repeats.
    pub fn respond_seq(&self, pattern: &str, replies: Vec<HttpReply>) {
        self.lock()
            .replies
            .insert(pattern.to_owned(), replies.into());
    }

    /// Gives a network error to the next `count` requests that match.
    pub fn fail_next(&self, pattern: &str, count: u32) {
        self.lock().failures.insert(pattern.to_owned(), count);
    }

    /// Returns all requests.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.lock().requests.clone()
    }

    /// Returns the requests that match `pattern`.
    #[must_use]
    pub fn requests_to(&self, pattern: &str) -> Vec<RecordedRequest> {
        self.lock()
            .requests
            .iter()
            .filter(|r| matches(&r.url, pattern))
            .cloned()
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn reply_for(&self, url: &str) -> Result<HttpReply, TransportError> {
        let mut inner = self.lock();
        if let Some(n) = inner
            .failures
            .iter_mut()
            .find(|(p, n)| **n > 0 && matches(url, p))
            .map(|(_, n)| n)
        {
            *n -= 1;
            drop(inner);
            return Err(TransportError("memory transport: set failure".to_owned()));
        }
        let reply = inner
            .replies
            .iter_mut()
            .find(|(p, _)| matches(url, p))
            .and_then(|(_, q)| {
                if q.len() > 1 {
                    q.pop_front()
                } else {
                    q.front().cloned()
                }
            });
        drop(inner);
        Ok(reply.unwrap_or_else(|| HttpReply::json(200, &serde_json::json!({"ok": true}))))
    }
}

/// A pattern matches the full URL, or the last path part (a method name).
fn matches(url: &str, pattern: &str) -> bool {
    url == pattern || url.rsplit('/').next() == Some(pattern)
}

impl HttpTransport for MemoryTransport {
    fn post(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpReply, TransportError>> {
        Box::pin(async move {
            let reply = self.reply_for(&req.url);
            self.lock().requests.push(RecordedRequest {
                url: req.url,
                headers: req.headers,
                body: req.body,
            });
            reply
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_default_seq_and_failures() {
        let t = MemoryTransport::new();
        let url = "https://slack.com/api/chat.postMessage";
        let r = t.post(HttpRequest::new(url)).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, br#"{"ok":true}"#);
        t.respond_seq(
            "chat.postMessage",
            vec![HttpReply::text(500, "a"), HttpReply::text(201, "b")],
        );
        assert_eq!(t.post(HttpRequest::new(url)).await.unwrap().status, 500);
        assert_eq!(t.post(HttpRequest::new(url)).await.unwrap().status, 201);
        assert_eq!(t.post(HttpRequest::new(url)).await.unwrap().status, 201);
        t.fail_next(url, 1);
        assert!(t.post(HttpRequest::new(url)).await.is_err());
        assert_eq!(t.post(HttpRequest::new(url)).await.unwrap().status, 201);
        assert_eq!(t.requests_to("chat.postMessage").len(), 6);
        assert!(
            t.requests_to("postMessage").is_empty(),
            "pattern is a whole method"
        );
        assert!(t.requests_to("chat.update").is_empty());
    }

    #[test]
    fn recorded_decoders() {
        let r = RecordedRequest {
            url: "u".into(),
            headers: vec![("x-a".into(), "1".into())],
            body: b"a=1&b=two+words".to_vec(),
        };
        assert_eq!(r.header("x-a").as_deref(), Some("1"));
        assert_eq!(r.header("X-A").as_deref(), Some("1"));
        assert_eq!(r.form()["b"], "two words");
        assert!(r.json().is_null());
    }

    #[test]
    fn debug_redacts_authorization() {
        let r = HttpRequest::new("u").header("Authorization", "Bearer xoxb-1");
        assert!(!format!("{r:?}").contains("xoxb-1"));
        assert!(format!("{:?}", MemoryTransport::new()).contains("MemoryTransport"));
    }
}
