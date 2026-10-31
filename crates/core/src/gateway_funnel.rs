//! Synchronous funnel and invite seams staging one dispatch's database writes.
//! Effects and processed sequence commit together. Only milestone read models
//! and the current invite baseline survive a flush, not a growing event log.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::analytics_family::AnalyticsCategory;
use crate::{
    EventType, FunnelEvent, FunnelStore, InviteSnapshotStore, InviteState, MemStore, RecordOutcome,
    Snowflake,
};

pub enum SnapshotWrite {
    StoreAll(Snowflake, Vec<InviteState>),
    DeleteMissing(Snowflake, HashSet<String>),
}

/// A successful counter read replaces the guild snapshot. InviteCreate only
/// upserts its one code: it says nothing about other invites still being live.
pub struct InviteSnapshotWrite {
    pub guild_id: Snowflake,
    pub states: Vec<InviteState>,
    pub observed_at: String,
    pub replace_all: bool,
}

/// One timestamp-buffer entry plus the analytics family of the touch that
/// staged it. The commit gate needs the origin: a voice-only touch must not
/// advance `last_active_at` while the guild's voice family is disabled, even
/// though the entry itself carries no event row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityTouch {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub at: String,
    pub family: AnalyticsCategory,
}

#[derive(Default)]
pub struct FunnelBatch {
    pub events: Vec<FunnelEvent>,
    pub activity: Vec<ActivityTouch>,
    pub bots: Vec<(Snowflake, Snowflake)>,
    pub snapshots: Vec<SnapshotWrite>,
    pub invite_snapshots: Vec<InviteSnapshotWrite>,
}

#[derive(Clone, Default)]
pub struct GatewayFunnelBuffer {
    milestones: Arc<MemStore>,
    pending: Arc<Mutex<FunnelBatch>>,
    keys: Arc<Mutex<HashSet<String>>>,
    snapshots: Arc<Mutex<HashMap<Snowflake, HashMap<String, InviteState>>>>,
}

impl GatewayFunnelBuffer {
    pub fn from_milestones(events: Vec<FunnelEvent>) -> Self {
        let buffer = Self::default();
        for event in events {
            buffer.milestones.record(event);
        }
        buffer
    }

    /// Hydrate a durable baseline before dispatch starts, without staging writes.
    pub fn seed_snapshots(&self, guild_id: Snowflake, states: Vec<InviteState>) {
        self.snapshots.lock().expect("snapshot buffer").insert(
            guild_id,
            states
                .into_iter()
                .map(|state| (state.code.clone(), state))
                .collect(),
        );
    }

    /// Called only by the serial dispatch worker, after handling one dispatch.
    pub fn take_batch(&self) -> FunnelBatch {
        self.keys.lock().expect("buffer keys").clear();
        std::mem::take(&mut *self.pending.lock().expect("funnel buffer"))
    }
}

impl FunnelStore for GatewayFunnelBuffer {
    fn stage_invite_snapshot(&self, mut snapshot: InviteSnapshotWrite) {
        // Clamp before the counterpart match: the queued `StoreAll` tail this
        // replaces was already clamped at ingest, so an unclamped staged copy
        // would miss the match and write twice. Silent: the ingest warn above
        // already named this code.
        for state in &mut snapshot.states {
            state.uses = crate::invites::saturate_invite_uses(state.uses);
        }
        let mut pending = self.pending.lock().expect("funnel buffer");
        // The persistent tracker has just staged this same mutation through
        // InviteSnapshotStore. Replace only that matching tail with the receipt-
        // stamped form, not unrelated/directly staged snapshot writes.
        let counterpart = if snapshot.replace_all {
            let live: HashSet<String> = snapshot.states.iter().map(|s| s.code.clone()).collect();
            matches!(pending.snapshots.as_slice(), [..,
                SnapshotWrite::StoreAll(guild, states),
                SnapshotWrite::DeleteMissing(prune_guild, codes)]
                if *guild == snapshot.guild_id && *prune_guild == snapshot.guild_id
                    && states == &snapshot.states && codes == &live)
        } else {
            matches!(pending.snapshots.last(), Some(SnapshotWrite::StoreAll(guild, states))
                if *guild == snapshot.guild_id && states == &snapshot.states)
        };
        if counterpart {
            let retained = pending.snapshots.len() - if snapshot.replace_all { 2 } else { 1 };
            pending.snapshots.truncate(retained);
        }
        pending.invite_snapshots.push(snapshot);
    }

