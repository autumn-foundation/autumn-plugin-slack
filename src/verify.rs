//! Slack request signature check (`v0`).
//!
//! Base string: `v0:{timestamp}:{raw body}`. Key: the signing secret.
//! Signature header: `X-Slack-Signature: v0=<hex HMAC-SHA256>`.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::policy::timestamp_fresh;

/// Header with the request timestamp (Unix seconds).
pub const TIMESTAMP_HEADER: &str = "x-slack-request-timestamp";
/// Header with the signature.
pub const SIGNATURE_HEADER: &str = "x-slack-signature";

/// Why a request failed verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// No timestamp header.
    #[error("missing timestamp header")]
    MissingTimestamp,
    /// No signature header.
    #[error("missing signature header")]
    MissingSignature,
    /// The timestamp is not a number.
    #[error("bad timestamp header")]
    BadTimestamp,
    /// The signature is not `v0=` and 64 hex digits.
    #[error("bad signature header")]
    BadSignatureFormat,
    /// The timestamp is outside the tolerance.
    #[error("stale timestamp")]
    Stale,
    /// No secret gives this signature.
    #[error("signature mismatch")]
    Mismatch,
}

impl VerifyError {
    /// HTTP status for this error: 400 for bad input, 401 for a failed check.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::Stale | Self::Mismatch => 401,
            _ => 400,
        }
    }

    /// Short label for metrics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stale => "stale",
            Self::Mismatch => "bad_signature",
            _ => "bad_request",
        }
    }
}

/// Checks Slack signatures. Holds the current and the previous secrets.
#[derive(Clone)]
pub struct Verifier {
    secrets: Vec<Vec<u8>>,
    tolerance_secs: u64,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier")
            .field("secrets", &self.secrets.len())
            .field("tolerance_secs", &self.tolerance_secs)
            .finish()
    }
}

impl Verifier {
    /// Makes a verifier. `previous` holds old secrets during rotation.
    ///
    /// It ignores an empty or blank secret. HMAC accepts an empty key, so an
    /// empty secret would let anyone sign. With no secret left, all checks fail.
    #[must_use]
    pub fn new(secret: &str, previous: &[String], tolerance_secs: u64) -> Self {
        let secrets = std::iter::once(secret)
            .chain(previous.iter().map(String::as_str))
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.as_bytes().to_vec())
            .collect();
        Self {
            secrets,
            tolerance_secs,
        }
    }

    /// Checks one request. Call it before any parse of `body`.
    ///
    /// # Errors
    /// Returns the first [`VerifyError`] found.
    pub fn verify(
        &self,
        timestamp: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
        now_secs: u64,
    ) -> Result<(), VerifyError> {
        let ts = timestamp.ok_or(VerifyError::MissingTimestamp)?;
        let sig = signature.ok_or(VerifyError::MissingSignature)?;
        // Digits only: no sign, no space.
        if ts.is_empty() || !ts.bytes().all(|b| b.is_ascii_digit()) {
            return Err(VerifyError::BadTimestamp);
        }
        let ts_secs: u64 = ts.parse().map_err(|_| VerifyError::BadTimestamp)?;
        let expected = sig
            .strip_prefix("v0=")
            .filter(|h| h.len() == 64)
            .and_then(|h| hex::decode(h).ok())
            .ok_or(VerifyError::BadSignatureFormat)?;
        if !timestamp_fresh(now_secs, ts_secs, self.tolerance_secs) {
            return Err(VerifyError::Stale);
        }
        for secret in &self.secrets {
            // `verify_slice` compares in constant time.
            if mac(secret, ts, body).verify_slice(&expected).is_ok() {
                return Ok(());
            }
        }
        Err(VerifyError::Mismatch)
    }
}

/// Returns `v0=<hex>` for this secret, timestamp, and body.
pub(crate) fn signature(secret: &[u8], ts: &str, body: &[u8]) -> String {
    format!(
        "v0={}",
        hex::encode(mac(secret, ts, body).finalize().into_bytes())
    )
}

