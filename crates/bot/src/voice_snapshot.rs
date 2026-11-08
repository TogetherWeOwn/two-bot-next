//! Keyless `voice.snapshot` projection over the shared gateway cache.
//!
//! The signed `POST /internal/actions` receiver serves this verb from the
//! same [`DefaultInMemoryCache`] the gateway loop feeds through the
//! pipeline: Discord exposes no REST voice-state list, so the live cache is
//! the only source. A cache that cannot prove a complete guild view refuses
//! with [`SnapshotRefusal::CacheIncomplete`] (the receiver answers
//! `discord_unavailable`), never a partial listing and never invented data.
//!
//! Shape v1 (`result` beside the correlated id, `Cache-Control: no-store`
//! like `event.read`):
//! `{"v":1,"observed_at":"<Zulu whole seconds>","total_in_voice":N,
//! "rooms":[{"channel_id":"<snowflake>","name":"...","position":N|null,
//! "user_limit":N,"member_ids":["<snowflake>",...],"afk":true?,
//! "occupied_since":"<Zulu whole seconds>"|null,"event_key":"..."?}]}`.
//!
//! Projection rules: guild voice channels only (stage and text excluded);
//! every voice channel is a room even when empty; the AFK channel carries
//! `"afk": true` and its occupants stay listed but leave `total_in_voice`
//! out; bots leave both the member lists and the total out; a member the
//! cache has not populated counts as human (never delete on incomplete bot
//! data); member ids are snowflake decimal strings only — no usernames,
//! nicknames, presence or activity; rooms sort by position then name.
//! `occupied_since` is the ISO-8601 UTC instant the room's current
//! continuous human occupancy began (retained until the room empties), or
//! `null` when unknown — an empty room, or a join the gateway never
//! witnessed (process start, reconnect reseed) is unknown, never guessed.
//! `event_key` rides only on the room bound to a currently-windowed event's
//! voice channel and is omitted elsewhere. Member ids never reach logs; the
//! read audits through the receiver's counters only.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

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
    /// Whole-second Zulu join time of the room's current continuous
    /// occupancy (`None` = unknown: empty room, or a join the gateway never
    /// witnessed). Always serialized: a string when proven, else JSON null.
    pub occupied_since: Option<String>,
    /// `internal_event_keys` key for the currently-windowed event bound to
    /// this room's voice channel (`None` = unlinked room, omitted on the
    /// wire).
    pub event_key: Option<String>,
}

/// A room's current interval, never a per-member join history. Unknown starts
/// remain unknown until the room empties, even if all original occupants leave.
#[derive(Debug)]
struct OccupiedRoom {
    members: HashSet<Snowflake>,
    since_ms: Option<u64>,
}

/// Memory-only continuous occupancy clock, independent of the gated room actor.
/// The pipeline supplies the affected rooms' post-transition human rosters.
/// A witnessed join into an empty room starts an interval; overlapping joins
/// and leaves preserve it until empty. Reconnects and reseeds discard evidence.
/// Reads compare rosters before using evidence, so a cache/observer race is
/// unknown rather than a fabricated start.
#[derive(Debug, Default)]
pub(crate) struct VoiceOccupancyClock {
    rooms: Mutex<HashMap<(Snowflake, Snowflake), OccupiedRoom>>,
    /// Move admissions share this always-on gateway observer, not the gated
    /// voice-room actor. The lock is never held across an await.
    pub(crate) move_admissions: Mutex<crate::voice_move::VoiceMoveAdmissions>,
}

impl VoiceOccupancyClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn update_room(
        rooms: &mut HashMap<(Snowflake, Snowflake), OccupiedRoom>,
        guild_id: Snowflake,
        channel_id: Snowflake,
        members: &[Snowflake],
        member_id: Snowflake,
        joining: bool,
        occurred_at_ms: Option<u64>,
    ) {
        let key = (guild_id, channel_id);
        let members: HashSet<_> = members.iter().copied().collect();
        if members.is_empty() {
            rooms.remove(&key);
            return;
        }
        let mut before = members.clone();
        if joining {
            before.remove(&member_id);
        } else {
            before.insert(member_id);
        }
        let since_ms = match rooms.get(&key) {
            Some(room) if room.members == before => room.since_ms,
            _ if joining && before.is_empty() && members.contains(&member_id) => occurred_at_ms,
            _ => None,
        };
        rooms.insert(key, OccupiedRoom { members, since_ms });
    }

    fn stamp_rooms(&self, guild_id: Snowflake, drafts: &mut [RoomDraft]) {
        let rooms = self.rooms.lock().expect("voice occupancy clock");
        // Drafts may predate a gateway mutation. Reads must not delete newer
        // evidence; witnessed leaves and lifecycle resets own all pruning.
        for draft in drafts {
            let members: HashSet<_> = draft.member_ids.iter().copied().collect();
            draft.occupied_since = rooms
                .get(&(guild_id, draft.channel_id))
                .filter(|room| room.members == members)
                .and_then(|room| room.since_ms)
                .and_then(format_whole_second_zulu);
        }
    }
}

