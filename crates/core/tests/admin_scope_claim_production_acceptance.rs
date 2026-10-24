//! Production signing/verifying goldens, not merely an in-test HMAC implementation.
//!
//! Baseline adapters keep this file compilable before the scoped APIs exist.
//! Rust resolves an inherent method before a trait method: the Builder adds
//! `SigningKey::sign_scoped` and `KeyRing::verify_scoped`, not test edits.
//! Fallback behavior is deliberately v1, so intact scoped vectors fail by
//! assertion on the old product rather than by an unresolved import.

use two_bot_core::internal_actions::{parse_keys, sign, AuthHeaders, KeyRing, SigningKey};

trait BaselineScopedSigning {
    fn sign_scoped(
        &self,
        timestamp: &str,
        nonce: &str,
        raw: &[u8],
        scope: &str,
        guild: &str,
    ) -> String;
}

impl BaselineScopedSigning for SigningKey {
    fn sign_scoped(
        &self,
        timestamp: &str,
        nonce: &str,
        raw: &[u8],
        _scope: &str,
        _guild: &str,
    ) -> String {
        sign(self.secret.expose(), timestamp, nonce, raw)
    }
}

trait BaselineScopedVerify {
    fn verify_scoped(
        &self,
        headers: &AuthHeaders<'_>,
        raw: &[u8],
        scope: &str,
        guild: &str,
    ) -> bool;
}

impl BaselineScopedVerify for KeyRing {
    fn verify_scoped(
        &self,
        headers: &AuthHeaders<'_>,
        raw: &[u8],
        _scope: &str,
        _guild: &str,
    ) -> bool {
        self.verify(
            headers.key_id,
            headers.signature,
            headers.timestamp,
            headers.nonce,
            raw,
        )
    }
}

fn vectors() -> Vec<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(include_str!(
        "fixtures/admin-scope-claim-signing.json"
    ))
    .expect("golden fixture")["vectors"]
        .as_array()
        .expect("golden vectors")
        .clone()
}

#[test]
fn production_scope_signer_matches_every_frozen_golden() {
    for entry in vectors() {
        let keys = parse_keys(&format!(
            "golden:{}",
            entry["secret"].as_str().expect("secret")
        ))
        .expect("fixture key");
        let key = &keys[0];
        let signature = key.sign_scoped(
            entry["timestamp"].as_str().expect("timestamp"),
            entry["nonce"].as_str().expect("nonce"),
            entry["body"].as_str().expect("body").as_bytes(),
            entry["scope"].as_str().expect("scope"),
            entry["guild"].as_str().expect("guild"),
        );
        assert_eq!(
            signature,
            entry["signature"].as_str().expect("signature"),
            "production signer must cover all eight lines, not silently use v1"
        );
    }
}

#[test]
fn production_scope_verifier_accepts_intact_goldens_and_rejects_each_mutation() {
    for entry in vectors() {
        let keys = parse_keys(&format!(
            "golden:{}",
            entry["secret"].as_str().expect("secret")
        ))
        .expect("fixture key");
        let ring = KeyRing::new(keys);
        let headers = AuthHeaders {
            key_id: "golden",
            signature: entry["signature"].as_str().expect("signature"),
            timestamp: entry["timestamp"].as_str().expect("timestamp"),
            nonce: entry["nonce"].as_str().expect("nonce"),
        };
        let raw = entry["body"].as_str().expect("body").as_bytes();
        let scope = entry["scope"].as_str().expect("scope");
        let guild = entry["guild"].as_str().expect("guild");
        assert!(
            ring.verify_scoped(&headers, raw, scope, guild),
            "intact production golden verifies"
        );
        for other_scope in ["read", "moderate", "admin"] {
            if other_scope != scope {
                assert!(
                    !ring.verify_scoped(&headers, raw, other_scope, guild),
                    "scope flip refuses"
                );
            }
        }
        assert!(
            !ring.verify_scoped(&headers, raw, scope, "223456789012345678"),
            "guild flip refuses"
        );
        let mut changed = raw.to_vec();
        changed.push(b' ');
        assert!(
            !ring.verify_scoped(&headers, &changed, scope, guild),
            "body flip refuses"
        );
        let changed_headers = AuthHeaders {
            key_id: "unknown",
            ..headers
        };
        assert!(
            !ring.verify_scoped(&changed_headers, raw, scope, guild),
            "unknown key refuses"
        );
        assert!(
            !ring.verify(
                "golden",
                headers.signature,
                headers.timestamp,
                headers.nonce,
                raw
            ),
            "scoped golden cannot verify as v1"
        );
    }
}

#[test]
fn baseline_adapters_are_v1_only_and_cannot_masquerade_as_scoped_apis() {
    let entry = &vectors()[0];
    let keys = parse_keys(&format!(
        "golden:{}",
        entry["secret"].as_str().expect("secret")
    ))
    .expect("fixture key");
    let raw = entry["body"].as_str().expect("body").as_bytes();
    let headers = AuthHeaders {
        key_id: "golden",
        signature: entry["signature"].as_str().expect("signature"),
        timestamp: entry["timestamp"].as_str().expect("timestamp"),
        nonce: entry["nonce"].as_str().expect("nonce"),
    };
    assert_ne!(
        BaselineScopedSigning::sign_scoped(
            &keys[0],
            headers.timestamp,
            headers.nonce,
            raw,
            "read",
            "123456789012345678"
        ),
        headers.signature
    );
    assert!(!BaselineScopedVerify::verify_scoped(
        &KeyRing::new(keys),
        &headers,
        raw,
        "read",
        "123456789012345678"
    ));
}
