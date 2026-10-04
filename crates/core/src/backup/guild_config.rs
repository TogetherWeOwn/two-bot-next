//! Sealed guild-config snapshot (TOG-3513).
//!
//! Port of legacy `src/redesign/guildConfig.ts`. A snapshot captures roles,
//! channels, overwrites, settings and emoji; the capture is sealed with the
//! sha256 of its canonical form, and restore refuses a sealed snapshot whose
//! content no longer matches. Pure and local: safe to run before any Discord
//! call, so a tampered backup is refused with zero writes.
//!
//! The canonical JSON encoding (`stable()`) matches legacy exactly: objects
//! with keys sorted lexicographically, `undefined` fields dropped, arrays in
//! order. Hashes of the same snapshot agree across the two implementations.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Accepted clean-slate spec (legacy `src/redesign/clean-slate.ts`, TOG-1317).
// ---------------------------------------------------------------------------

/// Owner-accepted server description (TOG-1317).
pub const SERVER_DESCRIPTION: &str = "An 18+ gaming clan since 1998. No application, no interview, no member number — join the Discord, play a session, and find out what it is like when people notice you came back.";

/// Owner role shape in the accepted spec.
pub fn owner_role() -> Value {
    serde_json::json!({
        "name": "Owner",
        "color": 0xd4af37,
        "hoist": true,
        "permissions": "0",
        "mentionable": false,
    })
}

/// Moderator role shape in the accepted spec.
pub fn moderator_role() -> Value {
    // Kick | Ban | ManageMessages | ReadMessageHistory | ManageRoles | ModerateMembers
    let permissions =
        (1u64 << 1) | (1u64 << 2) | (1u64 << 13) | (1u64 << 16) | (1u64 << 28) | (1u64 << 40);
    serde_json::json!({
        "name": "Moderator",
        "color": 0x5865f2,
        "hoist": true,
        "permissions": permissions.to_string(),
        "mentionable": false,
    })
}

pub const TEXT_CHANNEL_NAMES: &[&str] = &[
    "start-here",
    "announcements",
    "general",
    "looking-to-play",
    "discord-updates",
    "moderation-log",
    "audit-log",
    "voice-log",
];

pub const VOICE_CHANNEL_NAMES: &[&str] = &["Lobby", "Squad"];

/// (category name, channels) in accepted position order.
pub const CATEGORIES: &[(&str, &[&str])] = &[
    ("👋 START HERE", &["start-here", "announcements"]),
    ("💬 COMMUNITY", &["general", "looking-to-play"]),
    ("🔊 VOICE", &["Lobby", "Squad"]),
    (
        "🔒 OPERATIONS",
        &[
            "discord-updates",
            "moderation-log",
            "audit-log",
            "voice-log",
        ],
    ),
];

fn channel_topic(name: &str) -> Option<&'static str> {
    match name {
        "start-here" => Some("Four rules, then the server is yours. There is no application, no interview, and no quiz — this page is the only gate. Say hello in #general when you are ready."),
        "announcements" => Some("Important TWO news and scheduled events. Low-volume and read-only; if it is posted here, it matters."),
        "general" => Some("The shared table for games, life, questionable strategies, and introductions. New here? Say hello and tell us what you play — this is a place where people notice who comes back."),
        "looking-to-play" => Some("Finding a group should not require a spreadsheet, three bots, and divine intervention. Post the game, the platform if it matters, and your start time; claim a voice room when the party forms."),
        "discord-updates" => Some("Discord Community and platform notices. Internal record; no conversation."),
        "moderation-log" => Some("Screening, anti-raid, report, and moderation actions. Internal evidence; no conversation."),
        "audit-log" => Some("Channel, role, configuration, and retained-bot events. Internal evidence; no conversation."),
        "voice-log" => Some("Voice join, leave, and session telemetry used for community-health metrics. Internal evidence; no conversation."),
        _ => None,
    }
}

// Permission bits (legacy `clean-slate.ts`).
const VIEW_CHANNEL: u64 = 1 << 10;
const SEND_MESSAGES: u64 = 1 << 11;
const CONNECT: u64 = 1 << 20;
const SPEAK: u64 = 1 << 21;

