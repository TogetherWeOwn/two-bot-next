//! Ghost-channel cleanup runner (TOG-13564).
//!
//! Dry-run-first executor over the pure
//! [`two_bot_core::voice_ghost_cleanup::plan_ghost_cleanup`] plan. The live
//! channel snapshot comes from a file (produced by the sibling read-only
//! ghost count, TOG-13548, or any gateway-derived dump) because Discord
//! REST lists channels but not their voice occupants:
//!
//! ```json
//! [
//!   {"channel_id": "123", "human_occupants": 0, "manageable": true}
//! ]
//! ```
//!
//! `channel_id` accepts a JSON string or number; entries must be unique and
//! non-zero. Dry run (the default) reads the tracked rows, prints the plan,
//! and writes nothing anywhere — no Discord client is even constructed.
//! `--execute` deletes the planned channels and forgets the planned rows,
//! and refuses to run unless `--expect` matches the plan's action count.

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};
use two_bot_core::raid_removal::RemovalMode;
use two_bot_core::voice_ghost_cleanup::GhostCleanupPlan;
use two_bot_core::voice_rooms::{SeenChannel, VoiceRoom};
use two_bot_core::Snowflake;

use super::rest::{DeleteOutcome, RestClient};
use super::voice_rooms::PgRoomStore;

/// One live channel from the operator-supplied snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotChannel {
    #[serde(deserialize_with = "de_snowflake")]
    pub channel_id: Snowflake,
    pub human_occupants: usize,
    pub manageable: bool,
}

fn de_snowflake<'de, D>(deserializer: D) -> Result<Snowflake, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct SnowflakeVisitor;

    impl Visitor<'_> for SnowflakeVisitor {
        type Value = Snowflake;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a Discord snowflake as a string or integer")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Snowflake, E> {
            parse_snapshot_id(v).map_err(E::custom)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Snowflake, E> {
            check_snapshot_digits(v).map_err(E::custom)?;
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Snowflake, E> {
            if v <= 0 {
                return Err(E::custom("channel_id must be non-zero"));
            }
            check_snapshot_digits(v as Snowflake).map_err(E::custom)?;
            Ok(v as Snowflake)
        }
    }

    deserializer.deserialize_any(SnowflakeVisitor)
}

/// Integer snapshot ids get the same 17–20-digit snowflake shape the string
/// path enforces: Discord counts UTF-16 units, and ids outside this band are
/// typos, not channels.
fn check_snapshot_digits(v: Snowflake) -> Result<(), String> {
    let digits = v.to_string().len();
    if !(17..=20).contains(&digits) {
        return Err(format!(
            "invalid channel_id \"{v}\": not a Discord snowflake"
        ));
    }
    Ok(())
}

fn parse_snapshot_id(raw: &str) -> Result<Snowflake, String> {
    if !(17..=20).contains(&raw.len())
        || raw.starts_with('0')
        || !raw.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!(
            "invalid channel_id \"{raw}\": not a Discord snowflake"
        ));
    }
    raw.parse::<Snowflake>()
        .map_err(|_| format!("invalid channel_id \"{raw}\": not a Discord snowflake"))
        .and_then(|n| {
            if n == 0 {
                Err(format!(
                    "invalid channel_id \"{raw}\": not a Discord snowflake"
                ))
            } else {
                Ok(n)
            }
        })
}

/// Parse and validate a snapshot document: a JSON array of
/// [`SnapshotChannel`]. Duplicates are rejected — two occupancy claims for
/// one channel cannot both be honored.
pub fn parse_snapshot(text: &str) -> Result<Vec<SnapshotChannel>, String> {
    let channels: Vec<SnapshotChannel> = serde_json::from_str(text)
        .map_err(|e| format!("snapshot is not a JSON channel array: {e}"))?;
    if channels.is_empty() {
        return Err("snapshot holds no channels".to_owned());
    }
    let mut seen_ids = HashSet::new();
    for channel in &channels {
        if !seen_ids.insert(channel.channel_id) {
            return Err(format!(
                "snapshot lists channel {} twice: occupancy is ambiguous, refusing",
                channel.channel_id
            ));
        }
    }
    Ok(channels)
}

/// Project snapshot channels into the planner's live-channel shape.
#[must_use]
pub fn snapshot_to_seen(channels: &[SnapshotChannel]) -> Vec<SeenChannel> {
    channels
        .iter()
        .map(|c| SeenChannel {
            channel_id: c.channel_id,
            human_occupants: c.human_occupants,
            manageable: c.manageable,
        })
        .collect()
}