impl two_bot_discord::VoiceTransitionObserver for VoiceOccupancyClock {
    fn observe_voice_transition(&self, transition: two_bot_discord::VoiceTransitionObservation) {
        {
            let mut admissions = self.move_admissions.lock().expect("voice move admissions");
            admissions.observe(transition.member_id, transition.current_channel_id);
            // A reseed may prove an arrival without emitting a transition.
            // A later witnessed departure from that target also settles it.
            admissions.observe(transition.member_id, transition.previous_channel_id);
        }
        if transition.is_bot {
            return;
        }
        let mut rooms = self.rooms.lock().expect("voice occupancy clock");
        if let Some(channel) = transition.previous_channel_id {
            Self::update_room(
                &mut rooms,
                transition.guild_id,
                channel,
                &transition.previous_channel_members,
                transition.member_id,
                false,
                transition.occurred_at_ms,
            );
        }
        if let Some(channel) = transition.current_channel_id {
            Self::update_room(
                &mut rooms,
                transition.guild_id,
                channel,
                &transition.current_channel_members,
                transition.member_id,
                true,
                transition.occurred_at_ms,
            );
        }
    }

    fn reset_voice_occupancy(&self, guild_id: Option<u64>, channel_id: Option<u64>) {
        let mut rooms = self.rooms.lock().expect("voice occupancy clock");
        rooms.retain(|(guild, channel), _| {
            guild_id.is_some_and(|reset_guild| *guild != reset_guild)
                || (guild_id.is_some()
                    && channel_id.is_some_and(|reset_channel| *channel != reset_channel))
        });
    }
}

/// Read the §1 snapshot from the live gateway cache. `None` anywhere on the
/// guild → channels → voice-states → per-state lookup chain is
/// [`SnapshotRefusal::CacheIncomplete`]; see [`collect_rooms`] for the one
/// deliberate exception (a guild the cache holds whose voice-state set went
/// missing reads as empty, not incomplete).
///
/// `event_keys` carries the currently-windowed voice-channel → event-key map
/// behind the read; each draft takes its room's key, or none when unlinked.
/// The clock stamps `occupied_since` from the witnessed joins before
/// projection.
///
/// The cache alone cannot prove freshness: a transport loss leaves the last
/// occupancy readable, so callers gate on the supervisor's readiness first
/// and only call this while the gateway is connected.
pub(crate) fn read_voice_snapshot(
    cache: &DefaultInMemoryCache,
    clock: &VoiceOccupancyClock,
    guild_id: Snowflake,
    event_keys: &HashMap<Snowflake, String>,
) -> Result<Value, SnapshotRefusal> {
    let mut rooms = collect_rooms(cache, guild_id).ok_or(SnapshotRefusal::CacheIncomplete)?;
    clock.stamp_rooms(guild_id, &mut rooms);
    for room in &mut rooms {
        room.event_key = event_keys.get(&room.channel_id).cloned();
    }
    project(observed_at_now(), rooms)
}

