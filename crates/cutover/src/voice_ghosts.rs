//! Read-only ghost-channel count for the voice cutover (TOG-13548).
//!
//! Room tracking diffs otherwise live only inside the runtime `reconcile`
//! pass (`crates/bot/src/voice_rooms.rs`), surfaced via `/setup` health and
//! worker logs. This module is the pollable half: the existence diff of
//! tracked `voice_rooms` rows against the guild's live voice channels —
//! tracked-present, tracked-gone (the `ReconcilePlan::forget` class) and
//! untracked-present — with no deletes and no writes.
//!
//! Discord REST lists channels but not their voice occupants or the bot's
//! manageability on each one, so this diff is existence-only by
//! construction: occupancy classes (empty vs occupied, manageable vs
//! suspended) belong to a gateway-derived snapshot, not to this report. The
//! operator bridges the two by pairing this report's id lists with live
//! occupancy when building the cleanup tool's `--channels` snapshot.

use std::collections::HashSet;

use twilight_model::channel::ChannelType;
use two_bot_core::voice_rooms::VoiceRoom;
use two_bot_core::Snowflake;

/// Existence diff of tracked rooms against live voice channels. All three
/// lists hold channel ids sorted ascending for deterministic reports.
/// `tracked_gone` mirrors the runtime `reconcile` forget rule exactly:
/// tracked but absent from the live listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhostCounts {
    /// Tracked rows whose channel still exists.
    pub tracked_present: Vec<Snowflake>,
    /// Tracked rows whose channel is gone: forget candidates, never deletes.
    pub tracked_gone: Vec<Snowflake>,
    /// Live voice channels the bot never tracked: triage only, never touched.
    pub untracked_present: Vec<Snowflake>,
}

impl GhostCounts {
    /// Whether every tracked room still exists and no stranger appeared.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.tracked_gone.is_empty() && self.untracked_present.is_empty()
    }
}

/// Voice channel kinds: rooms, their creators and stage lounges all read as
/// voice here. Text, categories, threads, forums and directories never do —
/// a rehearsal guild full of text channels reports zero live voice.
#[must_use]
pub fn is_live_voice_kind(kind: ChannelType) -> bool {
    matches!(kind, ChannelType::GuildVoice | ChannelType::GuildStageVoice)
}

/// Diff `tracked` room rows against live voice channel ids. Pure set
/// arithmetic: no I/O, no writes, no occupancy claims.
#[must_use]
pub fn count_ghosts(tracked: &[VoiceRoom], live_voice: &[Snowflake]) -> GhostCounts {
    let live: HashSet<Snowflake> = live_voice.iter().copied().collect();
    let known: HashSet<Snowflake> = tracked.iter().map(|room| room.channel_id).collect();
    let mut present: Vec<Snowflake> = tracked
        .iter()
        .map(|room| room.channel_id)
        .filter(|id| live.contains(id))
        .collect();
    present.sort_unstable();
    let mut gone: Vec<Snowflake> = tracked
        .iter()
        .map(|room| room.channel_id)
        .filter(|id| !live.contains(id))
        .collect();
    gone.sort_unstable();
    let mut untracked: Vec<Snowflake> = live_voice
        .iter()
        .copied()
        .filter(|id| !known.contains(id))
        .collect();
    untracked.sort_unstable();
    GhostCounts {
        tracked_present: present,
        tracked_gone: gone,
        untracked_present: untracked,
    }
}

