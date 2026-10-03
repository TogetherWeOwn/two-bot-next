//! Moderation MAC marker acceptance (TOG-12697).
//!
//! Parity: the legacy moderation-audit token + HMAC reason markers
//! (`moderationAuditToken`, `moderationAuditReason`,
//! `parseModerationAuditReason`; see `docs/audit-core.md`
//! `two_bot_core::mac` and `docs/moderation-mac-acceptance.md`).
//!
//! Pins, through the existing public `mac` API only: the token is a stable
//! 32-hex one-way binding of `(guild_id, idempotency_key)`; minting without
//! a secret passes the reason through unchanged and parsing returns `None`;
//! a minted marker verifies while tampered action/actor/token/mac do not;
//! over-long reasons truncate the human suffix to the 512 UTF-16 budget
//! ending in an ellipsis without splitting a scalar; `is_auditable_action`
//! covers the nine service verbs plus `moderation.unban_scheduled` and
//! refuses unknown verbs. Secrets come from the public non-production
//! vectors in `tests/fixtures/moderation-mac.json`; no operational key is
//! embedded.
//!
//! Synthetic fixtures only: no Discord, network, or database.

use two_bot_core::mac::{
    is_auditable_action, moderation_audit_reason, moderation_audit_token,
    parse_moderation_audit_reason, AUDIT_REASON_MAX_UTF16, UNBAN_SCHEDULED_ACTION,
};
use two_bot_core::moderation::ModerationAction;

const GUILD: &str = "123456789012345678";
const OTHER_GUILD: &str = "999999999999999999";
const ACTOR: &str = "987654321098765432";
const KEY: &str = "idem-acceptance-1";

/// Public, non-production Node crypto vectors shared with the inline MAC
/// tests (`secret` + independently minted `reason`).
fn fixture_vectors() -> Vec<(String, String)> {
    let json: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/moderation-mac.json"))
            .expect("public MAC vector fixture");
    json.as_array()
        .expect("vector list")
        .iter()
        .map(|entry| {
            (
                entry["secret"].as_str().expect("secret").to_owned(),
                entry["reason"].as_str().expect("reason").to_owned(),
            )
        })
        .collect()
}