/// The accepted @everyone overwrite for a channel (legacy `desiredEveryoneOverwrite`).
pub fn desired_everyone_overwrite(guild_id: &str, name: &str) -> Value {
    const OPERATIONS: &[&str] = &[
        "discord-updates",
        "moderation-log",
        "audit-log",
        "voice-log",
    ];
    const READ_ONLY: &[&str] = &["start-here", "announcements"];
    let (allow, deny) = if OPERATIONS.contains(&name) {
        (0, VIEW_CHANNEL)
    } else if READ_ONLY.contains(&name) {
        (VIEW_CHANNEL, SEND_MESSAGES)
    } else if VOICE_CHANNEL_NAMES.contains(&name) {
        (VIEW_CHANNEL | CONNECT | SPEAK, 0)
    } else {
        (VIEW_CHANNEL | SEND_MESSAGES, 0)
    };
    serde_json::json!({
        "id": guild_id,
        "type": 0,
        "allow": allow.to_string(),
        "deny": deny.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Identity guards (legacy `src/staging/spec.ts`).
// ---------------------------------------------------------------------------

/// The live TWO server. Named here only so we can refuse to touch it.
pub const LIVE_GUILD_ID: &str = "326474832151838730";

/// The live guild's expected name, a second irreversible-write guard.
pub const LIVE_GUILD_NAME: &str = "TogetherWeOwn";

/// The staging bot application (`Owen QA Test`). Public identifier, not a secret.
pub const STAGING_BOT_APPLICATION_ID: &str = "1469137636663758888";

/// The live bot application. A token for this app is refused, loudly.
pub const LIVE_BOT_APPLICATION_ID: &str = "1539711683898118154";

/// The superseded staging bot (`test-two`, until 2026-09-05): diagnosed, not feared.
pub const FORMER_STAGING_BOT_APPLICATION_ID: &str = "1537629682449649724";

/// The staging guild fixed by TOG-1309 for every parity slice.
pub const TWO_STAGING_GUILD_ID: &str = "1545644954272137297";

/// A bot token's first dot-separated segment is the base64 of the application
/// id. Accepts the optional `Bot ` prefix used by the Discord clients.
/// Returns `None` for anything not shaped like a bot token, so a caller
/// can tell "wrong bot" apart from "unparseable".
#[must_use]
pub fn application_id_from_token(token: &str) -> Option<String> {
    let token = token.trim();
    let token = token.strip_prefix("Bot ").unwrap_or(token);
    let seg = token.split('.').next()?;
    if seg.is_empty() {
        return None;
    }
    let decoded = base64_decode_standard(seg).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    if text.len() >= 15 && text.len() <= 25 && text.bytes().all(|b| b.is_ascii_digit()) {
        Some(text)
    } else {
        None
    }
}

fn base64_decode_standard(seg: &str) -> Result<Vec<u8>, ()> {
    // Minimal standard-base64 decoder (no new dependency for one call site).
    // Padding (`=`) is only valid as the last one or two chars of the input.
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if seg.is_empty() {
        return Err(());
    }
    // Tolerate missing padding, like Node's Buffer.from(seg, 'base64').
    let owned;
    let seg = if seg.len().is_multiple_of(4) {
        seg
    } else {
        owned = format!("{}{}", seg, "=".repeat(4 - seg.len() % 4));
        &owned
    };
    let bytes = seg.as_bytes();
    let pad = if bytes.ends_with(b"==") {
        2
    } else if bytes.ends_with(b"=") {
        1
    } else {
        0
    };
    if bytes[..bytes.len() - pad].contains(&b'=') {
        return Err(());
    }
    let mut vals = Vec::with_capacity(bytes.len());
    for b in bytes {
        if *b == b'=' {
            vals.push(0);
            continue;
        }
        let v = ALPHABET.iter().position(|&c| c == *b).ok_or(())? as u8;
        vals.push(v);
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in vals.chunks_exact(4) {
        let n = (u32::from(chunk[0]) << 18)
            | (u32::from(chunk[1]) << 12)
            | (u32::from(chunk[2]) << 6)
            | u32::from(chunk[3]);
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
    }
    out.truncate(bytes.len() * 3 / 4 - pad);
    Ok(out)
}

/// Refuse every token except the exact `Owen QA Test` application.
///
/// A token reset changes the secret but never the application id, so exact
/// application matching stays valid across resets. A new staging application
/// is a policy change: update the constant and its review evidence first.
pub fn check_staging_token(token: &str) -> Result<String, String> {
    match application_id_from_token(token).as_deref() {
        Some(STAGING_BOT_APPLICATION_ID) => Ok(format!(
            "token is Owen QA Test ({STAGING_BOT_APPLICATION_ID})"
        )),
        Some(LIVE_BOT_APPLICATION_ID) => Err(format!(
            "This token belongs to the LIVE bot (application {LIVE_BOT_APPLICATION_ID}), not \
             Owen QA Test ({STAGING_BOT_APPLICATION_ID}).\n  Refusing to run. Nothing was contacted."
        )),
        Some(FORMER_STAGING_BOT_APPLICATION_ID) => Err(format!(
            "This token belongs to `test-two` ({FORMER_STAGING_BOT_APPLICATION_ID}), which was the \
             staging bot until 2026-09-05. Staging is now Owen QA Test ({STAGING_BOT_APPLICATION_ID}).\n  \
             Refusing to run. Re-read DISCORD_STAGING_BOT_TOKEN from the secrets store."
        )),
        Some(app) => Err(format!(
            "This token identifies application {app}, not Owen QA Test ({STAGING_BOT_APPLICATION_ID}).\n  \
             Refusing to run. Nothing was contacted."
        )),
        None => Err(format!(
            "This token identifies an unparseable application id, not Owen QA Test ({STAGING_BOT_APPLICATION_ID}).\n  \
             Refusing to run. Nothing was contacted."
        )),
    }
}

/// Resolve the staging guild id from the environment, pinned to the TOG-1309 guild.
pub fn staging_guild_id(env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    match env("DISCORD_STAGING_GUILD_ID") {
        None => Err(
            "Missing DISCORD_STAGING_GUILD_ID. A human has to create the server: no bot can. \
             Set it to the TWO Staging guild and re-run."
                .to_owned(),
        ),
        Some(id) if id.trim() == TWO_STAGING_GUILD_ID => Ok(TWO_STAGING_GUILD_ID.to_owned()),
        Some(id) => Err(format!(
            "DISCORD_STAGING_GUILD_ID must be the TWO Staging guild ({TWO_STAGING_GUILD_ID}); got {id:?}. \
             Refusing: a snapshot/restore aimed at any other guild is a policy change, not a typo."
        )),
    }
}

// ---------------------------------------------------------------------------
// Snapshot model.
// ---------------------------------------------------------------------------

/// Tamper-evident seal written at capture: sha256 of [`canonical_snapshot`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotIntegrity {
    pub algorithm: String,
    #[serde(rename = "snapshotHash")]
    pub snapshot_hash: String,
}

/// Integrity refusal: the typed negative signal callers match on. A
/// `SnapshotIntegrityError` means "tampered backup"; anything else means
/// another restore failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error(
    "Snapshot integrity check failed: content hash {actual_hash} does not match sealed hash {expected_hash}. \
     The backup was modified after capture; refusing to restore a tampered snapshot."
)]
pub struct SnapshotIntegrityError {
    pub expected_hash: String,
    pub actual_hash: String,
}

/// Guild fields covered by the snapshot (legacy `GUILD_CONFIG_FIELDS`).
pub const GUILD_CONFIG_FIELDS: &[&str] = &[
    "name",
    "description",
    "verification_level",
    "default_message_notifications",
    "explicit_content_filter",
    "afk_timeout",
    "preferred_locale",
    "premium_progress_bar_enabled",
    "system_channel_flags",
    "system_channel_id",
    "rules_channel_id",
    "public_updates_channel_id",
    "afk_channel_id",
];

/// Stable JSON encoding: objects with keys sorted lexicographically,
/// `undefined`/`null`-valued… — precisely: legacy drops `undefined` fields
/// only. `None` in Rust serialises to `null`, which legacy `stable()` keeps
/// (`JSON.stringify(null)` is `"null"`), so the models below use
/// `skip_serializing_if` nowhere and spell absent values as explicit nulls,
/// matching the TypeScript shape field-for-field.
pub fn stable(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_string(value).expect("json scalar serialises")
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(stable).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap(), stable(&map[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// sha256 of the stable encoding, hex.
#[must_use]
pub fn config_hash(value: &Value) -> String {
    let digest = Sha256::digest(stable(value).as_bytes());
    hex_of(&digest)
}

fn hex_of(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// The normalised form the seal covers: fixed guild fields, roles/channels/
/// overwrites/emojis reduced to their restorable shape and sorted. Any
/// post-capture edit (renamed channel, altered overwrite, swapped role)
/// breaks verification.
#[must_use]
pub fn canonical_snapshot(snapshot: &Map<String, Value>) -> Value {
    let empty = Vec::new();
    let guild = snapshot.get("guild").and_then(Value::as_object);
    let guild_obj: Map<String, Value> = GUILD_CONFIG_FIELDS
        .iter()
        .map(|f| {
            (
                (*f).to_owned(),
                guild
                    .and_then(|g| g.get(*f))
                    .cloned()
                    .unwrap_or(Value::Null),
            )
        })
        .collect();

    let roles = snapshot
        .get("roles")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&empty);
    let mut canon_roles: Vec<Value> = roles
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get("id"),
                "name": r.get("name"),
                "managed": r.get("managed"),
                "color": r.get("color"),
                "hoist": r.get("hoist"),
                "permissions": r.get("permissions"),
                "mentionable": r.get("mentionable"),
                "position": r.get("position"),
            })
        })
        .collect();
    canon_roles.sort_by(|a, b| {
        position_of(a)
            .cmp(&position_of(b))
            .then_with(|| id_of(a).cmp(id_of(b)))
    });

    let channels = snapshot
        .get("channels")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&empty);
    let mut canon_channels: Vec<Value> = channels
        .iter()
        .map(|c| {
            let mut overwrites: Vec<Value> = c
                .get("permission_overwrites")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            overwrites.sort_by(|a, b| {
                type_of(a)
                    .cmp(&type_of(b))
                    .then_with(|| id_of(a).cmp(id_of(b)))
            });
            serde_json::json!({
                "id": c.get("id"),
                "name": c.get("name"),
                "type": c.get("type"),
                "parent_id": c.get("parent_id"),
                "position": c.get("position"),
                "topic": c.get("topic").cloned().unwrap_or(Value::Null),
                "nsfw": c.get("nsfw").cloned().unwrap_or(Value::Bool(false)),
                "bitrate": c.get("bitrate").cloned().unwrap_or(Value::Null),
                "user_limit": c.get("user_limit").cloned().unwrap_or(Value::Null),
                "rate_limit_per_user": c.get("rate_limit_per_user").cloned().unwrap_or(Value::Null),
                "permission_overwrites": overwrites,
            })
        })
        .collect();
    canon_channels.sort_by(|a, b| {
        position_of(a)
            .cmp(&position_of(b))
            .then_with(|| id_of(a).cmp(id_of(b)))
    });

    let emojis = snapshot
        .get("emojis")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&empty);
    let mut canon_emojis: Vec<Value> = emojis
        .iter()
        .map(|e| {
            let mut roles: Vec<Value> = e
                .get("roles")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            roles.sort_by_key(stable);
            serde_json::json!({
                "id": e.get("id"),
                "name": e.get("name"),
                "roles": roles,
                "require_colons": e.get("require_colons"),
                "managed": e.get("managed"),
                "animated": e.get("animated"),
                "available": e.get("available"),
                "image": e.get("image").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    canon_emojis.sort_by(|a, b| {
        name_of(a)
            .cmp(name_of(b))
            .then_with(|| id_of(a).cmp(id_of(b)))
    });

    serde_json::json!({
        "version": snapshot.get("version").cloned().unwrap_or(Value::Null),
        "applicationId": snapshot.get("applicationId").cloned().unwrap_or(Value::Null),
        "guildId": snapshot.get("guildId").cloned().unwrap_or(Value::Null),
        "guild": guild_obj,
        "roles": canon_roles,
        "channels": canon_channels,
        "emojis": canon_emojis,
    })
}

fn position_of(v: &Value) -> i64 {
    v.get("position").and_then(Value::as_i64).unwrap_or(0)
}

fn id_of(v: &Value) -> &str {
    v.get("id").and_then(Value::as_str).unwrap_or("")
}

fn type_of(v: &Value) -> i64 {
    v.get("type").and_then(Value::as_i64).unwrap_or(0)
}

fn name_of(v: &Value) -> &str {
    v.get("name").and_then(Value::as_str).unwrap_or("")
}

/// Attach a tamper-evident seal to a freshly captured snapshot (TOG-3513).
/// Idempotent: resealing drops the old seal first.
#[must_use]
pub fn seal_snapshot(mut snapshot: Map<String, Value>) -> Map<String, Value> {
    snapshot.remove("integrity");
    let hash = config_hash(&canonical_snapshot(&snapshot));
    snapshot.insert(
        "integrity".to_owned(),
        serde_json::json!({"algorithm": "sha256", "snapshotHash": hash}),
    );
    snapshot
}

/// Seal state of a snapshot before restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealState {
    /// Content matches the seal.
    Sealed,
    /// Predates the seal (TOG-3513): restores with a warning, not a refusal.
    Legacy,
}

/// Verify a snapshot's tamper-evident seal before restore (TOG-3513).
/// Throws [`SnapshotIntegrityError`] — never a generic error — on mismatch.
pub fn verify_snapshot_integrity(
    snapshot: &Map<String, Value>,
) -> Result<SealState, SnapshotIntegrityError> {
    let Some(seal) = snapshot.get("integrity") else {
        return Ok(SealState::Legacy);
    };
    let algorithm = seal.get("algorithm").and_then(Value::as_str).unwrap_or("");
    let expected = seal
        .get("snapshotHash")
        .and_then(Value::as_str)
        .unwrap_or("");
    if algorithm != "sha256" || expected.is_empty() {
        return Err(SnapshotIntegrityError {
            expected_hash: format!("unsupported-seal:{algorithm}"),
            actual_hash: "unverifiable".to_owned(),
        });
    }
    let mut content = snapshot.clone();
    content.remove("integrity");
    let actual = config_hash(&canonical_snapshot(&content));
    if expected != actual {
        return Err(SnapshotIntegrityError {
            expected_hash: expected.to_owned(),
            actual_hash: actual,
        });
    }
    Ok(SealState::Sealed)
}

/// Role/channel/overwrite/emoji counts of a snapshot.
#[must_use]
pub fn snapshot_counts(snapshot: &Map<String, Value>) -> (usize, usize, usize, usize) {
    let roles = snapshot
        .get("roles")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let channels = snapshot
        .get("channels")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let overwrites: usize = snapshot
        .get("channels")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .map(|c| {
                    c.get("permission_overwrites")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0);
    let emojis = snapshot
        .get("emojis")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    (roles, channels, overwrites, emojis)
}

/// Drift of a snapshot against the accepted clean-slate spec.
#[must_use]
pub fn drift_against_accepted_spec(snapshot: &Map<String, Value>) -> Value {
    let mut drift: Vec<Value> = Vec::new();
    let guild_id = snapshot
        .get("guildId")
        .and_then(Value::as_str)
        .unwrap_or("");

    let actual_description = snapshot
        .get("guild")
        .and_then(|g| g.get("description"))
        .cloned()
        .unwrap_or(Value::Null);
    push_if_different(
        &mut drift,
        "guild.description",
        &Value::String(SERVER_DESCRIPTION.to_owned()),
        &actual_description,
        "patch",
    );

    let roles = snapshot
        .get("roles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let wanted = [owner_role(), moderator_role()];
    let mut actual_roles: Vec<&Value> = Vec::new();
    for role in &wanted {
        let name = role.get("name").and_then(Value::as_str).unwrap_or("");
        let actual = roles.iter().find(|r| {
            !r.get("managed").and_then(Value::as_bool).unwrap_or(false)
                && r.get("name").and_then(Value::as_str) == Some(name)
        });
        let Some(actual) = actual else {
            drift.push(serde_json::json!({"path": format!("roles.{name}"), "expected": role, "actual": Value::Null, "restore": "create"}));
            continue;
        };
        actual_roles.push(actual);
        for field in ["color", "hoist", "permissions", "mentionable"] {
            push_if_different(
                &mut drift,
                &format!("roles.{name}.{field}"),
                &role.get(field).cloned().unwrap_or(Value::Null),
                &actual.get(field).cloned().unwrap_or(Value::Null),
                "patch",
            );
        }
    }

    let channels = snapshot
        .get("channels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (category_position, (category, names)) in CATEGORIES.iter().enumerate() {
        let actual_category = channels.iter().find(|c| {
            c.get("type").and_then(Value::as_i64) == Some(4)
                && c.get("name").and_then(Value::as_str) == Some(*category)
        });
        let Some(actual_category) = actual_category else {
            drift.push(serde_json::json!({
                "path": format!("channels.{category}"),
                "expected": {"name": category, "type": 4, "position": category_position},
                "actual": Value::Null,
                "restore": "create",
            }));
            continue;
        };
        push_if_different(
            &mut drift,
            &format!("channels.{category}.position"),
            &Value::from(category_position as u64),
            &actual_category
                .get("position")
                .cloned()
                .unwrap_or(Value::Null),
            "patch",
        );
        for (channel_position, name) in names.iter().enumerate() {
            let channel_type: i64 = if VOICE_CHANNEL_NAMES.contains(name) {
                2
            } else {
                0
            };
            let actual = channels.iter().find(|c| {
                c.get("type").and_then(Value::as_i64) == Some(channel_type)
                    && c.get("name").and_then(Value::as_str) == Some(*name)
                    && c.get("parent_id").and_then(Value::as_str)
                        == actual_category.get("id").and_then(Value::as_str)
            });
            let Some(actual) = actual else {
                drift.push(serde_json::json!({
                    "path": format!("channels.{category}.{name}"),
                    "expected": {"name": name, "type": channel_type, "parent": category, "position": channel_position},
                    "actual": Value::Null,
                    "restore": "create",
                }));
                continue;
            };
            push_if_different(
                &mut drift,
                &format!("channels.{category}.{name}.position"),
                &Value::from(channel_position as u64),
                &actual.get("position").cloned().unwrap_or(Value::Null),
                "patch",
            );
            if TEXT_CHANNEL_NAMES.contains(name) {
                if let Some(topic) = channel_topic(name) {
                    push_if_different(
                        &mut drift,
                        &format!("channels.{category}.{name}.topic"),
                        &Value::String(topic.to_owned()),
                        &actual.get("topic").cloned().unwrap_or(Value::Null),
                        "patch",
                    );
                }
            }
            let expected_overwrite = desired_everyone_overwrite(guild_id, name);
            let actual_overwrite = actual
                .get("permission_overwrites")
                .and_then(Value::as_array)
                .and_then(|ows| {
                    ows.iter().find(|o| {
                        o.get("id").and_then(Value::as_str) == Some(guild_id)
                            && o.get("type").and_then(Value::as_i64) == Some(0)
                    })
                })
                .cloned()
                .unwrap_or(Value::Null);
            push_if_different(
                &mut drift,
                &format!("channels.{category}.{name}.everyoneOverwrite"),
                &expected_overwrite,
                &actual_overwrite,
                "patch",
            );
        }
    }

    let (roles_n, channels_n, overwrites_n, emojis_n) = snapshot_counts(snapshot);
    let snapshot_hash = config_hash(&canonical_snapshot(snapshot));
    let accepted_spec_hash = config_hash(&accepted_spec_value(guild_id));
    serde_json::json!({
        "version": 1,
        "generatedAt": unix_now_iso(),
        "guildId": guild_id,
        "snapshotHash": snapshot_hash,
        "acceptedSpecHash": accepted_spec_hash,
        "counts": {
            "roles": roles_n,
            "channels": channels_n,
            "overwrites": overwrites_n,
            "emojis": emojis_n,
            "drift": drift.len(),
        },
        "drift": drift,
    })
}

fn push_if_different(
    drift: &mut Vec<Value>,
    path: &str,
    expected: &Value,
    actual: &Value,
    restore: &str,
) {
    if stable(expected) != stable(actual) {
        drift.push(serde_json::json!({
            "path": path,
            "expected": expected,
            "actual": actual,
            "restore": restore,
        }));
    }
}

fn accepted_spec_value(guild_id: &str) -> Value {
    let categories: Vec<Value> = CATEGORIES
        .iter()
        .enumerate()
        .map(|(position, (name, channels))| {
            let chans: Vec<Value> = channels
                .iter()
                .enumerate()
                .map(|(channel_position, channel)| {
                    let channel_type: i64 = if VOICE_CHANNEL_NAMES.contains(channel) {
                        2
                    } else {
                        0
                    };
                    let mut obj = serde_json::json!({
                        "name": channel,
                        "type": channel_type,
                        "position": channel_position,
                        "everyoneOverwrite": desired_everyone_overwrite(guild_id, channel),
                    });
                    if TEXT_CHANNEL_NAMES.contains(channel) {
                        if let Some(topic) = channel_topic(channel) {
                            obj["topic"] = Value::String(topic.to_owned());
                        }
                    }
                    obj
                })
                .collect();
            serde_json::json!({
                "name": name,
                "type": 4,
                "position": position,
                "channels": chans,
            })
        })
        .collect();
    serde_json::json!({
        "guild": {"description": SERVER_DESCRIPTION},
        "roles": [owner_role(), moderator_role()],
        "categories": categories,
    })
}

/// Current Unix time as an ISO-8601 UTC string (no chrono dependency).
#[must_use]
pub fn unix_now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    iso_of_epoch(secs)
}

pub(crate) fn iso_of_epoch(epoch_secs: u64) -> String {
    // Civil-date conversion (Howard Hinnant's algorithm), proleptic Gregorian.
    let days = (epoch_secs / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let mut month = (mp + 3) as u64;
    if month > 12 {
        month -= 12;
        year += 1;
    }
    let secs = epoch_secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Human timestamp for backup filenames: `20260903T041700Z` (no separators).
#[must_use]
pub fn filename_stamp() -> String {
    unix_now_iso().replace(['-', ':'], "").replace(".000Z", "Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn snapshot_fixture() -> Map<String, Value> {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "generatedAt": "2026-09-24T00:00:00.000Z",
            "applicationId": STAGING_BOT_APPLICATION_ID,
            "guildId": TWO_STAGING_GUILD_ID,
            "guild": {"description": SERVER_DESCRIPTION, "name": "TWO Staging"},
            "roles": [
                {"id": TWO_STAGING_GUILD_ID, "name": "@everyone", "managed": false, "color": 0, "hoist": false, "permissions": "0", "mentionable": false, "position": 0},
                {"id": "r1", "name": "Owner", "managed": false, "color": 0xd4af37, "hoist": true, "permissions": "0", "mentionable": false, "position": 2},
                {"id": "r2", "name": "Moderator", "managed": false, "color": 0x5865f2, "hoist": true,
                 "permissions": moderator_role()["permissions"], "mentionable": false, "position": 1}
            ],
            "channels": [],
            "emojis": [],
        }))
        .unwrap()
    }

    #[test]
    fn seal_is_stable_and_idempotent() {
        let snap = snapshot_fixture();
        let sealed = seal_snapshot(snap.clone());
        assert_eq!(verify_snapshot_integrity(&sealed), Ok(SealState::Sealed));
        // Resealing drops the old seal first: seal(seal(s)) === seal(s).
        let resealed = seal_snapshot(sealed.clone());
        assert_eq!(sealed["integrity"], resealed["integrity"]);
    }

    #[test]
    fn tampered_content_is_refused_with_the_typed_error() {
        let snap = snapshot_fixture();
        let mut sealed = seal_snapshot(snap);
        sealed["guild"].as_object_mut().unwrap().insert(
            "description".to_owned(),
            Value::String("tampered".to_owned()),
        );
        let err = verify_snapshot_integrity(&sealed).expect_err("tamper must be refused");
        assert!(err
            .to_string()
            .contains("refusing to restore a tampered snapshot"));
        assert_ne!(err.expected_hash, err.actual_hash);
    }

    #[test]
    fn legacy_snapshot_without_seal_restores_with_a_warning_state() {
        let snap = snapshot_fixture();
        assert_eq!(verify_snapshot_integrity(&snap), Ok(SealState::Legacy));
    }

    #[test]
    fn stable_encoding_sorts_keys_and_matches_legacy_spelling() {
        let v: Value = serde_json::json!({"b": 1, "a": [3, 2], "c": {"y": true, "x": null}});
        assert_eq!(stable(&v), r#"{"a":[3,2],"b":1,"c":{"x":null,"y":true}}"#);
    }

    #[test]
    fn token_check_accepts_only_the_staging_application() {
        // First segments are base64("1469137636663758888") etc. (padded, as
        // Discord issues them); the decoder also tolerates missing padding
        // like Node's Buffer.from(seg, 'base64').
        let staging = "MTQ2OTEzNzYzNjY2Mzc1ODg4OA==.dummy.signature";
        assert!(check_staging_token(staging).is_ok());
        assert!(check_staging_token("MTQ2OTEzNzYzNjY2Mzc1ODg4OA.dummy.signature").is_ok());
        let live = "MTUzOTcxMTY4Mzg5ODExODE1NA==.dummy.signature";
        let err = check_staging_token(live).expect_err("live token refused");
        assert!(err.contains("LIVE bot"), "{err}");
        let former = "MTUzNzYyOTY4MjQ0OTY0OTcyNA==.dummy.signature";
        let err = check_staging_token(former).expect_err("superseded token refused");
        assert!(err.contains("test-two"), "{err}");
        assert!(check_staging_token("not-a-token").is_err());
    }

    #[test]
    fn token_identity_accepts_only_the_supported_bot_prefix() {
        for token in [
            "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature",
            "MTUzOTcxMTY4Mzg5ODExODE1NA.mock.signature",
        ] {
            assert_eq!(
                application_id_from_token(&format!("Bot {token}")),
                application_id_from_token(token)
            );
            for prefix in ["Bearer ", "bot ", "Bot Bot "] {
                assert_eq!(application_id_from_token(&format!("{prefix}{token}")), None);
            }
        }
        assert_eq!(application_id_from_token("Bot not-a-token"), None);
    }

    #[test]
    fn staging_guild_is_pinned_to_the_tog_1309_guild() {
        let env = HashMap::from([(
            "DISCORD_STAGING_GUILD_ID".to_owned(),
            TWO_STAGING_GUILD_ID.to_owned(),
        )]);
        assert_eq!(
            staging_guild_id(&|k| env.get(k).cloned()),
            Ok(TWO_STAGING_GUILD_ID.to_owned())
        );
        let env = HashMap::from([("DISCORD_STAGING_GUILD_ID".to_owned(), "123".to_owned())]);
        assert!(staging_guild_id(&|k| env.get(k).cloned()).is_err());
        let env: HashMap<String, String> = HashMap::new();
        assert!(staging_guild_id(&|k| env.get(k).cloned()).is_err());
    }

    #[test]
    fn drift_report_counts_match_and_hash_is_stable() {
        let sealed = seal_snapshot(snapshot_fixture());
        let report = drift_against_accepted_spec(&sealed);
        // No channels at all: every accepted category/channel is drift.
        assert!(report["counts"]["drift"].as_u64().unwrap() > 0);
        assert_eq!(report["snapshotHash"], sealed["integrity"]["snapshotHash"]);
    }

    #[test]
    fn filename_stamp_has_no_separators() {
        let stamp = filename_stamp();
        assert!(!stamp.contains(['-', ':']));
        assert!(stamp.ends_with('Z'));
    }
}
