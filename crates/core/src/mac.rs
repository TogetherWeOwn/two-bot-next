//! Moderation-audit MAC correlation (TOG-9810 S5).
//!
//! Ports legacy `src/audit/moderationIdentity.ts` exactly: the token scheme,
//! the marker MAC, the `[two-audit:v1:…]` reason marker, and the parser, plus
//! the correlated-action helpers from `src/audit/discordEvents.ts`
//! (`isChannelAction`, `outcomeFor`).
//!
//! Threat model (legacy TOG-2223 #8, preserved): the token alone is a one-way
//! hash of `(guild_id, idempotency_key)`, never a signature over the visible
//! marker fields — anything holding the bot's Discord token can already set
//! an arbitrary `X-Audit-Log-Reason` on a same-bot action without calling
//! this module, so a syntactically valid token/action/actor proves nothing.
//! The MAC is keyed on `TWO_MODERATION_AUDIT_SECRET`, which those callers
//! never see — only the in-process moderation service mints and verifies —
//! so a forged marker fails verification even though the forger holds the
//! same bot credential.
//!
//! Without a secret every function degrades honestly: minting returns the
//! reason unchanged, parsing returns `None` (legacy `if (!secret)` guards).

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::moderation::ModerationAction;

/// Extra correlated action the legacy parser accepts beyond the nine service
/// verbs: a scheduled unban firing carries the same marker shape.
pub const UNBAN_SCHEDULED_ACTION: &str = "moderation.unban_scheduled";

/// Marker prefix (legacy `[two-audit:v1:`).
pub const MARKER_PREFIX: &str = "[two-audit:v1:";
/// Marker version tag.
pub const MARKER_VERSION: &str = "v1";

/// One-way token binding a moderation attempt to its audit row (legacy
/// `moderationAuditToken`): first 32 hex chars of
/// `sha256("moderation-audit:v1:{guild_id}:{idempotency_key}")`.
#[must_use]
pub fn moderation_audit_token(guild_id: &str, idempotency_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("moderation-audit:v1:{guild_id}:{idempotency_key}").as_bytes());
    hex::encode(hasher.finalize())[..32].to_owned()
}

/// Audit-row entry id for a correlated moderation success (legacy
/// `moderationAuditEntryId`): `moderation-success:{guild_id}:{token}`.
#[must_use]
pub fn moderation_audit_entry_id(guild_id: &str, token: &str) -> String {
    format!("moderation-success:{guild_id}:{token}")
}

/// Marker MAC (legacy `markerMac`): first 16 hex chars of
/// `hmac_sha256(secret, "moderation-audit-mac:v1:{guild}:{token}:{action}:{actor}")`.
fn marker_mac(secret: &str, guild_id: &str, token: &str, action: &str, actor_id: &str) -> String {
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(
        format!("moderation-audit-mac:v1:{guild_id}:{token}:{action}:{actor_id}").as_bytes(),
    );
    hex::encode(mac.finalize().into_bytes())[..16].to_owned()
}

/// All auditable moderation actions: the nine service verbs plus the
/// scheduled-unban marker (legacy `MODERATION_ACTIONS` +
/// `'moderation.unban_scheduled'`).
#[must_use]
pub fn is_auditable_action(action: &str) -> bool {
    action == UNBAN_SCHEDULED_ACTION
        || ModerationAction::ALL
            .iter()
            .any(|a| a.action_name() == action)
}

/// Mint the audit-log reason marker (legacy `moderationAuditReason`).
/// With no secret the reason passes through unchanged — correlation is
/// impossible, and the row falls back to the plain `discord-audit:` entry id.
#[must_use]
pub fn moderation_audit_reason(
    secret: Option<&str>,
    guild_id: &str,
    idempotency_key: &str,
    action: &str,
    actor_id: &str,
    reason: &str,
) -> String {
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return reason.to_owned();
    };
    let token = moderation_audit_token(guild_id, idempotency_key);
    let mac = marker_mac(secret, guild_id, &token, action, actor_id);
    format!("[two-audit:v1:{token}:{action}:{actor_id}:{mac}] {reason}")
}

/// A verified marker: the correlated service action and the acting moderator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModerationMarker {
    pub token: String,
    pub action: String,
    pub actor_id: String,
}