/// One failed cleanup step, with the channel left untouched for the next run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhostFailure {
    pub channel_id: String,
    pub step: String,
    pub error: String,
}

/// What `--execute` changed (dry run reports the plan with empty outcomes).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhostApplyOutcome {
    /// Tracked-gone rows dropped without a Discord write.
    pub forgot: Vec<String>,
    /// Channels Discord confirms are gone (deleted or already gone).
    pub deleted: Vec<String>,
    /// Steps that failed; those channels/rows are untouched.
    pub failed: Vec<GhostFailure>,
    pub discord_requests: u64,
}

/// Forget tracked-gone rows: no Discord write, only row drops.
pub async fn apply_forgets(
    store: &PgRoomStore,
    guild_id: Snowflake,
    rooms: &[VoiceRoom],
) -> (Vec<String>, Vec<GhostFailure>) {
    let mut forgot = Vec::new();
    let mut failed = Vec::new();
    for room in rooms {
        match store.remove_room(guild_id, room.channel_id).await {
            Ok(_) => forgot.push(room.channel_id.to_string()),
            Err(e) => failed.push(GhostFailure {
                channel_id: room.channel_id.to_string(),
                step: "forget".to_owned(),
                error: format!("row drop refused: {e}"),
            }),
        }
    }
    forgot.sort();
    (forgot, failed)
}

/// Delete empty tracked channels, then forget their rows. `reason` travels
/// in the Discord audit-log header of every delete. A failed delete keeps
/// the row (a 403 is never proof of deletion); a failed row drop after a
/// successful delete is reported — the channel is already gone, so the next
/// run forgets it.
pub async fn apply_deletes(
    client: &RestClient,
    store: &PgRoomStore,
    guild_id: Snowflake,
    rooms: &[VoiceRoom],
    reason: &str,
) -> (Vec<String>, Vec<GhostFailure>) {
    use twilight_model::id::{marker::ChannelMarker, Id};

    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    for room in rooms {
        match client
            .delete_channel(Id::<ChannelMarker>::new(room.channel_id), reason)
            .await
        {
            Ok(DeleteOutcome::Deleted | DeleteOutcome::AlreadyGone) => {
                match store.remove_room(guild_id, room.channel_id).await {
                    Ok(_) => deleted.push(room.channel_id.to_string()),
                    Err(e) => failed.push(GhostFailure {
                        channel_id: room.channel_id.to_string(),
                        step: "forget-after-delete".to_owned(),
                        error: format!("channel gone but row drop refused: {e}"),
                    }),
                }
            }
            Err(e) => failed.push(GhostFailure {
                channel_id: room.channel_id.to_string(),
                step: "delete".to_owned(),
                error: format!("discord delete refused: {e}"),
            }),
        }
    }
    deleted.sort();
    (deleted, failed)
}

fn room_json(room: &VoiceRoom) -> serde_json::Value {
    serde_json::json!({
        "channel_id": room.channel_id.to_string(),
        "owner_id": room.owner_id.to_string(),
        "creator_channel_id": room.creator_channel_id.to_string(),
    })
}

/// Render the operator report: the plan plus (in execute mode) the outcomes.
/// Untracked channels are listed for manual triage — the tool never deletes
/// them.
#[must_use]
pub fn render_report(
    guild: &str,
    mode: RemovalMode,
    tracked_count: usize,
    plan: &GhostCleanupPlan,
    outcome: &GhostApplyOutcome,
) -> serde_json::Value {
    let mode_str = match mode {
        RemovalMode::DryRun => "dry-run",
        RemovalMode::Execute => "execute",
    };
    serde_json::json!({
        "tool": "ghost-cleanup",
        "guild": guild,
        "mode": mode_str,
        "tracked_rooms": tracked_count,
        "actions": {
            "forget": plan.forget.iter().map(room_json).collect::<Vec<_>>(),
            "delete": plan.delete.iter().map(room_json).collect::<Vec<_>>(),
        },
        "untouched": {
            "occupied": plan.occupied.iter().map(room_json).collect::<Vec<_>>(),
            "suspended": plan.suspended.iter().map(room_json).collect::<Vec<_>>(),
            "untracked_present": plan.untracked_present.iter().map(u64::to_string).collect::<Vec<_>>(),
        },
        "outcome": outcome,
        "note": "untracked channels are never deleted by this tool; claiming or removing one is an explicit operator act.",
    })
}

