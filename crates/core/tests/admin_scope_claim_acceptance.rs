//! Scope-claim signing pins for slice 1 of the approved admin API spec,
//! section 3.2 (public synthetic fixtures only).
//!
//! This file is the GREEN side of the acceptance pair: every test here passes
//! before and after the slice-1 build. It pins what the Builder must NOT
//! change (the v1 envelope, byte for byte) and carries the golden
//! scope-claim vectors both implementations sign against. The RED side — the
//! tamper matrix, the fail-closed unsigned-claim refusal, and the burn/audit
//! order — lives in `crates/bot/tests/admin_scope_claim_acceptance.rs`,
//! which drives the real receiver over HTTP.
//!
//! Pinned wire contract (the Builder honors or renegotiates on the
//! acceptance card):
//!
//! ```text
//! two-internal-action/scope-claim/v1
//! POST
//! /internal/actions
//! {timestamp}
//! {nonce}
//! {sha256_hex(body)}
//! {scope}
//! {guild}
//! ```
//!
//! Eight LF-joined lines, no trailing LF. `scope` is one of
//! `read|moderate|admin`; `guild` is a 1-20 digit snowflake. The signature
//! marker stays `sha256=` (it names the primitive, truthfully); the scheme
//! is selected by the presence of the `x-two-scope` + `x-two-guild`
//! envelope headers. Domain separation from v1 comes from the tag line, as
//! in `docs/internal-action-signing-v2.md`. There is deliberately NO
//! idempotency-key line: `Idempotency-Key` stays outside the MAC, unchanged
//! from v1 (the one v2 gap slice 1 explicitly does NOT close).
//!
//! Synthetic fixtures only: no Discord, network, or database.

use hmac::{KeyInit as _, Mac as _};
use two_bot_core::internal_actions::{body_hash, canonical_string, parse_keys, sign, KeyRing};

const SCOPE_TAG: &str = "two-internal-action/scope-claim/v1";
const ACTIONS_PATH: &str = "/internal/actions";

/// The exact canonical layout above, over caller-supplied claim lines. The
/// Builder's signer must produce byte-identical output for these inputs
/// (proven by `scope_claim_goldens_recompute_exactly`).
fn scoped_canonical(timestamp: &str, nonce: &str, raw: &[u8], scope: &str, guild: &str) -> String {
    format!(
        "{SCOPE_TAG}\nPOST\n{ACTIONS_PATH}\n{timestamp}\n{nonce}\n{}\n{scope}\n{guild}",
        body_hash(raw)
    )
}

fn scope_fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/admin-scope-claim-signing.json"))
        .expect("scope-claim vector fixture")
}

fn v1_fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/internal-action-signing.json"))
        .expect("v1 vector fixture")
}

#[test]
fn v1_golden_vectors_still_verify_through_the_existing_api() {
    // The frozen v1 vectors keep verifying through the unchanged public API:
    // slice 1 adds a scheme next to v1, it never edits v1.
    let fixture = v1_fixture();
    let vectors = fixture["vectors"].as_array().expect("v1 vector list");
    assert!(!vectors.is_empty(), "v1 fixture must carry vectors");
    for (index, entry) in vectors.iter().enumerate() {
        let secret = entry["secret"].as_str().expect("secret");
        let ring = KeyRing::new(parse_keys(&format!("acceptance-key:{secret}")).expect("keys"));
        let body = entry["body"].as_str().expect("body");
        let timestamp = entry["timestamp"].as_str().expect("timestamp");
        let nonce = entry["nonce"].as_str().expect("nonce");
        let signature = entry["signature"].as_str().expect("signature");
        assert_eq!(
            entry["body_hash"].as_str().expect("body_hash"),
            body_hash(body.as_bytes()),
            "vector {index}: body digest is stable"
        );
        assert!(
            ring.verify(
                "acceptance-key",
                signature,
                timestamp,
                nonce,
                body.as_bytes()
            ),
            "vector {index}: frozen v1 signature must verify"
        );
        assert!(
            !ring.verify(
                "acceptance-unknown",
                signature,
                timestamp,
                nonce,
                body.as_bytes()
            ),
            "vector {index}: unknown key id must refuse against the decoy"
        );
    }
}