/// Verify and parse a reason marker (legacy `parseModerationAuditReason`).
/// Returns `None` without a secret, on any shape deviation, for unknown
/// actions, or when the MAC does not verify — the caller treats all of those
/// as uncorrelated rows.
#[must_use]
pub fn parse_moderation_audit_reason(
    secret: Option<&str>,
    guild_id: &str,
    reason: Option<&str>,
) -> Option<ModerationMarker> {
    let secret = secret.filter(|s| !s.is_empty())?;
    let reason = reason?;
    let inner = reason.strip_prefix(MARKER_PREFIX)?;
    let end = inner.find(']')?;
    // The marker is `[…]` followed by a space or end of string (legacy
    // `(?: |$)`); anything else is a different bracketed text, not a marker.
    if !inner[end + 1..].is_empty() && !inner[end + 1..].starts_with(' ') {
        return None;
    }
    let mut parts = inner[..end].split(':');
    let (token, action, actor_id, mac) = match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some(token), Some(action), Some(actor), Some(mac), None) => (token, action, actor, mac),
        _ => return None,
    };
    if !is_hex_lower(token, 32) || !is_hex_lower(mac, 16) {
        return None;
    }
    if !is_marker_action(action) || !is_snowflake(actor_id) {
        return None;
    }
    if !is_auditable_action(action) {
        return None;
    }
    let expected = marker_mac(secret, guild_id, token, action, actor_id);
    if !signatures_match(&expected, mac) {
        return None;
    }
    Some(ModerationMarker {
        token: token.to_owned(),
        action: action.to_owned(),
        actor_id: actor_id.to_owned(),
    })
}

