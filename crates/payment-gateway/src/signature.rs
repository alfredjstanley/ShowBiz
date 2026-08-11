//! Webhook body signing.
//!
//! Every webhook carries `X-Payment-Signature: sha256=<hex>`, an HMAC-SHA256 of the **raw
//! request body** keyed with the shared secret (`PAYMENT_WEBHOOK_SECRET`, the same value on
//! both sides).
//!
//! Verifying it is a bonus requirement. If you do, the one thing that matters: sign the
//! bytes you actually received, not a re-serialisation of the parsed JSON. Round-tripping
//! through a struct reorders keys and changes whitespace, and the signature will not match.
//! In axum that means taking the body as `Bytes` or `String` before deserialising.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Returns the `sha256=<hex>` value for a body.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(body);

    let digest = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(7 + digest.len() * 2);
    hex.push_str("sha256=");
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_stable_and_key_dependent() {
        let body = br#"{"transaction_id":"txn_1"}"#;

        let a = sign("secret", body);
        assert_eq!(a, sign("secret", body), "signing is deterministic");
        assert_ne!(a, sign("other-secret", body), "key changes the signature");
        assert!(a.starts_with("sha256="));
        assert_eq!(a.len(), 7 + 64);
    }

    #[test]
    fn whitespace_changes_the_signature() {
        // Which is exactly why you must verify against the raw bytes.
        assert_ne!(sign("k", b"{\"a\":1}"), sign("k", b"{ \"a\": 1 }"));
    }
}