/// HMAC-SHA256 over `v0:{ts}:{body}`.
fn mac(secret: &[u8], ts: &str, body: &[u8]) -> Hmac<Sha256> {
    // HMAC accepts a key of any length. The error case cannot happen.
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
    m.update(b"v0:");
    m.update(ts.as_bytes());
    m.update(b":");
    m.update(body);
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // Example from the Slack docs ("Verifying requests from Slack").
    const DOC_SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
    const DOC_TS: &str = "1531420618";
    const DOC_BODY: &str = "token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
    const DOC_SIG: &str = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";

    fn now() -> u64 {
        DOC_TS.parse().unwrap()
    }

    #[test]
    fn slack_doc_example_verifies() {
        assert_eq!(
            signature(DOC_SECRET.as_bytes(), DOC_TS, DOC_BODY.as_bytes()),
            DOC_SIG
        );
        let v = Verifier::new(DOC_SECRET, &[], 300);
        assert_eq!(
            v.verify(Some(DOC_TS), Some(DOC_SIG), DOC_BODY.as_bytes(), now()),
            Ok(())
        );
    }

    #[test]
    fn errors_in_order() {
        let v = Verifier::new(DOC_SECRET, &[], 300);
        let b = DOC_BODY.as_bytes();
        assert_eq!(
            v.verify(None, Some(DOC_SIG), b, now()),
            Err(VerifyError::MissingTimestamp)
        );
        assert_eq!(
            v.verify(Some(DOC_TS), None, b, now()),
            Err(VerifyError::MissingSignature)
        );
        assert_eq!(
            v.verify(Some("-5"), Some(DOC_SIG), b, now()),
            Err(VerifyError::BadTimestamp)
        );
        assert_eq!(
            v.verify(Some(DOC_TS), Some("v1=00"), b, now()),
            Err(VerifyError::BadSignatureFormat)
        );
        assert_eq!(
            v.verify(Some(DOC_TS), Some("v0=zz"), b, now()),
            Err(VerifyError::BadSignatureFormat)
        );
        assert_eq!(
            v.verify(Some(DOC_TS), Some(DOC_SIG), b, now() + 301),
            Err(VerifyError::Stale)
        );
        let mut bad = DOC_SIG.to_owned();
        bad.replace_range(bad.len() - 1.., "4");
        assert_eq!(
            v.verify(Some(DOC_TS), Some(&bad), b, now()),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            v.verify(Some(DOC_TS), Some(DOC_SIG), b"x", now()),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn upper_case_hex_and_space_in_timestamp() {
        let v = Verifier::new(DOC_SECRET, &[], 300);
        let upper = format!("v0={}", DOC_SIG[3..].to_ascii_uppercase());
        assert_eq!(
            v.verify(Some(DOC_TS), Some(&upper), DOC_BODY.as_bytes(), now()),
            Ok(())
        );
        assert_eq!(
            v.verify(
                Some(" 1531420618"),
                Some(DOC_SIG),
                DOC_BODY.as_bytes(),
                now()
            ),
            Err(VerifyError::BadTimestamp)
        );
    }

    #[test]
    fn previous_secret_verifies() {
        let v = Verifier::new("new-secret", &[DOC_SECRET.to_owned()], 300);
        assert_eq!(
            v.verify(Some(DOC_TS), Some(DOC_SIG), DOC_BODY.as_bytes(), now()),
            Ok(())
        );
        let v = Verifier::new("new-secret", &[], 300);
        assert_eq!(
            v.verify(Some(DOC_TS), Some(DOC_SIG), DOC_BODY.as_bytes(), now()),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn empty_secrets_never_verify() {
        // Regression: an empty previous secret let anyone sign with an empty key.
        let body = b"command=%2Fx";
        let forged = signature(b"", DOC_TS, body);
        let v = Verifier::new(DOC_SECRET, &[String::new(), "  ".to_owned()], 300);
        assert_eq!(
            v.verify(Some(DOC_TS), Some(&forged), body, now()),
            Err(VerifyError::Mismatch)
        );
        let v = Verifier::new("", &[], 300);
        assert_eq!(
            v.verify(Some(DOC_TS), Some(&forged), body, now()),
            Err(VerifyError::Mismatch)
        );
        assert!(format!("{v:?}").contains("secrets: 0"));
    }

    #[test]
    fn debug_hides_secrets() {
        let v = Verifier::new(DOC_SECRET, &["older".to_owned()], 300);
        let d = format!("{v:?}");
        assert!(!d.contains(DOC_SECRET) && !d.contains("older"), "{d}");
    }

    #[test]
    fn status_and_labels() {
        assert_eq!(VerifyError::Mismatch.status(), 401);
        assert_eq!(VerifyError::Stale.status(), 401);
        assert_eq!(VerifyError::BadTimestamp.status(), 400);
        assert_eq!(VerifyError::MissingSignature.label(), "bad_request");
        assert_eq!(VerifyError::Mismatch.label(), "bad_signature");
        assert_eq!(VerifyError::Stale.label(), "stale");
    }

    proptest! {
        #[test]
        fn any_signed_body_verifies_and_any_flip_fails(
            body in proptest::collection::vec(any::<u8>(), 0..512),
            flip in any::<prop::sample::Index>(),
        ) {
            let v = Verifier::new("s3cret", &[], 300);
            let ts = "1700000000";
            let sig = signature(b"s3cret", ts, &body);
            prop_assert_eq!(v.verify(Some(ts), Some(&sig), &body, 1_700_000_000), Ok(()));
            if !body.is_empty() {
                let mut tampered = body;
                let i = flip.index(tampered.len());
                tampered[i] ^= 0x01;
                prop_assert_eq!(v.verify(Some(ts), Some(&sig), &tampered, 1_700_000_000), Err(VerifyError::Mismatch));
            }
        }
    }
}
