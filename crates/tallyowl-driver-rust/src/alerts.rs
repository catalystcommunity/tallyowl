//! Verifying a signed alert callback.
//!
//! An application that declares `TallyOwlAlertReceiver` receives each alert
//! notification as an `AlertNotifyRequest`. The head signs it the same way it
//! signs a webhook: a keyed BLAKE3 hash, with the secret the target names, over
//! the decimal text of `signed_at`, a full stop, and `body`. The TLS connection
//! proves the receiver to the head. This signature proves the head to the
//! receiver, because the head shows no certificate to an application.
//!
//! Verify first, and read `body` only when [`verify_alert_callback`] returns it.

use tallyowl_collector_api::types::AlertNotifyRequest;

/// How far `signed_at` may be from the receiver's own clock, in either
/// direction. Five minutes covers ordinary clock drift. A notification that is
/// older than this is a replay, and one that is newer is from a clock that is
/// wrong. The head signs each attempt again, so a retry is never too old.
pub const ALERT_CALLBACK_WINDOW_MS: i64 = 5 * 60 * 1000;

/// Why a callback did not verify. Answer each one with a `ServiceError` whose
/// code is `unauthenticated` and which is not retryable: the head does not send
/// that notification again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertCallbackError {
    /// The signature is not the one this secret makes for this time and body.
    /// A wrong secret, a changed body, or a changed time.
    Signature,
    /// `signed_at` is outside [`ALERT_CALLBACK_WINDOW_MS`] of `now_ms`.
    Time { signed_at: i64, now_ms: i64 },
    /// The receiver has no secret, so nothing can verify.
    NoSecret,
}

impl std::fmt::Display for AlertCallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlertCallbackError::Signature => write!(
                f,
                "This alert callback is not signed with the secret this receiver holds. Give the receiver the secret that the alert target's `secret_ref` names."
            ),
            AlertCallbackError::Time { signed_at, now_ms } => write!(
                f,
                "This alert callback was signed at {signed_at}, which is {} seconds from this receiver's clock, and the limit is {} seconds. Check the clocks, or this is a replay.",
                (now_ms - signed_at).abs() / 1000,
                ALERT_CALLBACK_WINDOW_MS / 1000
            ),
            AlertCallbackError::NoSecret => write!(
                f,
                "This receiver has no secret, so it cannot verify an alert callback. Give it the secret that the alert target's `secret_ref` names."
            ),
        }
    }
}

impl std::error::Error for AlertCallbackError {}

/// Verify one alert callback, and return the notification body (JSON) when it
/// verifies. `now_ms` is the receiver's clock in milliseconds since 1970.
pub fn verify_alert_callback<'a>(
    secret: &str,
    request: &'a AlertNotifyRequest,
    now_ms: i64,
) -> Result<&'a [u8], AlertCallbackError> {
    if secret.is_empty() {
        return Err(AlertCallbackError::NoSecret);
    }
    // The signature first: a time is worth reporting only on a request that
    // the head sent.
    let presented =
        blake3::Hash::from_hex(&request.signature).map_err(|_| AlertCallbackError::Signature)?;
    // `blake3::Hash` compares in constant time.
    if presented != expected(secret, request.signed_at, &request.body) {
        return Err(AlertCallbackError::Signature);
    }
    if (now_ms - request.signed_at).abs() > ALERT_CALLBACK_WINDOW_MS {
        return Err(AlertCallbackError::Time {
            signed_at: request.signed_at,
            now_ms,
        });
    }
    Ok(&request.body)
}

/// The head's signature for one time and body. The key is the BLAKE3 hash of
/// the secret, because a key is exactly 32 bytes and a secret is text of any
/// length.
fn expected(secret: &str, signed_at: i64, body: &[u8]) -> blake3::Hash {
    let key: [u8; 32] = *blake3::hash(secret.as_bytes()).as_bytes();
    let mut hasher = blake3::Hasher::new_keyed(&key);
    hasher.update(signed_at.to_string().as_bytes());
    hasher.update(b".");
    hasher.update(body);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_collector_api::codec::decode_alert_notify_request;

    /// The vector every implementation agrees on: the head signs it, and the Go
    /// and Rust app drivers verify it. `golden/alert-callback.json`.
    struct Vector {
        secret: String,
        signed_at: i64,
        body: String,
        signature: String,
        request: Vec<u8>,
    }

    fn vector() -> Vector {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../golden/alert-callback.json"),
        )
        .expect("the shared vector");
        let field = |name: &str| -> String {
            let start = text.find(&format!("\"{name}\": ")).expect(name) + name.len() + 4;
            let rest = &text[start..];
            if let Some(stripped) = rest.strip_prefix('"') {
                let mut out = String::new();
                let mut chars = stripped.chars();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => out.push(chars.next().expect("an escape")),
                        '"' => break,
                        c => out.push(c),
                    }
                }
                out
            } else {
                rest.split([',', '\n']).next().unwrap().trim().to_string()
            }
        };
        let hex = field("request");
        let request = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
            .collect();
        Vector {
            secret: field("secret"),
            signed_at: field("signed_at").parse().expect("a number"),
            body: field("body"),
            signature: field("signature"),
            request,
        }
    }

    fn request(v: &Vector) -> AlertNotifyRequest {
        AlertNotifyRequest {
            signed_at: v.signed_at,
            signature: v.signature.clone(),
            body: v.body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn the_shared_vector_verifies_and_decodes_to_the_same_request() {
        let v = vector();
        let decoded = decode_alert_notify_request(&v.request).expect("the head's bytes decode");
        assert_eq!(decoded, request(&v), "the encoded request is the vector");
        let body = verify_alert_callback(&v.secret, &decoded, v.signed_at).expect("verifies");
        assert_eq!(body, v.body.as_bytes());
    }

    #[test]
    fn a_changed_body_time_or_secret_does_not_verify() {
        let v = vector();
        let mut changed = request(&v);
        changed.body[0] ^= 1;
        assert_eq!(
            verify_alert_callback(&v.secret, &changed, v.signed_at),
            Err(AlertCallbackError::Signature)
        );
        let mut moved = request(&v);
        moved.signed_at += 1;
        assert_eq!(
            verify_alert_callback(&v.secret, &moved, moved.signed_at),
            Err(AlertCallbackError::Signature),
            "the time is inside the signature"
        );
        assert_eq!(
            verify_alert_callback("another secret", &request(&v), v.signed_at),
            Err(AlertCallbackError::Signature)
        );
        assert_eq!(
            verify_alert_callback("", &request(&v), v.signed_at),
            Err(AlertCallbackError::NoSecret)
        );
        let mut garbled = request(&v);
        garbled.signature = "z".repeat(64);
        assert_eq!(
            verify_alert_callback(&v.secret, &garbled, v.signed_at),
            Err(AlertCallbackError::Signature)
        );
    }

    #[test]
    fn a_callback_outside_the_window_is_refused_either_way() {
        let v = vector();
        let r = request(&v);
        let edge = v.signed_at + ALERT_CALLBACK_WINDOW_MS;
        assert!(
            verify_alert_callback(&v.secret, &r, edge).is_ok(),
            "the window is inclusive"
        );
        let late = verify_alert_callback(&v.secret, &r, edge + 1).expect_err("a replay");
        assert!(matches!(late, AlertCallbackError::Time { .. }));
        assert!(late.to_string().contains("replay"), "{late}");
        assert!(matches!(
            verify_alert_callback(&v.secret, &r, v.signed_at - ALERT_CALLBACK_WINDOW_MS - 1),
            Err(AlertCallbackError::Time { .. })
        ));
    }
}