    fn mark_bot(&self, guild_id: Snowflake, member_id: Snowflake) {
        self.pending
            .lock()
            .expect("funnel buffer")
            .bots
            .push((guild_id, member_id));
    }

    fn record(&self, event: FunnelEvent) -> RecordOutcome {
        if !self
            .keys
            .lock()
            .expect("buffer keys")
            .insert(crate::idempotency_key(&event))
        {
            return RecordOutcome { inserted: false };
        }
        if (crate::MESSAGE_RUNGS.contains(&event.event_type)
            || event.event_type == EventType::FirstVoiceSession)
            && !self.milestones.record(event.clone()).inserted
        {
            return RecordOutcome { inserted: false };
        }
        self.pending
            .lock()
            .expect("funnel buffer")
            .events
            .push(event);
        RecordOutcome { inserted: true }
    }

    fn touch_activity(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        at: &str,
        family: AnalyticsCategory,
    ) {
        self.pending
            .lock()
            .expect("funnel buffer")
            .activity
            .push(ActivityTouch {
                guild_id,
                member_id,
                at: at.to_owned(),
                family,
            });
    }

    fn next_message_rung(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        at: &str,
    ) -> Option<EventType> {
        self.milestones.next_message_rung(guild_id, member_id, at)
    }

    fn has_event(&self, guild_id: Snowflake, member_id: Snowflake, event_type: EventType) -> bool {
        self.milestones.has_event(guild_id, member_id, event_type)
    }
}

impl InviteSnapshotStore for GatewayFunnelBuffer {
    fn load(&self, guild_id: Snowflake) -> Vec<InviteState> {
        self.snapshots
            .lock()
            .expect("snapshot buffer")
            .get(&guild_id)
            .map_or_else(Vec::new, |states| states.values().cloned().collect())
    }

    fn store_all(&self, guild_id: Snowflake, states: &[InviteState]) {
        // Discord counters are `u64`; the column is INTEGER. Clamp at ingest
        // so the queued flush can never poison the dispatch transaction, and
        // warn once per saturated code (identifiers only, never the counter).
        let mut states = states.to_vec();
        for state in &mut states {
            let uses = crate::invites::saturate_invite_uses(state.uses);
            if uses != state.uses {
                tracing::warn!(
                    guild_id = %guild_id,
                    code = %state.code,
                    "invite uses saturated at i32::MAX",
                );
                state.uses = uses;
            }
        }
        let mut snapshots = self.snapshots.lock().expect("snapshot buffer");
        let table = snapshots.entry(guild_id).or_default();
        for state in &states {
            table.insert(state.code.clone(), state.clone());
        }
        self.pending
            .lock()
            .expect("funnel buffer")
            .snapshots
            .push(SnapshotWrite::StoreAll(guild_id, states));
    }

