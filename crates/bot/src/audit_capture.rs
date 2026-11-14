//! Bounded, volatile admission ahead of the existing durable private mirror.
//! Producers never await database/REST work or spawn per-event tasks. Once the
//! retry job records a row, the existing store owns crash recovery. Before that
//! boundary a process loss can lose rows; startup and overflow report the gap.

use std::{collections::VecDeque, sync::Mutex};
use two_bot_core::audit::AuditEvent;

pub(crate) const CAPACITY: usize = 256;
pub(crate) const BATCH: usize = 25;
const EVENT_BYTES: usize = 16_384;

#[derive(Default)]
pub(crate) struct CaptureQueue {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    rows: VecDeque<AuditEvent>,
    dropped: u64,
}

impl CaptureQueue {
    pub(crate) fn submit(&self, event: AuditEvent) {
        // Hard byte bound as well as a row bound. Serialize only metadata that
        // already passed a classifier: never retain an arbitrary gateway frame.
        let fits = serde_json::to_vec(&event).is_ok_and(|bytes| bytes.len() <= EVENT_BYTES);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !fits || state.rows.len() >= CAPACITY {
            state.dropped = state.dropped.saturating_add(1);
            // Logarithmic warnings during an outage, not a second flood.
            if state.dropped.is_power_of_two() {
                tracing::warn!(dropped = state.dropped, "audit_capture_gap");
            }
            return;
        }
        state.rows.push_back(event);
    }

    pub(crate) fn front(&self) -> Option<AuditEvent> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
            .front()
            .cloned()
    }

    pub(crate) fn recorded(&self, entry_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .rows
            .front()
            .is_some_and(|row| row.entry_id == entry_id)
        {
            state.rows.pop_front();
        }
    }

    pub(crate) fn counts(&self) -> (usize, u64) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.rows.len(), state.dropped)
    }
}

/// This is admission, not a durable receipt. No configured handle means no
/// capture, just as for the existing disabled audit runtime.
pub(crate) fn submit(event: AuditEvent) {
    if let Some(runtime) = crate::audit_runtime::handle() {
        runtime.submit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_bot_core::audit::AuditKind;

    fn event(id: usize) -> AuditEvent {
        AuditEvent::new(
            id.to_string(),
            AuditKind::ChannelChange,
            "1".into(),
            "2026-11-08T00:00:00.000Z".into(),
        )
    }

    #[test]
    fn full_queue_preserves_oldest_and_counts_loss_without_growing() {
        let queue = CaptureQueue::default();
        for id in 0..CAPACITY + 3 {
            queue.submit(event(id));
        }
        assert_eq!(queue.counts(), (CAPACITY, 3));
        assert_eq!(queue.front(), Some(event(0)));
        // A failed/uncertain write leaves the same identity for retry.
        assert_eq!(queue.front(), Some(event(0)));
        queue.recorded("not-the-front");
        assert_eq!(queue.counts(), (CAPACITY, 3));
        queue.recorded("0");
        assert_eq!(queue.front(), Some(event(1)));
        queue.submit(event(CAPACITY + 3));
        assert_eq!(queue.counts(), (CAPACITY, 3));
    }

    #[test]
    fn oversized_row_is_an_observable_gap_and_restart_is_not_durable() {
        let queue = CaptureQueue::default();
        let mut oversized = event(0);
        oversized.metadata_json = "x".repeat(EVENT_BYTES);
        queue.submit(oversized);
        assert_eq!(queue.counts(), (0, 1));
        queue.submit(event(1));
        assert_eq!(queue.front(), Some(event(1)));
        let restarted = CaptureQueue::default();
        assert_eq!(restarted.counts(), (0, 0));
        assert_eq!(restarted.front(), None);
    }
}