/// Seeded demo feeds: two present rooms, one gone, one untracked channel.
/// No database, no Discord; `--seed` prints the report for these.
#[must_use]
pub fn build_seed_ghost_data() -> (Vec<VoiceRoom>, Vec<Snowflake>) {
    let room = |channel: Snowflake| VoiceRoom {
        guild_id: 9,
        channel_id: channel,
        creator_channel_id: 7,
        owner_id: 41,
        original_creator_id: 41,
        name_seed: 1,
        created_at: "2026-09-20T12:00:00.000Z".to_owned(),
    };
    let tracked = vec![room(101), room(102), room(103)];
    // 101 and 102 still exist; 103 was deleted by hand; 201 was never tracked.
    let live = vec![101, 102, 201];
    (tracked, live)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(channel: Snowflake) -> VoiceRoom {
        VoiceRoom {
            guild_id: 9,
            channel_id: channel,
            creator_channel_id: 7,
            owner_id: 41,
            original_creator_id: 41,
            name_seed: 1,
            created_at: "2026-09-20T12:00:00.000Z".to_owned(),
        }
    }

    #[test]
    fn only_voice_and_stage_kinds_count_as_live() {
        assert!(is_live_voice_kind(ChannelType::GuildVoice));
        assert!(is_live_voice_kind(ChannelType::GuildStageVoice));
        for kind in [
            ChannelType::GuildText,
            ChannelType::GuildCategory,
            ChannelType::GuildAnnouncement,
            ChannelType::AnnouncementThread,
            ChannelType::PublicThread,
            ChannelType::PrivateThread,
            ChannelType::GuildDirectory,
            ChannelType::GuildForum,
            ChannelType::GuildMedia,
        ] {
            assert!(!is_live_voice_kind(kind), "{kind:?} must not read as voice");
        }
    }

    #[test]
    fn empty_guild_is_clean() {
        let counts = count_ghosts(&[], &[]);
        assert!(counts.is_clean());
        assert_eq!(counts, GhostCounts::default());
    }

    #[test]
    fn classifies_present_gone_and_untracked() {
        let tracked = vec![room(101), room(102), room(103)];
        let counts = count_ghosts(&tracked, &[102, 103, 201]);
        assert_eq!(counts.tracked_present, vec![102, 103]);
        assert_eq!(counts.tracked_gone, vec![101]);
        assert_eq!(counts.untracked_present, vec![201]);
        assert!(!counts.is_clean());
    }

    #[test]
    fn all_present_with_no_strangers_is_clean() {
        let tracked = vec![room(101), room(102)];
        let counts = count_ghosts(&tracked, &[101, 102]);
        assert!(counts.is_clean());
    }

    #[test]
    fn results_are_sorted_regardless_of_input_order() {
        let tracked = vec![room(300), room(100), room(200)];
        let counts = count_ghosts(&tracked, &[400, 200, 100, 300, 500]);
        assert_eq!(counts.tracked_present, vec![100, 200, 300]);
        assert_eq!(counts.tracked_gone, Vec::<Snowflake>::new());
        assert_eq!(counts.untracked_present, vec![400, 500]);
    }

    #[test]
    fn seed_feeds_cover_every_class() {
        let (tracked, live) = build_seed_ghost_data();
        let counts = count_ghosts(&tracked, &live);
        assert_eq!(counts.tracked_present, vec![101, 102]);
        assert_eq!(counts.tracked_gone, vec![103]);
        assert_eq!(counts.untracked_present, vec![201]);
    }

    #[test]
    fn tracked_gone_matches_runtime_reconcile_forget_ids() {
        use two_bot_core::voice_rooms::{reconcile, SeenChannel};

        // One fixture drives both halves: tracked rooms 101-103 against live
        // voice 101, 102 plus untracked interim-bot leftover 201.
        let tracked = vec![room(101), room(102), room(103)];
        let live = vec![101, 102, 201];
        let counts = count_ghosts(&tracked, &live);
        // Occupied and manageable, so the runtime plan isolates the forget
        // class: nothing to delete, nothing to suspend.
        let seen: Vec<SeenChannel> = live
            .iter()
            .map(|id| SeenChannel {
                channel_id: *id,
                human_occupants: 2,
                manageable: true,
            })
            .collect();
        let plan = reconcile(&tracked, &seen);
        let mut forget_ids: Vec<Snowflake> =
            plan.forget.iter().map(|room| room.channel_id).collect();
        forget_ids.sort_unstable();
        assert_eq!(counts.tracked_gone, forget_ids);
        assert_eq!(counts.tracked_gone, vec![103]);
        assert!(plan.delete_empty.is_empty());
        assert!(plan.suspend.is_empty());
        // The untracked leftover is triage-only: the runtime plan never
        // mentions it or the still-present rooms.
        assert_eq!(counts.untracked_present, vec![201]);
        let mentioned: Vec<Snowflake> = plan
            .forget
            .iter()
            .chain(plan.delete_empty.iter())
            .chain(plan.suspend.iter())
            .map(|room| room.channel_id)
            .collect();
        assert!(!mentioned.contains(&201));
        assert!(!mentioned.contains(&101));
        assert!(!mentioned.contains(&102));
    }
}