#[test]
fn v1_tamper_matrix_still_refuses_each_mutation() {
    // The tamper-detection property slice 1 must replicate for scope claims,
    // proven here against v1: flipping any signed input refuses, while the
    // intact vector verifies. Matrix technique shared with the HTTP suite.
    let entry = v1_fixture()["vectors"][0].clone();
    let secret = entry["secret"].as_str().expect("secret").to_owned();
    let ring = KeyRing::new(parse_keys(&format!("acceptance-key:{secret}")).expect("keys"));
    let body = entry["body"].as_str().expect("body").to_owned();
    let timestamp = entry["timestamp"].as_str().expect("timestamp").to_owned();
    let nonce = entry["nonce"].as_str().expect("nonce").to_owned();
    let signature = entry["signature"].as_str().expect("signature").to_owned();
    assert!(
        ring.verify(
            "acceptance-key",
            &signature,
            &timestamp,
            &nonce,
            body.as_bytes()
        ),
        "intact v1 vector verifies (control)"
    );
    let mut tampered_body = body.clone();
    tampered_body.push(' ');
    let cases = [
        (
            "intact control",
            body.clone(),
            timestamp.clone(),
            nonce.clone(),
        ),
        ("body byte", tampered_body, timestamp.clone(), nonce.clone()),
        (
            "timestamp",
            body.clone(),
            format!("{timestamp}0"),
            nonce.clone(),
        ),
        (
            "nonce",
            body.clone(),
            timestamp.clone(),
            format!("{nonce}0"),
        ),
    ];
    for (field, case_body, case_ts, case_nonce) in cases {
        // The first row re-verifies the intact vector inside the matrix so a
        // broken harness (all-refuse) cannot pass as tamper detection.
        let intact = case_body == body && case_ts == timestamp && case_nonce == nonce;
        assert_eq!(
            ring.verify(
                "acceptance-key",
                &signature,
                &case_ts,
                &case_nonce,
                case_body.as_bytes()
            ),
            intact,
            "v1 tamper matrix: {field} mutation must refuse, intact must verify"
        );
    }
    // A signature minted under another secret never verifies here.
    let other = sign(
        b"distinct-test-secret-00000000000000",
        &timestamp,
        &nonce,
        body.as_bytes(),
    );
    assert!(
        !ring.verify(
            "acceptance-key",
            &other,
            &timestamp,
            &nonce,
            body.as_bytes()
        ),
        "cross-secret signature must refuse"
    );
}

#[test]
fn v1_canonical_bytes_are_frozen() {
    // The v1 canonical string is byte-frozen: the Builder's extension adds
    // lines to the new scheme only and never reorders v1.
    let entry = v1_fixture()["vectors"][0].clone();
    let body = entry["body"].as_str().expect("body");
    let timestamp = entry["timestamp"].as_str().expect("timestamp");
    let nonce = entry["nonce"].as_str().expect("nonce");
    let canonical = canonical_string(timestamp, nonce, body.as_bytes());
    let expected = format!(
        "POST\n{ACTIONS_PATH}\n{timestamp}\n{nonce}\n{}",
        entry["body_hash"].as_str().expect("body_hash")
    );
    assert_eq!(canonical, expected, "v1 canonical layout is frozen");
    assert_eq!(canonical.lines().count(), 5, "v1 keeps exactly five lines");
}