/// Whole-second Zulu timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for `observed_at`.
fn observed_at_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    format_whole_second_zulu_parts(
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Whole-second Zulu timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for a receipt-time
/// millis value. Out-of-range inputs leave the start unknown rather than
/// substituting a timestamp the gateway never witnessed.
fn format_whole_second_zulu(millis: u64) -> Option<String> {
    let at = i64::try_from(millis.div_euclid(1000))
        .ok()
        .and_then(|secs| time::OffsetDateTime::from_unix_timestamp(secs).ok())?;
    Some(format_whole_second_zulu_parts(
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    ))
}

fn format_whole_second_zulu_parts(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
) -> String {
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Shared-cache handle constructor for the binary wiring (kept beside the
/// reader so `main.rs` needs no twilight imports of its own).
pub(crate) fn new_shared_cache() -> Arc<DefaultInMemoryCache> {
    Arc::new(two_bot_discord::build_cache())
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
    let afk_channel: Option<Id<ChannelMarker>> = guild.afk_channel_id();
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
            // Stamped by the occupancy clock and the event-key map on the
            // read path; collection alone proves neither.
            occupied_since: None,
            event_key: None,
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
    let total_in_voice: usize = rooms
        .iter()
        .filter(|room| !room.afk)
        .map(|room| room.member_ids.len())
        .sum();
    let wire_rooms: Vec<Value> = rooms
        .iter()
        .map(|room| {
            let mut wire = json!({
                "channel_id": room.channel_id.to_string(),
                "name": room.name,
                "position": room.position,
                "user_limit": room.user_limit,
                "member_ids": room.member_ids.iter().map(Snowflake::to_string).collect::<Vec<_>>(),
                // Always present: a proven join instant, else explicit null.
                // Unknown occupancy is data, not an absent field.
                "occupied_since": room.occupied_since.clone(),
            });
            if room.afk {
                wire["afk"] = Value::Bool(true);
            }
            // Present only on the room bound to a currently-windowed event's
            // voice channel; unlinked rooms omit the key entirely.
            if let Some(key) = room.event_key.as_deref() {
                wire["event_key"] = Value::String(key.to_owned());
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
            occupied_since: None,
            event_key: None,
        }
    }

    /// Whole-second Zulu for a fixed instant: 2026-11-03T10:00:00Z.
    fn stamp_ms() -> u64 {
        1_793_700_000_000
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
    fn rooms_without_proof_read_null_occupied_since() {
        // Collection alone proves no join: the wire carries explicit null,
        // never an absent field and never a guess.
        let value = project(
            "2026-11-03T10:00:00Z".to_owned(),
            vec![
                draft(10, "lobby", Some(0), &[7], false),
                draft(11, "quiet", Some(1), &[], false),
            ],
        )
        .expect("small projection fits");
        assert_eq!(value["rooms"][0]["occupied_since"], json!(null));
        assert_eq!(value["rooms"][1]["occupied_since"], json!(null));
    }

    #[test]
    fn event_key_rides_only_on_the_linked_room() {
        let mut rooms = vec![
            draft(10, "lobby", Some(0), &[7], false),
            draft(11, "stage-linked", Some(1), &[8], false),
        ];
        rooms[1].event_key = Some("autumn-showcase".to_owned());
        let value =
            project("2026-11-03T10:00:00Z".to_owned(), rooms).expect("small projection fits");
        assert!(value["rooms"][0].get("event_key").is_none());
        assert_eq!(value["rooms"][1]["event_key"], json!("autumn-showcase"));
    }

    fn transition(
        member_id: u64,
        previous: Option<(u64, &[u64])>,
        current: Option<(u64, &[u64])>,
        occurred_at_ms: Option<u64>,
    ) -> two_bot_discord::VoiceTransitionObservation {
        two_bot_discord::VoiceTransitionObservation {
            guild_id: 9,
            member_id,
            previous_channel_id: previous.map(|(channel, _)| channel),
            current_channel_id: current.map(|(channel, _)| channel),
            previous_channel_members: previous
                .map(|(_, members)| members.to_vec())
                .unwrap_or_default(),
            current_channel_members: current
                .map(|(_, members)| members.to_vec())
                .unwrap_or_default(),
            is_bot: false,
            occurred_at_ms,
        }
    }

    #[test]
    fn clock_stamps_the_oldest_tracked_join() {
        use two_bot_discord::VoiceTransitionObserver;
        let clock = VoiceOccupancyClock::new();
        clock.observe_voice_transition(transition(7, None, Some((10, &[7])), Some(stamp_ms())));
        clock.observe_voice_transition(transition(
            8,
            None,
            Some((10, &[7, 8])),
            Some(stamp_ms() + 60_000),
        ));
        let mut rooms = vec![draft(10, "lobby", Some(0), &[7, 8], false)];
        clock.stamp_rooms(9, &mut rooms);
        assert_eq!(
            rooms[0].occupied_since.as_deref(),
            Some("2026-11-03T10:00:00Z")
        );
        let value =
            project("2026-11-03T10:00:00Z".to_owned(), rooms).expect("small projection fits");
        assert_eq!(
            value["rooms"][0]["occupied_since"],
            json!("2026-11-03T10:00:00Z")
        );
    }

    #[test]
    fn clock_out_of_range_stamp_reads_null() {
        use two_bot_discord::VoiceTransitionObserver;
        let clock = VoiceOccupancyClock::new();
        clock.observe_voice_transition(transition(7, None, Some((10, &[7])), Some(u64::MAX)));
        let mut rooms = vec![draft(10, "lobby", Some(0), &[7], false)];
        clock.stamp_rooms(9, &mut rooms);
        assert_eq!(rooms[0].occupied_since, None);
    }

    #[test]
    fn move_admissions_reconcile_without_an_intervening_http_read() {
        use two_bot_discord::VoiceTransitionObserver;
        let clock = VoiceOccupancyClock::new();
        clock
            .move_admissions
            .lock()
            .unwrap()
            .reserve(7, 11)
            .unwrap();
        // A stale source observation cannot release an unobserved move.
        clock.observe_voice_transition(transition(7, None, Some((10, &[7])), None));
        assert!(clock
            .move_admissions
            .lock()
            .unwrap()
            .reserve(7, 11)
            .is_err());
        clock.observe_voice_transition(transition(7, Some((10, &[])), Some((11, &[7])), None));
        clock.observe_voice_transition(transition(7, Some((11, &[])), None, None));
        assert!(clock.move_admissions.lock().unwrap().reserve(7, 11).is_ok());
        let mut bot = transition(7, None, Some((11, &[])), None);
        bot.is_bot = true;
        clock.observe_voice_transition(bot);
        assert!(clock.move_admissions.lock().unwrap().reserve(7, 11).is_ok());
    }

    #[test]
    fn clock_moves_restamp_and_leaves_clear() {
        use two_bot_discord::VoiceTransitionObserver;
        let clock = VoiceOccupancyClock::new();
        clock.observe_voice_transition(transition(7, None, Some((10, &[7])), Some(stamp_ms())));
        clock.observe_voice_transition(transition(
            7,
            Some((10, &[])),
            Some((11, &[7])),
            Some(stamp_ms() + 60_000),
        ));
        let mut rooms = vec![
            draft(10, "lobby", Some(0), &[], false),
            draft(11, "breakout", Some(1), &[7], false),
        ];
        clock.stamp_rooms(9, &mut rooms);
        assert_eq!(rooms[0].occupied_since, None);
        assert_eq!(
            rooms[1].occupied_since.as_deref(),
            Some("2026-11-03T10:01:00Z")
        );
        clock.observe_voice_transition(transition(
            7,
            Some((11, &[])),
            None,
            Some(stamp_ms() + 120_000),
        ));
        clock.stamp_rooms(9, &mut rooms);
        assert_eq!(rooms[1].occupied_since, None);
        assert!(clock.rooms.lock().expect("clock lock").is_empty());
    }

    #[test]
    fn clock_unknown_stamp_or_member_reads_null() {
        use two_bot_discord::VoiceTransitionObserver;
        let clock = VoiceOccupancyClock::new();
        clock.observe_voice_transition(transition(7, None, Some((10, &[7])), None));
        let mut rooms = vec![draft(10, "lobby", Some(0), &[7, 8], false)];
        clock.stamp_rooms(9, &mut rooms);
        assert_eq!(rooms[0].occupied_since, None);
        clock.observe_voice_transition(transition(8, None, Some((11, &[8])), Some(stamp_ms())));
        clock.stamp_rooms(9, &mut rooms);
        assert_eq!(rooms[0].occupied_since, None);
    }

    #[test]
    fn clock_read_refuses_roster_mismatch_without_deleting_newer_evidence() {
        use two_bot_discord::VoiceTransitionObserver;
        let clock = VoiceOccupancyClock::new();
        clock.observe_voice_transition(transition(7, None, Some((10, &[7])), Some(stamp_ms())));
        clock.observe_voice_transition(transition(8, None, Some((10, &[7, 8])), Some(stamp_ms())));
        let mut rooms = vec![draft(10, "lobby", Some(0), &[7], false)];
        clock.stamp_rooms(9, &mut rooms);
        assert_eq!(
            rooms[0].occupied_since, None,
            "unwitnessed change is unknown"
        );
        assert_eq!(clock.rooms.lock().expect("clock lock").len(), 1);
        // An older read may omit a newly created channel. It cannot erase
        // an interval recorded by the gateway after collection began.
        clock.stamp_rooms(9, &mut []);
        rooms[0].member_ids = vec![7, 8];
        clock.stamp_rooms(9, &mut rooms);
        assert_eq!(
            rooms[0].occupied_since.as_deref(),
            Some("2026-11-03T10:00:00Z")
        );
        clock.reset_voice_occupancy(Some(9), Some(10));
        assert!(clock.rooms.lock().expect("clock lock").is_empty());
    }

    #[test]
    fn clock_retains_known_and_unknown_starts_across_overlapping_members() {
        use two_bot_discord::VoiceTransitionObserver;
        for witnessed_first in [false, true] {
            let clock = VoiceOccupancyClock::new();
            if witnessed_first {
                clock.observe_voice_transition(transition(
                    7,
                    None,
                    Some((10, &[7])),
                    Some(stamp_ms()),
                ));
            }
            clock.observe_voice_transition(transition(
                8,
                None,
                Some((10, &[7, 8])),
                Some(stamp_ms() + 60_000),
            ));
            // The first occupant leaves. The room never emptied, so its start
            // cannot become B's later join (nor known if A predates startup).
            clock.observe_voice_transition(transition(
                7,
                Some((10, &[8])),
                None,
                Some(stamp_ms() + 120_000),
            ));
            let mut rooms = vec![draft(10, "lobby", Some(0), &[8], false)];
            clock.stamp_rooms(9, &mut rooms);
            assert_eq!(
                rooms[0].occupied_since.as_deref(),
                witnessed_first.then_some("2026-11-03T10:00:00Z")
            );
            clock.observe_voice_transition(transition(
                8,
                Some((10, &[])),
                None,
                Some(stamp_ms() + 180_000),
            ));
            clock.observe_voice_transition(transition(
                9,
                None,
                Some((10, &[9])),
                Some(stamp_ms() + 240_000),
            ));
            rooms[0].member_ids = vec![9];
            clock.stamp_rooms(9, &mut rooms);
            assert_eq!(
                rooms[0].occupied_since.as_deref(),
                Some("2026-11-03T10:04:00Z")
            );
        }
    }

    #[test]
    fn clock_resets_scoped_evidence_and_ignores_bots() {
        use two_bot_discord::VoiceTransitionObserver;
        let clock = VoiceOccupancyClock::new();
        for channel in [10, 11] {
            clock.observe_voice_transition(transition(
                7,
                None,
                Some((channel, &[7])),
                Some(stamp_ms()),
            ));
        }
        let mut other = transition(7, None, Some((10, &[7])), Some(stamp_ms()));
        other.guild_id = 99;
        clock.observe_voice_transition(other);
        clock.reset_voice_occupancy(Some(9), Some(10));
        let mut rooms = vec![
            draft(10, "a", Some(0), &[7], false),
            draft(11, "b", Some(1), &[7], false),
        ];
        clock.stamp_rooms(9, &mut rooms);
        assert_eq!(rooms[0].occupied_since, None);
        assert!(rooms[1].occupied_since.is_some());
        clock.reset_voice_occupancy(Some(9), None);
        assert_eq!(
            clock.rooms.lock().expect("clock lock").len(),
            1,
            "other guild survives"
        );
        clock.reset_voice_occupancy(None, None);
        assert!(clock.rooms.lock().expect("clock lock").is_empty());
        let mut bot = transition(88, None, Some((10, &[])), Some(stamp_ms()));
        bot.is_bot = true;
        clock.observe_voice_transition(bot);
        assert!(clock.rooms.lock().expect("clock lock").is_empty());
    }

    fn pipeline_voice(member: u64, channel: Option<u64>) -> twilight_model::gateway::event::Event {
        twilight_model::gateway::event::Event::VoiceStateUpdate(Box::new(
            serde_json::from_value(json!({
                "guild_id": "100", "user_id": member.to_string(),
                "channel_id": channel.map(|id| id.to_string()), "session_id": "fixture",
                "deaf": false, "mute": false, "self_deaf": false,
                "self_mute": false, "self_video": false, "suppress": false
            }))
            .expect("voice frame"),
        ))
    }

    fn pipeline_guild(members: &[u64]) -> twilight_model::gateway::event::Event {
        use twilight_model::gateway::{event::Event, payload::incoming::GuildCreate};
        let mut guild: Value =
            serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json"))
                .expect("guild fixture");
        guild["voice_states"] = Value::Array(
            members
                .iter()
                .map(|member| {
                    let Event::VoiceStateUpdate(state) = pipeline_voice(*member, Some(200)) else {
                        unreachable!("voice fixture")
                    };
                    serde_json::to_value(state).expect("voice state")
                })
                .collect(),
        );
        Event::GuildCreate(Box::new(GuildCreate::Available(
            serde_json::from_value(guild).expect("guild fixture"),
        )))
    }

    #[test]
    fn pipeline_reseed_then_departure_reconciles_move_without_http_read() {
        for destination in [None, Some(201)] {
            let pipeline = two_bot_discord::MemPipeline::for_replay();
            let clock = VoiceOccupancyClock::new();
            pipeline.set_voice_transition_observer(clock.clone());
            clock
                .move_admissions
                .lock()
                .unwrap()
                .reserve(7, 200)
                .unwrap();
            pipeline.handle_at(&pipeline_guild(&[7]), "2026-11-03T10:00:00Z");
            assert!(clock
                .move_admissions
                .lock()
                .unwrap()
                .reserve(7, 200)
                .is_err());
            // GuildCreate reseeds occupancy without an arrival transition.
            pipeline.handle_at(&pipeline_voice(7, destination), "2026-11-03T10:01:00Z");
            assert!(clock
                .move_admissions
                .lock()
                .unwrap()
                .reserve(7, 200)
                .is_ok());
        }
    }

    #[test]
    fn pipeline_clock_preserves_intervals_and_forgets_same_member_after_a_gap() {
        use twilight_model::gateway::event::Event;
        for reseed in [false, true] {
            let pipeline = two_bot_discord::MemPipeline::for_replay();
            let clock = VoiceOccupancyClock::new();
            pipeline.set_voice_transition_observer(clock.clone());
            pipeline.handle_at(&pipeline_guild(&[]), "2026-11-03T09:59:00Z");
            pipeline.handle_at(&pipeline_voice(7, Some(200)), "2026-11-03T10:00:00Z");
            pipeline.handle_at(&pipeline_voice(8, Some(200)), "2026-11-03T10:01:00Z");
            let snapshot = || {
                read_voice_snapshot(pipeline.cache(), &clock, 100, &HashMap::new())
                    .expect("complete guild")
            };
            for member in [99, 7] {
                // An unrelated guild departure cannot erase this interval.
                // For an occupant, Twilight retains the voice state until the
                // actual leave frame; that leave must still preserve overlap.
                let removed = Event::MemberRemove(
                    serde_json::from_value(json!({
                        "guild_id": "100", "user": {"id": member.to_string(),
                        "username": "fixture", "discriminator": "0", "bot": false}
                    }))
                    .expect("member remove"),
                );
                pipeline.handle_at(&removed, "2026-11-03T10:01:30Z");
                assert_eq!(
                    snapshot()["rooms"][0]["occupied_since"],
                    json!("2026-11-03T10:00:00Z")
                );
            }
            pipeline.handle_at(&pipeline_voice(7, None), "2026-11-03T10:02:00Z");
            assert_eq!(
                snapshot()["rooms"][0]["occupied_since"],
                json!("2026-11-03T10:00:00Z")
            );
            let reset = if reseed {
                pipeline_guild(&[8])
            } else {
                Event::Resumed
            };
            pipeline.handle_at(&reset, "2026-11-03T10:03:00Z");
            assert_eq!(snapshot()["rooms"][0]["member_ids"], json!(["8"]));
            assert_eq!(
                snapshot()["rooms"][0]["occupied_since"],
                Value::Null,
                "the same cached occupant cannot prove continuity after a gap"
            );
            pipeline.handle_at(&pipeline_voice(9, Some(200)), "2026-11-03T10:04:00Z");
            pipeline.handle_at(&pipeline_voice(8, None), "2026-11-03T10:05:00Z");
            assert_eq!(snapshot()["rooms"][0]["occupied_since"], Value::Null);
            pipeline.handle_at(&pipeline_voice(9, None), "2026-11-03T10:06:00Z");
            pipeline.handle_at(&pipeline_voice(10, Some(200)), "2026-11-03T10:07:00Z");
            assert_eq!(
                snapshot()["rooms"][0]["occupied_since"],
                json!("2026-11-03T10:07:00Z")
            );
        }
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
