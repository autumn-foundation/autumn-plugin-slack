//! Helpers for app tests.
//!
//! ```rust
//! use autumn_plugin_slack::testing;
//!
//! let sig = testing::sign("secret", 1_700_000_000, b"{}");
//! assert!(sig.starts_with("v0="));
//! ```
//!
//! See also [`crate::transport::MemoryTransport`].

/// Returns the `X-Slack-Signature` value for this secret, time, and body.
#[must_use]
pub fn sign(secret: &str, timestamp: u64, body: &[u8]) -> String {
    crate::verify::signature(secret.as_bytes(), &timestamp.to_string(), body)
}

/// Returns the two Slack headers for a signed request, as `(name, value)`.
#[must_use]
pub fn signed_headers(secret: &str, timestamp: u64, body: &[u8]) -> [(&'static str, String); 2] {
    [
        (crate::verify::TIMESTAMP_HEADER, timestamp.to_string()),
        (
            crate::verify::SIGNATURE_HEADER,
            sign(secret, timestamp, body),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::Verifier;

    #[test]
    fn signed_headers_verify() {
        let [(_, ts), (_, sig)] = signed_headers("k", 10, b"body");
        let v = Verifier::new("k", &[], 300);
        assert_eq!(v.verify(Some(&ts), Some(&sig), b"body", 10), Ok(()));
    }
}