#[test]
fn scope_claim_goldens_recompute_exactly() {
    // Fixture integrity: the golden canonical strings and signatures
    // recompute from the fixture inputs through the pinned layout. This
    // guards the fixture against typos; the Builder cross-checks the Rust
    // verifier and the Worker signer against these same bytes.
    let fixture = scope_fixture();
    let layout = fixture["canonical_layout"]
        .as_array()
        .expect("layout list")
        .iter()
        .map(|line| line.as_str().expect("layout line").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(layout.len(), 8, "scope-claim canonical keeps eight lines");
    assert_eq!(layout[0], SCOPE_TAG, "line 1 is the domain tag");
    let vectors = fixture["vectors"].as_array().expect("vector list");
    assert_eq!(vectors.len(), 3, "one golden per scope tier");
    for (index, entry) in vectors.iter().enumerate() {
        let scope = entry["scope"].as_str().expect("scope");
        let guild = entry["guild"].as_str().expect("guild");
        let body = entry["body"].as_str().expect("body");
        let timestamp = entry["timestamp"].as_str().expect("timestamp");
        let nonce = entry["nonce"].as_str().expect("nonce");
        let secret = entry["secret"].as_str().expect("secret");
        assert!(
            ["read", "moderate", "admin"].contains(&scope),
            "vector {index}: scope is one of the three tiers"
        );
        assert!(
            !guild.is_empty() && guild.len() <= 20 && guild.bytes().all(|b| b.is_ascii_digit()),
            "vector {index}: guild is a snowflake"
        );
        assert!(
            secret.len() >= 32,
            "vector {index}: test secret meets the minimum"
        );
        let canonical = scoped_canonical(timestamp, nonce, body.as_bytes(), scope, guild);
        assert_eq!(
            canonical,
            entry["canonical"].as_str().expect("golden canonical"),
            "vector {index}: canonical recomputes byte-exact"
        );
        assert_eq!(
            entry["body_hash"].as_str().expect("golden digest"),
            body_hash(body.as_bytes()),
            "vector {index}: body digest recomputes"
        );
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
            .expect("HMAC accepts the test secret");
        mac.update(canonical.as_bytes());
        assert_eq!(
            format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
            entry["signature"].as_str().expect("golden signature"),
            "vector {index}: signature recomputes"
        );
    }
}

#[test]
fn scope_claim_canonical_covers_claims_and_body_but_no_idempotency_key() {
    // The MAC coverage pin: flipping one claim char, one guild digit, or one
    // body byte changes the canonical (so the tag refuses), while the
    // canonical never contains an idempotency-key line in any golden.
    let entry = scope_fixture()["vectors"][0].clone();
    let body = entry["body"].as_str().expect("body").to_owned();
    let timestamp = entry["timestamp"].as_str().expect("timestamp").to_owned();
    let nonce = entry["nonce"].as_str().expect("nonce").to_owned();
    let scope = entry["scope"].as_str().expect("scope").to_owned();
    let guild = entry["guild"].as_str().expect("guild").to_owned();
    let intact = scoped_canonical(&timestamp, &nonce, body.as_bytes(), &scope, &guild);
    assert_eq!(intact.lines().count(), 8, "eight lines, no more");
    assert!(intact.starts_with(SCOPE_TAG), "domain tag first");
    assert!(
        !scoped_canonical(&timestamp, &nonce, body.as_bytes(), "moderate", &guild).eq(&intact)
            && scoped_canonical(&timestamp, &nonce, body.as_bytes(), "moderate", &guild)
                .lines()
                .nth(6)
                == Some("moderate"),
        "scope is a covered line"
    );
    let mut flipped_guild = guild.clone();
    flipped_guild.pop();
    flipped_guild.push(if guild.ends_with('8') { '7' } else { '8' });
    assert_ne!(
        scoped_canonical(&timestamp, &nonce, body.as_bytes(), &scope, &flipped_guild),
        intact,
        "one guild digit flips the canonical"
    );
    assert_ne!(
        scoped_canonical(
            &timestamp,
            &nonce,
            format!("{body} ").as_bytes(),
            &scope,
            &guild
        ),
        intact,
        "one body byte flips the canonical"
    );
    for (index, golden) in scope_fixture()["vectors"]
        .as_array()
        .expect("vector list")
        .iter()
        .enumerate()
    {
        let canonical = golden["canonical"].as_str().expect("golden canonical");
        assert!(
            !canonical.to_ascii_lowercase().contains("idempotency"),
            "vector {index}: no idempotency-key line in the MAC"
        );
    }
}

#[test]
fn scope_claim_tag_domain_separates_from_v1() {
    // No byte string verifies under both schemes: the tag line means a v1
    // canonical can never equal a scope-claim canonical, so a tag cut for
    // one scheme never verifies as the other under one secret.
    let entry = v1_fixture()["vectors"][0].clone();
    let body = entry["body"].as_str().expect("body");
    let timestamp = entry["timestamp"].as_str().expect("timestamp");
    let nonce = entry["nonce"].as_str().expect("nonce");
    let v1 = canonical_string(timestamp, nonce, body.as_bytes());
    for golden in scope_fixture()["vectors"].as_array().expect("vector list") {
        assert_ne!(
            golden["canonical"].as_str().expect("golden canonical"),
            v1.as_str(),
            "golden scope-claim canonical is never a v1 canonical"
        );
    }
    assert!(
        !v1.starts_with(SCOPE_TAG),
        "v1 canonicals never carry the scope tag"
    );
}