fn fixture_secret() -> String {
    fixture_vectors().remove(0).0
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

#[test]
fn token_is_stable_32_hex_one_way_binding() {
    let token = moderation_audit_token(GUILD, KEY);
    assert_eq!(token, moderation_audit_token(GUILD, KEY));
    assert_eq!(token.len(), 32);
    assert!(
        token
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "lowercase hex only: {token}"
    );
    // Bound to both inputs: a different guild or key mints a different token.
    assert_ne!(token, moderation_audit_token(GUILD, "idem-acceptance-2"));
    assert_ne!(token, moderation_audit_token(OTHER_GUILD, KEY));
    // One-way: the preimage inputs are not recoverable from the token.
    assert!(!token.contains(GUILD));
    assert!(!token.contains(KEY));
}

#[test]
fn no_secret_passes_through_and_parses_none() {
    let plain = "plain reason without marker";
    // Empty secret built at runtime, as in the inline `mac` tests.
    let empty_secret = String::new();
    assert_eq!(
        moderation_audit_reason(None, GUILD, KEY, "moderation.ban", ACTOR, plain),
        plain
    );
    assert_eq!(
        moderation_audit_reason(
            Some(empty_secret.as_str()),
            GUILD,
            KEY,
            "moderation.ban",
            ACTOR,
            plain
        ),
        plain
    );
    assert_eq!(
        parse_moderation_audit_reason(None, GUILD, Some(plain)),
        None
    );
    assert_eq!(
        parse_moderation_audit_reason(Some(empty_secret.as_str()), GUILD, Some(plain)),
        None
    );
    // Even a well-formed marker is uncorrelated without a secret.
    let secret = fixture_secret();
    let minted = moderation_audit_reason(Some(&secret), GUILD, KEY, "moderation.ban", ACTOR, plain);
    assert_eq!(
        parse_moderation_audit_reason(None, GUILD, Some(&minted)),
        None
    );
    assert_eq!(
        parse_moderation_audit_reason(Some(empty_secret.as_str()), GUILD, Some(&minted)),
        None
    );
}

#[test]
fn minted_marker_verifies_and_tampering_fails() {
    let vectors = fixture_vectors();
    let (secret, golden) = &vectors[0];
    // The public Node vector pins the exact minted wire form.
    let minted = moderation_audit_reason(
        Some(secret),
        GUILD,
        "idem-1",
        "moderation.ban",
        ACTOR,
        "spam",
    );
    assert_eq!(&minted, golden);
    let marker = parse_moderation_audit_reason(Some(secret), GUILD, Some(&minted))
        .expect("minted marker verifies");
    assert_eq!(marker.token, moderation_audit_token(GUILD, "idem-1"));
    assert_eq!(marker.action, "moderation.ban");
    assert_eq!(marker.actor_id, ACTOR);
    // The fixture reason parses as-is, without re-minting.
    assert!(parse_moderation_audit_reason(Some(secret), GUILD, Some(golden)).is_some());

    let marker_only = minted.split("] ").next().expect("marker").to_owned() + "]";
    let suffixed = format!("{marker_only} trailing prose");
    // Tampering with any one authenticated field breaks verification; the
    // replacements keep the public shape so each case exercises the MAC.
    let tampered_action = marker_only.replace("moderation.ban", "moderation.kick");
    let tampered_actor = marker_only.replace(ACTOR, "987654321098765433");
    // Flip the first token nibble whatever it is; shape stays valid 32-hex.
    let token_start = marker_only.find("v1:").expect("version") + 3;
    let mut tampered_token = marker_only.clone();
    let token_replacement = if &tampered_token[token_start..token_start + 1] == "a" {
        "b"
    } else {
        "a"
    };
    tampered_token.replace_range(token_start..token_start + 1, token_replacement);
    let mac_start = marker_only.rfind(':').expect("mac field") + 1;
    let mut tampered_mac = marker_only.clone();
    let replacement = if &tampered_mac[mac_start..mac_start + 1] == "a" {
        "b"
    } else {
        "a"
    };
    tampered_mac.replace_range(mac_start..mac_start + 1, replacement);
    assert_ne!(tampered_token, marker_only);
    for (field, tampered) in [
        ("action", tampered_action),
        ("actor", tampered_actor),
        ("token", tampered_token),
        ("mac", tampered_mac),
    ] {
        assert_eq!(
            parse_moderation_audit_reason(Some(secret), GUILD, Some(&tampered)),
            None,
            "tampered {field} must not verify"
        );
    }
    // Untouched marker prose stays correlated; a different guild does not.
    assert!(parse_moderation_audit_reason(Some(secret), GUILD, Some(&suffixed)).is_some());
    assert_eq!(
        parse_moderation_audit_reason(Some(secret), OTHER_GUILD, Some(&minted)),
        None
    );
    // A secret that was never used to mint fails verification.
    let wrong_secret = format!("{secret}-wrong");
    assert_eq!(
        parse_moderation_audit_reason(Some(&wrong_secret), GUILD, Some(&minted)),
        None
    );
}

#[test]
fn over_long_reasons_truncate_suffix_within_budget() {
    let secret = fixture_secret();
    let mint = |human: &str| {
        moderation_audit_reason(Some(&secret), GUILD, KEY, "moderation.ban", ACTOR, human)
    };
    let marker_len = utf16_len(&mint(""));
    let budget = AUDIT_REASON_MAX_UTF16 - marker_len;
    assert!(budget < AUDIT_REASON_MAX_UTF16);

    // Exact boundary passes through unchanged.
    assert_eq!(
        mint(&"x".repeat(budget)).split("] ").nth(1),
        Some("x".repeat(budget).as_str())
    );
    // One unit over shortens to exactly the budget, ending in an ellipsis.
    let one_over = mint(&"x".repeat(budget + 1));
    assert_eq!(utf16_len(&one_over), AUDIT_REASON_MAX_UTF16);
    let suffix = one_over.split("] ").nth(1).expect("suffix");
    assert!(suffix.ends_with('\u{2026}'));
    assert!("x"
        .repeat(budget + 1)
        .starts_with(suffix.trim_end_matches('\u{2026}')));

    // Long ASCII and multi-byte/astral suffixes all fit, keep a whole-scalar
    // prefix of the human text, and leave the marker verifiable.
    for human in [
        "y".repeat(512),
        "\u{1F600}".repeat(256),
        "\u{00E9}".repeat(512),
        format!("spam \u{1F600} {}", "z".repeat(500)),
    ] {
        let out = mint(&human);
        assert!(
            utf16_len(&out) <= AUDIT_REASON_MAX_UTF16,
            "over budget: {}",
            utf16_len(&out)
        );
        let suffix = out.split("] ").nth(1).expect("suffix");
        let kept = suffix.trim_end_matches('\u{2026}');
        assert!(human.starts_with(kept), "prefix intact: {suffix}");
        assert!(
            suffix.ends_with('\u{2026}'),
            "shortened suffix ends in ellipsis: {suffix}"
        );
        let marker = parse_moderation_audit_reason(Some(&secret), GUILD, Some(&out))
            .expect("marker intact after truncation");
        assert_eq!(marker.token, moderation_audit_token(GUILD, KEY));
    }
    // Short reasons are untouched.
    assert_eq!(mint("spam").split("] ").nth(1), Some("spam"));
}

#[test]
fn auditable_actions_cover_service_verbs_plus_scheduled_unban() {
    for action in ModerationAction::ALL.iter().map(|a| a.action_name()) {
        assert!(is_auditable_action(action), "{action} is auditable");
    }
    assert_eq!(ModerationAction::ALL.len(), 9);
    assert!(is_auditable_action(UNBAN_SCHEDULED_ACTION));
    assert_eq!(UNBAN_SCHEDULED_ACTION, "moderation.unban_scheduled");
    for unknown in [
        "moderation.nuke",
        "moderation.unban",
        "moderation.BAN",
        "moderation.ban ",
        "ban",
        "",
    ] {
        assert!(!is_auditable_action(unknown), "{unknown} is not auditable");
    }
}