    fn delete_missing(&self, guild_id: Snowflake, live_codes: &HashSet<String>) {
        if let Some(table) = self
            .snapshots
            .lock()
            .expect("snapshot buffer")
            .get_mut(&guild_id)
        {
            table.retain(|code, _| live_codes.contains(code));
        }
        self.pending
            .lock()
            .expect("funnel buffer")
            .snapshots
            .push(SnapshotWrite::DeleteMissing(guild_id, live_codes.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_snapshots_replace_only_their_tracker_staging_counterpart() {
        let buffer = GatewayFunnelBuffer::default();
        let invite = |code: &str| InviteState {
            code: code.into(),
            uses: 1,
            inviter_id: Some(9),
            channel_id: Some(8),
            max_uses: 0,
            temporary: false,
        };
        // A direct write for a different code is not part of the tracker tail.
        buffer.store_all(1, &[invite("direct")]);
        buffer.store_all(1, &[invite("created")]);
        buffer.stage_invite_snapshot(InviteSnapshotWrite {
            guild_id: 1,
            states: vec![invite("created")],
            observed_at: "2026-09-30T04:00:00.000Z".into(),
            replace_all: false,
        });
        let batch = buffer.take_batch();
        assert!(matches!(batch.snapshots.as_slice(),
            [SnapshotWrite::StoreAll(1, states)] if states[0].code == "direct"));
        assert_eq!(batch.invite_snapshots.len(), 1);
        assert!(!batch.invite_snapshots[0].replace_all);
        assert_eq!(batch.invite_snapshots[0].states[0].inviter_id, Some(9));

        buffer.store_all(1, &[invite("created")]);
        buffer.delete_missing(1, &HashSet::from(["created".into()]));
        buffer.stage_invite_snapshot(InviteSnapshotWrite {
            guild_id: 1,
            states: vec![invite("created")],
            observed_at: "2026-09-30T04:00:01.000Z".into(),
            replace_all: true,
        });
        let batch = buffer.take_batch();
        assert!(
            batch.snapshots.is_empty(),
            "full reads must not write twice"
        );
        assert_eq!(batch.invite_snapshots.len(), 1);
        assert!(batch.invite_snapshots[0].replace_all);
        assert_eq!(
            batch.invite_snapshots[0].observed_at,
            "2026-09-30T04:00:01.000Z"
        );
        assert_eq!(buffer.load(1), vec![invite("created")]);

        // Pipeline::new owns an in-memory tracker, with no legacy counterpart.
        buffer.store_all(2, &[invite("foreign")]);
        buffer.stage_invite_snapshot(InviteSnapshotWrite {
            guild_id: 1,
            states: Vec::new(),
            observed_at: "2026-09-30T04:00:02.000Z".into(),
            replace_all: true,
        });
        let batch = buffer.take_batch();
        assert!(matches!(
            batch.snapshots.as_slice(),
            [SnapshotWrite::StoreAll(2, _)]
        ));
        assert!(batch.invite_snapshots[0].states.is_empty());
        assert!(buffer.take_batch().invite_snapshots.is_empty());
    }

    #[test]
    fn hydrated_baseline_and_staged_seed_do_not_prune_unrelated_codes() {
        let buffer = GatewayFunnelBuffer::default();
        let invite = |code: &str| InviteState {
            code: code.into(),
            uses: 1,
            inviter_id: None,
            channel_id: None,
            max_uses: 0,
            temporary: false,
        };
        buffer.seed_snapshots(1, vec![invite("old")]);
        assert!(buffer.take_batch().snapshots.is_empty());
        let snapshots = buffer.clone();
        snapshots.store_all(1, &[invite("new")]);
        assert_eq!(buffer.load(1).len(), 2);
        let batch = buffer.take_batch();
        assert_eq!(batch.snapshots.len(), 1);
        snapshots.delete_missing(1, &HashSet::from(["new".into()]));
        assert_eq!(buffer.load(1)[0].code, "new");
        assert_eq!(buffer.take_batch().snapshots.len(), 1);
        assert!(buffer.take_batch().snapshots.is_empty());
    }

    #[test]
    fn store_all_saturates_out_of_range_uses_and_staging_still_coalesces() {
        let buffer = GatewayFunnelBuffer::default();
        let invite = |code: &str, uses: u64| InviteState {
            code: code.into(),
            uses,
            inviter_id: Some(9),
            channel_id: Some(8),
            max_uses: 0,
            temporary: false,
        };
        buffer.store_all(1, &[invite("hot", u64::MAX)]);
        assert_eq!(buffer.load(1)[0].uses, crate::invites::MAX_INVITE_USES);
        // The staged full read carries the raw Discord counter; the buffer
        // clamps it before the counterpart match so it still replaces the
        // tracker tail instead of writing twice.
        buffer.delete_missing(1, &HashSet::from(["hot".into()]));
        buffer.stage_invite_snapshot(InviteSnapshotWrite {
            guild_id: 1,
            states: vec![invite("hot", u64::MAX)],
            observed_at: "2026-10-21T00:00:00.000Z".into(),
            replace_all: true,
        });
        let batch = buffer.take_batch();
        assert!(
            batch.snapshots.is_empty(),
            "saturated full reads must not write twice"
        );
        assert_eq!(batch.invite_snapshots.len(), 1);
        assert_eq!(
            batch.invite_snapshots[0].states[0].uses,
            crate::invites::MAX_INVITE_USES
        );
        // A lone seed queues the clamped counter for the flush.
        buffer.store_all(2, &[invite("seed", u64::MAX)]);
        let batch = buffer.take_batch();
        assert!(matches!(batch.snapshots.as_slice(),
            [SnapshotWrite::StoreAll(2, states)]
            if states[0].uses == crate::invites::MAX_INVITE_USES));
    }
}
