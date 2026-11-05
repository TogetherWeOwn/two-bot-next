//! Keyless `voice.snapshot` projection over the shared gateway cache.
//!
//! The signed `POST /internal/actions` receiver serves this verb from the
//! same fenced [`two_bot_discord::SharedCache`] the gateway loop feeds through
//! the pipeline: Discord exposes no REST voice-state list, so the live cache is
//! the only source. A cache that cannot prove a complete guild view refuses
//! with [`SnapshotRefusal::CacheIncomplete`] (the receiver answers
//! `discord_unavailable`), never a partial listing and never invented data.
//!
//! Shape v1 (`result` beside the correlated id, `Cache-Control: no-store`
//! like `event.read`):
//! `{"v":1,"observed_at":"<Zulu whole seconds>","total_in_voice":N,
//! "rooms":[{"channel_id":"<snowflake>","name":"...","position":N|null,
//! "user_limit":N,"member_ids":["<snowflake>",...],"afk":true?}]}`.
//!
//! Projection rules: guild voice channels only (stage and text excluded);
//! every voice channel is a room even when empty; the AFK channel carries
//! `"afk": true` and its occupants stay listed but leave `total_in_voice`
//! out; bots leave both the member lists and the total out; a member the
//! cache has not populated counts as human (never delete on incomplete bot
//! data); member ids are snowflake decimal strings only — no usernames,
//! nicknames, presence or activity; rooms sort by position then name.
//! Member ids never reach logs; the read audits through the receiver's
//! counters only.

use std::sync::Arc;

use serde_json::{json, Value};
use twilight_cache_inmemory::{DefaultInMemoryCache, ResourceType};
use twilight_model::channel::ChannelType;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, UserMarker},
    Id,
};
use two_bot_core::Snowflake;

/// Relayed-answer budget for the serialized `result` object. The Worker
/// refuses to relay bodies over its 64 KiB bound
/// (`wrangler/src/internal-actions.ts` `MAX_RESPONSE_BYTES`); the 512-byte
/// reserve covers the `{"ok","result","request_id"}` envelope around it.
/// Stays far under the contract's 256 KiB ceiling with margin.
const MAX_SNAPSHOT_RESULT_BYTES: usize = 64 * 1024 - 512;

/// Why the snapshot read refused. Both map to existing refusal codes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotRefusal {
    /// The cache holds no complete guild view (gateway not READY, guild
    /// gone, channels missing, a voice state mid-mutation). The receiver
    /// answers `discord_unavailable` (502, retryable).
    CacheIncomplete,
    /// The projection blew the relayed-answer budget. The receiver answers
    /// `internal` (500, retryable): a later read may be smaller.
    OverBudget,
}

/// One guild voice channel with its human occupants, pre-sort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoomDraft {
    pub channel_id: Snowflake,
    pub name: String,
    pub position: Option<i32>,
    pub user_limit: u32,
    /// Human occupant ids (bots already excluded, unknown identity kept).
    /// Sorted ascending before serializing so the wire shape is stable.
    pub member_ids: Vec<Snowflake>,
    pub afk: bool,
}

/// Read the §1 snapshot from the live gateway cache. `None` anywhere on the
/// guild → channels → voice-states → per-state lookup chain is
/// [`SnapshotRefusal::CacheIncomplete`]; see [`collect_rooms`] for the one
/// deliberate exception (a guild the cache holds whose voice-state set went
/// missing reads as empty, not incomplete).
///
/// The cache alone cannot prove freshness: a transport loss leaves the last
/// occupancy readable, so callers gate on the supervisor's readiness first
/// and only call this while the gateway is connected.
pub(crate) fn read_voice_snapshot(
    cache: &two_bot_discord::SharedCache,
    guild_id: Snowflake,
) -> Result<Value, SnapshotRefusal> {
    let rooms = cache
        .with_guild_snapshot(guild_id, |cache| collect_rooms(cache, guild_id))
        .flatten()
        .ok_or(SnapshotRefusal::CacheIncomplete)?;
    project(observed_at_now(), rooms)
}