/// Seeded demo feeds: one room per plan class plus one untracked channel.
/// No database, no Discord; `--seed` prints the dry-run report for these.
#[must_use]
pub fn build_seed_ghost_cleanup() -> (Vec<VoiceRoom>, Vec<SnapshotChannel>) {
    let room = |channel: Snowflake| VoiceRoom {
        guild_id: 9,
        channel_id: channel,
        creator_channel_id: 7,
        owner_id: 41,
        original_creator_id: 41,
        name_seed: 1,
        created_at: "2026-09-20T12:00:00.000Z".to_owned(),
    };
    let seen = |channel: Snowflake, humans: usize, manageable: bool| SnapshotChannel {
        channel_id: channel,
        human_occupants: humans,
        manageable,
    };
    let tracked = vec![
        room(101), // gone -> forget
        room(102), // empty + manageable -> delete
        room(103), // occupied -> untouched
        room(104), // unmanageable -> suspended
    ];
    let live = vec![
        seen(102, 0, true),
        seen(103, 2, true),
        seen(104, 0, false),
        seen(201, 0, true), // untracked
    ];
    (tracked, live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_bot_core::voice_ghost_cleanup::plan_ghost_cleanup;

    #[test]
    fn snapshot_accepts_string_and_integer_ids() {
        let parsed = parse_snapshot(
            r#"[{"channel_id": "123456789012345678", "human_occupants": 0, "manageable": true},
                {"channel_id": 123456789012345679, "human_occupants": 2, "manageable": false}]"#,
        )
        .expect("valid snapshot parses");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].channel_id, 123456789012345678);
        assert_eq!(parsed[1].channel_id, 123456789012345679);
        let seen = snapshot_to_seen(&parsed);
        assert_eq!(seen[1].human_occupants, 2);
        assert!(!seen[1].manageable);
    }

    #[test]
    fn snapshot_rejects_non_array_bad_ids_and_duplicates() {
        assert!(parse_snapshot(r#"{"channel_id": "1"}"#).is_err());
        assert!(parse_snapshot(r#"[]"#).is_err());
        assert!(parse_snapshot(
            r#"[{"channel_id": "nope", "human_occupants": 0, "manageable": true}]"#
        )
        .is_err());
        assert!(parse_snapshot(
            r#"[{"channel_id": "0", "human_occupants": 0, "manageable": true}]"#
        )
        .is_err());
        assert!(
            parse_snapshot(
                r#"[{"channel_id": "123456789012345678", "human_occupants": 0, "manageable": true},
                    {"channel_id": "123456789012345678", "human_occupants": 1, "manageable": true}]"#
            )
            .is_err()
        );
        // Integer ids get the same 17–20-digit shape as string ids.
        assert!(parse_snapshot(
            r#"[{"channel_id": 42, "human_occupants": 0, "manageable": true}]"#
        )
        .is_err());
        assert!(parse_snapshot(
            r#"[{"channel_id": -5, "human_occupants": 0, "manageable": true}]"#
        )
        .is_err());
    }

    #[test]
    fn seed_feeds_cover_every_plan_class() {
        let (tracked, live) = build_seed_ghost_cleanup();
        let plan = plan_ghost_cleanup(&tracked, &snapshot_to_seen(&live));
        assert_eq!(plan.forget.len(), 1);
        assert_eq!(plan.delete.len(), 1);
        assert_eq!(plan.occupied.len(), 1);
        assert_eq!(plan.suspended.len(), 1);
        assert_eq!(plan.untracked_present, vec![201]);
        assert_eq!(plan.action_count(), 2);
    }

    #[test]
    fn report_marks_mode_and_never_deletes_untracked() {
        let (tracked, live) = build_seed_ghost_cleanup();
        let plan = plan_ghost_cleanup(&tracked, &snapshot_to_seen(&live));
        let report = render_report(
            "9",
            RemovalMode::DryRun,
            tracked.len(),
            &plan,
            &GhostApplyOutcome::default(),
        );
        assert_eq!(report["tool"], "ghost-cleanup");
        assert_eq!(report["mode"], "dry-run");
        assert_eq!(
            report["actions"]["delete"].as_array().map_or(0, Vec::len),
            1
        );
        assert_eq!(
            report["untouched"]["untracked_present"],
            serde_json::json!(["201"])
        );
    }
}
