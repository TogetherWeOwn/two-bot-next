//! Exact-byte signing for outbound Discord realtime v1 ingest.

use crate::{internal_actions::sign_post, Secret};

pub const INGEST_PATH: &str = "/internal/discord-live/v1/ingest";
const MAX_TIMESTAMP: i64 = 999_999_999_999_999;

/// Signing fields for the ingest POST. Only expose the signature at the
/// transport boundary; never log headers or body bytes.
pub struct IngestSignature {
    pub timestamp: String,
    pub nonce: String,
    pub signature: Secret<String>,
}

/// Sign the exact body bytes that will be sent. The site encodes the key as
/// UTF-8; use a separate bot-to-site ingress key and a fresh cryptographically
/// random nonce for each attempt, including retries of the same body.
/// The route is fixed and cannot be selected by data from Discord or the site.
#[must_use]
pub fn sign_ingest(
    key: &Secret<String>,
    unix_seconds: i64,
    nonce: [u8; 16],
    body: &[u8],
) -> Option<IngestSignature> {
    if !(1..=MAX_TIMESTAMP).contains(&unix_seconds) || key.expose().is_empty() {
        return None;
    }
    let timestamp = unix_seconds.to_string();
    let nonce = hex::encode(nonce);
    let signature = Secret::new(sign_post(
        INGEST_PATH,
        key.expose().as_bytes(),
        &timestamp,
        &nonce,
        body,
    ));
    Some(IngestSignature {
        timestamp,
        nonce,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal_actions::sign;

    #[test]
    fn signs_the_ingest_path_and_the_original_utf8_bytes() {
        // Same synthetic input as the companion hub's Web Crypto verification test.
        let key = Secret::new(String::from("offline signing fixture"));
        let raw = "{ \"name\": \"é\" }\n".as_bytes();
        let signed = sign_ingest(&key, 1_800_000_000, [0xaa; 16], raw).unwrap();
        assert_eq!(signed.timestamp, "1800000000");
        assert_eq!(signed.nonce, "a".repeat(32));
        assert_eq!(
            signed.signature.expose(),
            "sha256=3dd5a5f84a1e400c2528013e9ff90eb3dc49b4db30650a72d670351b7a9488ba"
        );
        let action = sign(
            key.expose().as_bytes(),
            &signed.timestamp,
            &signed.nonce,
            raw,
        );
        assert_ne!(signed.signature.expose(), &action);
        let compact = "{\"name\":\"é\"}".as_bytes();
        let reencoded = sign_ingest(&key, 1_800_000_000, [0xaa; 16], compact).unwrap();
        assert_ne!(signed.signature.expose(), reencoded.signature.expose());
        let retry = sign_ingest(&key, 1_800_000_000, [0xab; 16], raw).unwrap();
        assert_ne!(signed.signature.expose(), retry.signature.expose());
    }

    #[test]
    fn enforces_timestamp_grammar_and_rejects_empty_keys() {
        let key = Secret::new(String::from("offline signing fixture"));
        for timestamp in [1, MAX_TIMESTAMP] {
            let signed = sign_ingest(&key, timestamp, [1; 16], b"{}").unwrap();
            assert_eq!(signed.timestamp, timestamp.to_string());
        }
        for timestamp in [-1, 0, MAX_TIMESTAMP + 1] {
            assert!(sign_ingest(&key, timestamp, [1; 16], b"{}").is_none());
        }
        assert!(sign_ingest(&Secret::new(String::new()), 1, [1; 16], b"{}").is_none());
    }
}
