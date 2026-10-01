//! Synchronous funnel seam that stages one dispatch's database writes. The
//! shard runner commits the batch and processed sequence in one transaction.
//! Only milestone read models survive a flush; repeatable rows do not build an
//! unbounded in-memory event log.

use std::collections::HashSet;
use std::sync::Mutex;

use crate::{EventType, FunnelEvent, FunnelStore, InviteState, MemStore, RecordOutcome, Snowflake};

/// A successful counter read replaces the guild snapshot. InviteCreate only
/// upserts its one code: it says nothing about other invites still being live.
pub struct InviteSnapshotWrite {
    pub guild_id: Snowflake,
    pub states: Vec<InviteState>,
    pub observed_at: String,
    pub replace_all: bool,
}

#[derive(Default)]
pub struct FunnelBatch {
    pub events: Vec<FunnelEvent>,
    pub activity: Vec<(Snowflake, Snowflake, String)>,
    pub invite_snapshots: Vec<InviteSnapshotWrite>,
}

#[derive(Default)]
pub struct GatewayFunnelBuffer {
    milestones: MemStore,
    pending: Mutex<FunnelBatch>,
    keys: Mutex<HashSet<String>>,
}

impl GatewayFunnelBuffer {
    pub fn from_milestones(events: Vec<FunnelEvent>) -> Self {
        let buffer = Self::default();
        for event in events {
            buffer.milestones.record(event);
        }
        buffer
    }

    /// Called only by the serial shard runner, after handling one dispatch.
    pub fn take_batch(&self) -> FunnelBatch {
        self.keys.lock().expect("buffer keys").clear();
        std::mem::take(&mut *self.pending.lock().expect("funnel buffer"))
    }
}

impl FunnelStore for GatewayFunnelBuffer {
    fn stage_invite_snapshot(&self, snapshot: InviteSnapshotWrite) {
        self.pending
            .lock()
            .expect("funnel buffer")
            .invite_snapshots
            .push(snapshot);
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
