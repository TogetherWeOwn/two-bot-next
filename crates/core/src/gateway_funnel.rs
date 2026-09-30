//! Synchronous funnel and invite seams staging one dispatch's database writes.
//! Effects and processed sequence commit together. Only milestone read models
//! and the current invite baseline survive a flush, not a growing event log.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::{
    EventType, FunnelEvent, FunnelStore, InviteSnapshotStore, InviteState, MemStore, RecordOutcome,
    Snowflake,
};

pub enum SnapshotWrite {
    StoreAll(Snowflake, Vec<InviteState>),
    DeleteMissing(Snowflake, HashSet<String>),
}

#[derive(Default)]
pub struct FunnelBatch {
    pub events: Vec<FunnelEvent>,
    pub activity: Vec<(Snowflake, Snowflake, String)>,
    pub bots: Vec<(Snowflake, Snowflake)>,
    pub snapshots: Vec<SnapshotWrite>,
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

    fn touch_activity(&self, guild_id: Snowflake, member_id: Snowflake, at: &str) {
        self.pending.lock().expect("funnel buffer").activity.push((
            guild_id,
            member_id,
            at.to_owned(),
        ));
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
        let mut snapshots = self.snapshots.lock().expect("snapshot buffer");
        let table = snapshots.entry(guild_id).or_default();
        for state in states {
            table.insert(state.code.clone(), state.clone());
        }
        self.pending
            .lock()
            .expect("funnel buffer")
            .snapshots
            .push(SnapshotWrite::StoreAll(guild_id, states.to_vec()));
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
    fn hydrated_baseline_and_staged_seed_do_not_prune_unrelated_codes() {
        let buffer = GatewayFunnelBuffer::default();
        let invite = |code: &str| InviteState {
            code: code.into(),
            uses: 1,
            inviter_id: None,
            channel_id: None,
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
}