/// Legacy marker action shape: `moderation.` followed by lowercase/underscore
/// (regex `moderation\.[a-z_]+`).
fn is_marker_action(action: &str) -> bool {
    let rest = action.strip_prefix("moderation.").unwrap_or_default();
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

/// Discord snowflake shape: 17–20 ASCII digits (legacy `\d{17,20}`).
fn is_snowflake(id: &str) -> bool {
    (17..=20).contains(&id.len()) && id.chars().all(|c| c.is_ascii_digit())
}

fn is_hex_lower(s: &str, len: usize) -> bool {
    s.len() == len
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Constant-time comparison after the parser validates the public shape:
/// both strings contain exactly 16 lowercase hex characters.
fn signatures_match(a_hex: &str, b_hex: &str) -> bool {
    bool::from(a_hex.as_bytes().ct_eq(b_hex.as_bytes()))
}

/// True for the channel-targeted verbs, whose audit-log target is the channel
/// (legacy `isChannelAction`): purge, slowmode, lockdown, unlock.
#[must_use]
pub fn is_channel_action(action: &str) -> bool {
    matches!(
        action,
        "moderation.purge" | "moderation.slowmode" | "moderation.lockdown" | "moderation.unlock"
    )
}

/// Human outcome for a correlated action (legacy `outcomeFor`).
#[must_use]
pub fn outcome_for(action: &str) -> &'static str {
    match action {
        "moderation.ban" => "banned",
        "moderation.tempban" => "temporarily_banned",
        "moderation.kick" => "kicked",
        "moderation.timeout" => "timed_out",
        "moderation.warn" => "warned",
        "moderation.purge" => "purged",
        "moderation.slowmode" => "slowmode_updated",
        "moderation.lockdown" => "locked_down",
        "moderation.unlock" => "unlocked",
        _ => "unbanned",
    }
}

/// Load the MAC secret (legacy `moderationAuditSecret` from
/// `src/moderation/config.ts` via `readSecret('moderation_audit_secret',
/// ['TWO_MODERATION_AUDIT_SECRET'])`: systemd credential file first, env
/// fallback).
///
/// `credential_dir` is the systemd credential directory when provisioned
/// (`source.dir`); `vars` is the env map. Credential-file values trim like
/// legacy `readFileSync(...).trim()`. Nonempty env values retain their exact
/// bytes, including padding and whitespace-only keys. A non-ENOENT credential read
/// error surfaces as [`SecretError::CredentialUnreadable`] without the path
/// contents (legacy never prints them either).
///
/// Empty/absent everywhere means unconfigured — minting degrades to plain
/// reasons and parsing returns `None`.
pub fn moderation_audit_secret(
    vars: &std::collections::HashMap<String, String>,
    credential_dir: Option<&std::path::Path>,
) -> Result<Option<String>, SecretError> {
    if let Some(dir) = credential_dir {
        match std::fs::read_to_string(dir.join("moderation_audit_secret")) {
            Ok(raw) => {
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    return Ok(Some(trimmed.to_owned()));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(SecretError::CredentialUnreadable(err.kind().to_string())),
        }
    }
    Ok(vars
        .get("TWO_MODERATION_AUDIT_SECRET")
        .filter(|s| !s.is_empty())
        .cloned())
}

/// A credential file that exists but cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// The file exists but could not be read; carries the IO error kind only,
    /// never the path contents.
    #[error("Credential \"moderation_audit_secret\" exists but could not be read ({0}).")]
    CredentialUnreadable(String),
}

#[cfg(test)]
#[derive(serde::Deserialize)]
pub(crate) struct ModerationTestVector {
    pub secret: String,
    pub reason: String,
}

/// Public, non-production Node crypto vectors shared by MAC/classifier tests.
#[cfg(test)]
pub(crate) fn moderation_test_vectors() -> Vec<ModerationTestVector> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/moderation-mac.json");
    let json = std::fs::read_to_string(path).expect("public MAC vector fixture");
    serde_json::from_str(&json).expect("valid MAC vectors")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: &str = "123456789012345678";
    const ACTOR: &str = "987654321098765432";

    #[test]
    fn token_is_stable_and_hex() {
        let a = moderation_audit_token(GUILD, "key-1");
        assert_eq!(a, moderation_audit_token(GUILD, "key-1"));
        assert_eq!(a.len(), 32);
        assert!(is_hex_lower(&a, 32));
        assert_ne!(a, moderation_audit_token(GUILD, "key-2"));
        assert_ne!(a, moderation_audit_token("other-guild", "key-1"));
    }

    #[test]
    fn marker_matches_independent_node_crypto_vector() {
        // node:crypto createHash/createHmac; legacy moderationIdentity.ts.
        let vector = moderation_test_vectors().remove(0);
        let secret = vector.secret;
        let golden = vector.reason.as_str();
        assert_eq!(
            moderation_audit_reason(
                Some(&secret),
                GUILD,
                "idem-1",
                "moderation.ban",
                ACTOR,
                "spam"
            ),
            golden
        );
        assert!(parse_moderation_audit_reason(Some(&secret), GUILD, Some(golden)).is_some());
        let marker_only = golden.split("] ").next().unwrap().to_owned() + "]";
        assert!(parse_moderation_audit_reason(Some(&secret), GUILD, Some(&marker_only)).is_some());
        assert!(
            parse_moderation_audit_reason(Some(&secret), GUILD, Some(&(marker_only + "x")))
                .is_none()
        );
        for changed in [
            golden.replace("moderation.ban", "moderation.kick"),
            golden.replace(ACTOR, "987654321098765433"),
        ] {
            assert!(parse_moderation_audit_reason(Some(&secret), GUILD, Some(&changed)).is_none());
        }
        let empty_secret = String::new();
        assert_eq!(
            moderation_audit_reason(
                Some(&empty_secret),
                GUILD,
                "k",
                "moderation.ban",
                ACTOR,
                "plain"
            ),
            "plain"
        );
        assert!(parse_moderation_audit_reason(Some(&empty_secret), GUILD, Some(golden)).is_none());
    }

    #[test]
    fn mint_then_parse_round_trip() {
        let secret = moderation_test_vectors().remove(0).secret;
        for action in ModerationAction::ALL.iter().map(|a| a.action_name()) {
            let reason =
                moderation_audit_reason(Some(&secret), GUILD, "idem-1", action, ACTOR, "spam");
            let marker = parse_moderation_audit_reason(Some(&secret), GUILD, Some(&reason))
                .expect("must verify");
            assert_eq!(marker.action, action);
            assert_eq!(marker.actor_id, ACTOR);
            assert_eq!(
                moderation_audit_entry_id(GUILD, &marker.token),
                format!("moderation-success:{GUILD}:{}", marker.token)
            );
        }
        // The scheduled-unban marker parses too.
        let reason = moderation_audit_reason(
            Some(&secret),
            GUILD,
            "idem-u",
            UNBAN_SCHEDULED_ACTION,
            ACTOR,
            "timer",
        );
        let marker = parse_moderation_audit_reason(Some(&secret), GUILD, Some(&reason))
            .expect("unban_scheduled must verify");
        assert_eq!(marker.action, UNBAN_SCHEDULED_ACTION);
    }

    #[test]
    fn no_secret_degrades_honestly() {
        let reason = moderation_audit_reason(None, GUILD, "k", "moderation.ban", ACTOR, "plain");
        assert_eq!(reason, "plain");
        assert_eq!(
            parse_moderation_audit_reason(None, GUILD, Some("[two-audit:v1:x] y")),
            None
        );
    }

    #[test]
    fn forgeries_and_mismatches_rejected() {
        let secret = moderation_test_vectors().remove(0).secret;
        let wrong_secret = format!("{secret}-wrong");
        let reason = moderation_audit_reason(
            Some(&secret),
            GUILD,
            "idem-1",
            "moderation.ban",
            ACTOR,
            "spam",
        );
        // Wrong secret.
        assert_eq!(
            parse_moderation_audit_reason(Some(&wrong_secret), GUILD, Some(&reason)),
            None
        );
        // Wrong guild.
        assert_eq!(
            parse_moderation_audit_reason(Some(&secret), "111111111111111111", Some(&reason)),
            None
        );
        // Tampered MAC nibble.
        let mut tampered = reason.clone();
        let mac_start = tampered.find("987654321098765432:").unwrap() + 19;
        tampered.replace_range(
            mac_start..mac_start + 1,
            if &tampered[mac_start..mac_start + 1] == "a" {
                "b"
            } else {
                "a"
            },
        );
        assert_eq!(
            parse_moderation_audit_reason(Some(&secret), GUILD, Some(&tampered)),
            None
        );
        // Unknown action, short actor, missing trailing space discipline.
        assert_eq!(
            parse_moderation_audit_reason(Some(&secret), GUILD, Some("[two-audit:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:moderation.nuke:987654321098765432:bbbbbbbbbbbbbbbb] x")),
            None
        );
        assert_eq!(
            parse_moderation_audit_reason(Some(&secret), GUILD, Some("[two-audit:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:moderation.ban:123:bbbbbbbbbbbbbbbb] x")),
            None
        );
        assert_eq!(
            parse_moderation_audit_reason(Some(&secret), GUILD, Some("no marker")),
            None
        );
        assert_eq!(
            parse_moderation_audit_reason(Some(&secret), GUILD, None),
            None
        );
    }

    #[test]
    fn channel_actions_and_outcomes_match_legacy() {
        for action in [
            "moderation.purge",
            "moderation.slowmode",
            "moderation.lockdown",
            "moderation.unlock",
        ] {
            assert!(is_channel_action(action), "{action} targets the channel");
        }
        assert!(!is_channel_action("moderation.ban"));
        assert_eq!(outcome_for("moderation.ban"), "banned");
        assert_eq!(outcome_for("moderation.tempban"), "temporarily_banned");
        assert_eq!(outcome_for("moderation.timeout"), "timed_out");
        assert_eq!(outcome_for("moderation.unban_scheduled"), "unbanned");
    }

    #[test]
    fn secret_loader_preserves_env_bytes_and_legacy_macs() {
        for vector in moderation_test_vectors() {
            let vars = [(
                "TWO_MODERATION_AUDIT_SECRET".to_owned(),
                vector.secret.clone(),
            )]
            .into();
            let loaded = moderation_audit_secret(&vars, None).expect("reads");
            assert_eq!(loaded.as_deref(), Some(vector.secret.as_str()));
            assert_eq!(
                moderation_audit_reason(
                    loaded.as_deref(),
                    GUILD,
                    "idem-1",
                    "moderation.ban",
                    ACTOR,
                    "spam"
                ),
                vector.reason
            );
            assert!(
                parse_moderation_audit_reason(loaded.as_deref(), GUILD, Some(&vector.reason))
                    .is_some()
            );
        }
        let vars = [("TWO_MODERATION_AUDIT_SECRET".to_owned(), String::new())].into();
        assert_eq!(moderation_audit_secret(&vars, None).expect("reads"), None);
        assert_eq!(
            moderation_audit_secret(&Default::default(), None).expect("reads"),
            None
        );
    }

    #[test]
    fn secret_loader_trims_file_and_surfaces_read_failure() {
        let vectors = moderation_test_vectors();
        let base = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("mac-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("moderation_audit_secret");
        std::fs::write(&path, format!("{}\n", vectors[1].secret)).expect("write");
        let vars = [(
            "TWO_MODERATION_AUDIT_SECRET".to_owned(),
            vectors[2].secret.clone(),
        )]
        .into();
        // Credential file wins over env, trimmed.
        assert_eq!(
            moderation_audit_secret(&vars, Some(&dir)).expect("reads"),
            Some(vectors[0].secret.clone())
        );
        // Missing and blank credential files retain the raw environment value.
        std::fs::write(&path, &vectors[2].secret).expect("blank fixture");
        assert_eq!(
            moderation_audit_secret(&vars, Some(&dir)).expect("reads"),
            Some(vectors[2].secret.clone())
        );
        std::fs::remove_file(&path).expect("remove fixture");
        assert_eq!(
            moderation_audit_secret(&vars, Some(&dir)).expect("reads"),
            Some(vectors[2].secret.clone())
        );
        // A credential path that exists but is not readable as a file must
        // fail; never substitute the otherwise usable environment value.
        std::fs::create_dir(&path).expect("directory fixture");
        assert!(matches!(
            moderation_audit_secret(&vars, Some(&dir)),
            Err(SecretError::CredentialUnreadable(_))
        ));
        std::fs::remove_dir_all(&dir).expect("remove fixtures");
    }
}