/// Read the same complete-cache, bot/AFK-excluded total without building an
/// HTTP response. The relay byte budget applies only to the room listing,
/// never to the sampler's scalar. Callers still gate gateway freshness.
pub(crate) fn read_voice_total(
    cache: &two_bot_discord::SharedCache,
    guild_id: Snowflake,
) -> Result<usize, SnapshotRefusal> {
    let rooms = cache
        .with_guild_snapshot(guild_id, |cache| collect_rooms(cache, guild_id))
        .flatten()
        .ok_or(SnapshotRefusal::CacheIncomplete)?;
    Ok(total_in_voice(&rooms))
}

fn total_in_voice(rooms: &[RoomDraft]) -> usize {
    rooms
        .iter()
        .filter(|room| !room.afk)
        .map(|room| room.member_ids.len())
        .sum()
}

/// Whole-second Zulu timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for `observed_at`.
fn observed_at_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Shared-cache handle constructor for the binary wiring (kept beside the
/// reader so `main.rs` needs no twilight imports of its own).
pub(crate) fn new_shared_cache() -> Arc<two_bot_discord::SharedCache> {
    Arc::new(two_bot_discord::SharedCache::default())
}

/// Collect every guild voice channel with its human occupants. Returns
/// `None` unless the cache proves the full view:
/// - guild entry missing → `None` (gateway not READY, or the receiver runs
///   split from the gateway with a cold cache);
/// - channel set missing, or any listed channel failing lookup → `None` (an
///   unavailable guild keeps its entry while its channels are gone, so this
///   also refuses stale unavailable views);
/// - voice-state set missing → empty occupants, but only when the cache is
///   built with voice states: Twilight drops the set when the last member
///   leaves and re-creates it on the next join, so for a held guild a
///   missing set means "nobody in voice", not "not loaded yet";
/// - any listed occupant's voice state failing lookup → `None` (a leave
///   racing the read; retryable, never a half-moved member).
fn collect_rooms(cache: &DefaultInMemoryCache, guild_id: Snowflake) -> Option<Vec<RoomDraft>> {
    let guild_key: Id<GuildMarker> = Id::new(guild_id);
    let guild = cache.guild(guild_key)?;
    if guild.unavailable() == Some(true) {
        return None;
    }
    let afk_channel: Option<Id<ChannelMarker>> = guild.afk_channel_id();
    drop(guild);
    let channel_ids: Vec<Id<ChannelMarker>> =
        cache.guild_channels(guild_key)?.iter().copied().collect();
    let mut rooms = Vec::new();
    for channel_id in &channel_ids {
        let channel = cache.channel(*channel_id)?;
        if channel.kind != ChannelType::GuildVoice {
            continue;
        }
        rooms.push(RoomDraft {
            channel_id: channel_id.get(),
            // Channel objects cache atomically; a missing name is cosmetic,
            // never membership data, so it defaults instead of refusing.
            name: channel.name.clone().unwrap_or_default(),
            position: channel.position,
            user_limit: channel.user_limit.unwrap_or(0),
            member_ids: Vec::new(),
            afk: Some(*channel_id) == afk_channel,
        });
    }
    let occupants: Vec<Id<UserMarker>> = match cache.guild_voice_states(guild_key) {
        Some(users) => users.iter().copied().collect(),
        None if cache
            .config()
            .resource_types()
            .contains(ResourceType::VOICE_STATE) =>
        {
            Vec::new()
        }
        None => return None,
    };
    for user_id in &occupants {
        let state = cache.voice_state(*user_id, guild_key)?;
        let room = rooms
            .iter_mut()
            .find(|room| room.channel_id == state.channel_id().get());
        // States pointing outside the guild's voice channels (stage
        // speakers) are out of scope for the v1 shape, not a gap: the total
        // stays the verifiable sum of the listed rooms.
        let Some(room) = room else { continue };
        // Unknown member identity counts as human: never delete on
        // incomplete bot data.
        let is_bot = cache.user(*user_id).is_some_and(|user| user.bot);
        if !is_bot {
            room.member_ids.push(user_id.get());
        }
    }
    Some(rooms)
}

/// Sort, total and serialize the v1 shape, enforcing the relayed-answer
/// budget. Pure over plain drafts so the ordering/total rules pin without a
/// gateway cache.
pub(crate) fn project(
    observed_at: String,
    mut rooms: Vec<RoomDraft>,
) -> Result<Value, SnapshotRefusal> {
    for room in &mut rooms {
        room.member_ids.sort_unstable();
    }
    // Missing positions sort last; the channel id breaks any remaining tie
    // so the wire order is fully deterministic.
    rooms.sort_by(|a, b| {
        (a.position.unwrap_or(i32::MAX), &a.name, a.channel_id).cmp(&(
            b.position.unwrap_or(i32::MAX),
            &b.name,
            b.channel_id,
        ))
    });
    let total_in_voice = total_in_voice(&rooms);
    let wire_rooms: Vec<Value> = rooms
        .iter()
        .map(|room| {
            let mut wire = json!({
                "channel_id": room.channel_id.to_string(),
                "name": room.name,
                "position": room.position,
                "user_limit": room.user_limit,
                "member_ids": room.member_ids.iter().map(Snowflake::to_string).collect::<Vec<_>>(),
            });
            if room.afk {
                wire["afk"] = Value::Bool(true);
            }
            wire
        })
        .collect();
    let value = json!({
        "v": 1,
        "observed_at": observed_at,
        "total_in_voice": total_in_voice,
        "rooms": wire_rooms,
    });
    let bytes = serde_json::to_vec(&value).map_err(|_| SnapshotRefusal::OverBudget)?;
    if bytes.len() > MAX_SNAPSHOT_RESULT_BYTES {
        return Err(SnapshotRefusal::OverBudget);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guild_snapshot() -> twilight_model::guild::Guild {
        let mut guild: Value =
            serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json"))
                .expect("guild fixture");
        guild["channels"] = json!([{
            "id": "31", "guild_id": "100", "type": 2, "name": "voice",
            "permission_overwrites": []
        }]);
        guild["voice_states"] = json!([{
            "guild_id": "100", "channel_id": "31", "user_id": "42",
            "session_id": "fixture", "deaf": false, "mute": false,
            "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false
        }]);
        serde_json::from_value(guild).expect("populated guild fixture")
    }

    #[test]
    fn unavailable_and_ready_pending_guilds_refuse_until_full_snapshot() {
        use twilight_model::gateway::{event::Event, payload::incoming::GuildCreate};
        let cache = new_shared_cache();
        assert!(cache.needs_bootstrap());
        cache.update(&Event::UserUpdate(
            serde_json::from_value(json!({
                "id": "999", "username": "fixture", "discriminator": "0", "mfa_enabled": false
            }))
            .expect("replayed user update"),
        ));
        assert!(
            cache.needs_bootstrap(),
            "a replayed identity is not READY evidence"
        );
        let full = || Event::GuildCreate(Box::new(GuildCreate::Available(guild_snapshot())));
        cache.update(&full());
        assert_eq!(read_voice_total(&cache, 100), Ok(1));
        // Twilight retains an old guild on READY. Its pending marker must
        // prevent the retained complete-looking roster from being sampled.
        let ready = serde_json::from_value(json!({
            "v": 10, "session_id": "fixture", "resume_gateway_url": "ws://fixture.invalid",
            "application": {"id": "999", "flags": 0},
            "user": {"id": "999", "username": "fixture", "discriminator": "0", "mfa_enabled": false},
            "guilds": [{"id": "100", "unavailable": true}]
        }))
        .expect("ready fixture");
        cache.update(&Event::Ready(Box::new(ready)));
        assert!(!cache.needs_bootstrap());
        assert_eq!(
            read_voice_total(&cache, 100),
            Err(SnapshotRefusal::CacheIncomplete)
        );
        assert_eq!(
            read_voice_snapshot(&cache, 100),
            Err(SnapshotRefusal::CacheIncomplete)
        );
        cache.update(&full());
        assert_eq!(read_voice_total(&cache, 100), Ok(1));
        cache.update(&Event::GuildCreate(Box::new(GuildCreate::Unavailable(
            serde_json::from_value(json!({"id": "100", "unavailable": true}))
                .expect("unavailable guild fixture"),
        ))));
        assert_eq!(
            read_voice_total(&cache, 100),
            Err(SnapshotRefusal::CacheIncomplete)
        );
        cache.update(&full());
        assert_eq!(read_voice_total(&cache, 100), Ok(1));
        assert_eq!(
            read_voice_snapshot(&cache, 100).expect("full snapshot")["total_in_voice"],
            1
        );
        let mut empty = guild_snapshot();
        empty.voice_states.clear();
        cache.update(&Event::GuildCreate(Box::new(GuildCreate::Available(empty))));
        assert_eq!(
            read_voice_total(&cache, 100),
            Ok(0),
            "genuine empty snapshot is zero"
        );
    }

    fn draft(
        channel_id: Snowflake,
        name: &str,
        position: Option<i32>,
        member_ids: &[Snowflake],
        afk: bool,
    ) -> RoomDraft {
        RoomDraft {
            channel_id,
            name: name.to_owned(),
            position,
            user_limit: 0,
            member_ids: member_ids.to_vec(),
            afk,
        }
    }

    fn rooms_of(value: &Value) -> Vec<(String, String, bool, Vec<String>)> {
        value["rooms"]
            .as_array()
            .expect("rooms is an array")
            .iter()
            .map(|room| {
                (
                    room["channel_id"].as_str().expect("channel id").to_owned(),
                    room["name"].as_str().expect("name").to_owned(),
                    room.get("afk").and_then(Value::as_bool).unwrap_or(false),
                    room["member_ids"]
                        .as_array()
                        .expect("member ids")
                        .iter()
                        .map(|id| id.as_str().expect("id string").to_owned())
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn rooms_sort_by_position_then_name() {
        let value = project(
            "2026-11-03T10:00:00Z".to_owned(),
            vec![
                draft(3, "beta", Some(1), &[], false),
                draft(1, "alpha", Some(1), &[], false),
                draft(2, "first", Some(0), &[], false),
                draft(4, "last", None, &[], false),
            ],
        )
        .expect("small projection fits");
        let names: Vec<String> = rooms_of(&value).into_iter().map(|room| room.1).collect();
        assert_eq!(names, vec!["first", "alpha", "beta", "last"]);
    }

    #[test]
    fn afk_room_flags_and_leaves_the_total_out() {
        let value = project(
            "2026-11-03T10:00:00Z".to_owned(),
            vec![
                draft(10, "lobby", Some(0), &[7, 9], false),
                draft(11, "afk", Some(1), &[8], true),
            ],
        )
        .expect("small projection fits");
        assert_eq!(value["total_in_voice"], json!(2));
        let rooms = rooms_of(&value);
        // Occupants stay listed in their room; only the total excludes them.
        assert_eq!(rooms[1].3, vec!["8"]);
        assert!(rooms[1].2, "afk room carries the flag");
        assert!(!rooms[0].2, "plain rooms omit the flag");
        assert!(value["rooms"][0].get("afk").is_none());
    }

    #[test]
    fn empty_rooms_stay_listed_with_empty_member_ids() {
        let value = project(
            "2026-11-03T10:00:00Z".to_owned(),
            vec![draft(10, "quiet", Some(0), &[], false)],
        )
        .expect("small projection fits");
        assert_eq!(value["total_in_voice"], json!(0));
        assert_eq!(value["rooms"][0]["member_ids"], json!([]));
    }

    #[test]
    fn member_ids_serialize_sorted_as_decimal_strings() {
        let value = project(
            "2026-11-03T10:00:00Z".to_owned(),
            vec![draft(10, "lobby", Some(0), &[300, 42], false)],
        )
        .expect("small projection fits");
        assert_eq!(value["rooms"][0]["member_ids"], json!(["42", "300"]));
    }

    #[test]
    fn shape_carries_version_and_whole_second_timestamp() {
        let value = project(
            "2026-11-03T10:00:00Z".to_owned(),
            vec![draft(10, "lobby", Some(0), &[7], false)],
        )
        .expect("small projection fits");
        assert_eq!(value["v"], json!(1));
        assert_eq!(value["observed_at"], json!("2026-11-03T10:00:00Z"));
        assert_eq!(value["rooms"][0]["channel_id"], json!("10"));
        assert_eq!(value["rooms"][0]["position"], json!(0));
    }

    #[test]
    fn oversize_projection_refuses_over_budget() {
        // ~20 bytes per member id: five thousand occupants blow the 64 KiB
        // relay bound the Worker enforces on the whole body.
        let members: Vec<Snowflake> = (1..=5000).map(|n| 100_000_000_000_000_000 + n).collect();
        let err = project(
            "2026-11-03T10:00:00Z".to_owned(),
            vec![draft(10, "crowd", Some(0), &members, false)],
        )
        .expect_err("crowded projection exceeds the relay budget");
        assert_eq!(err, SnapshotRefusal::OverBudget);
    }
}
