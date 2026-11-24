//! Publish a complete replacement baseline only after durably reserving a gap.

use std::sync::Arc;

use super::{encode_delta, encode_snapshot, Change, Id, Operation, Topics, WireTime, MAX_SEQ};

/// Identifies one rebuild in one ordered guild reducer. Clones share identity.
#[derive(Clone)]
pub struct Generation(Arc<()>);

/// The normalized baseline and its direct action reply at a single watermark.
/// The reducer must replace this view when applying subsequent gateway deltas.
pub struct Baseline {
    pub seq: u64,
    pub topics: Topics,
    pub snapshot: Vec<u8>,
    observed_at: WireTime,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublishError<E> {
    StaleGeneration,
    AlreadyPublished,
    SequenceExhausted,
    InvalidBaseline,
    Persist(E),
}

/// Owned by the ordered reducer for one guild, not by individual handlers.
pub struct Recovery {
    guild_id: Id,
    generation: Option<Generation>,
    baseline: Option<Baseline>,
}

impl Recovery {
    pub fn new(guild_id: Id) -> Self {
        Self {
            guild_id,
            generation: None,
            baseline: None,
        }
    }

    /// Withdraw the old baseline before rebuilding. Old generation tokens no
    /// longer match; the delivery owner must also retire their queued attempts.
    pub fn begin(&mut self) -> Generation {
        self.retire();
        let generation = Generation(Arc::new(()));
        self.generation = Some(generation.clone());
        generation
    }

    /// Withdraw a published baseline and invalidate its generation immediately.
    pub fn retire(&mut self) {
        self.generation = None;
        self.baseline = None;
    }

    pub fn is_current(&self, generation: &Generation) -> bool {
        self.generation
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.0, &generation.0))
    }

    pub fn baseline(&self) -> Option<&Baseline> {
        self.baseline.as_ref()
    }

    pub(crate) fn read_snapshot(
        &self,
        guild_id: Id,
        generation: &Generation,
        now: WireTime,
    ) -> Option<&[u8]> {
        let baseline = self.baseline.as_ref()?;
        self.allows_snapshot(guild_id, generation, baseline.observed_at, now)
            .then_some(baseline.snapshot.as_slice())
    }

    pub(crate) fn allows_snapshot(
        &self,
        guild_id: Id,
        generation: &Generation,
        observed_at: WireTime,
        now: WireTime,
    ) -> bool {
        // Match the Web client's observation window and millisecond precision.
        let age = now.unix_millis() - observed_at.unix_millis();
        self.guild_id == guild_id && self.is_current(generation) && (-2_000..30_000).contains(&age)
    }

    /// The caller must establish completeness in this gateway generation before
    /// passing topics. The codec checks bounds and references, not completeness.
    /// `persist` must durably compare the expected watermark (zero for a new
    /// stream) and replace it with the new one before returning success. After
    /// an uncertain write the caller must re-read durable state before retrying.
    ///
    /// Returns a guild-only delta at the snapshot watermark, to trigger hub
    /// recovery even without subsequent gateway traffic. A failed or cancelled
    /// reservation leaves the snapshot unavailable and returns no marker.
    pub async fn publish<E>(
        &mut self,
        generation: &Generation,
        expected_seq: u64,
        observed_at: WireTime,
        topics: Topics,
        persist: impl AsyncFnOnce(Id, u64, u64) -> Result<(), E>,
    ) -> Result<Vec<u8>, PublishError<E>> {
        if !self.is_current(generation) {
            return Err(PublishError::StaleGeneration);
        }
        if self.baseline.is_some() {
            return Err(PublishError::AlreadyPublished);
        }
        // Leave one unused sequence between the last reserved value and the
        // replacement baseline so the hub must recover instead of applying a
        // contiguous delta to the pre-reconnect state.
        let seq = expected_seq
            .checked_add(2)
            .filter(|&seq| seq <= MAX_SEQ)
            .ok_or(PublishError::SequenceExhausted)?;
        let snapshot = encode_snapshot(self.guild_id, seq, observed_at, &topics)
            .ok_or(PublishError::InvalidBaseline)?;
        let marker = encode_delta(
            self.guild_id,
            seq,
            observed_at,
            &[Operation::Guild(Change::Upsert {
                id: self.guild_id,
                value: topics.guild.clone(),
            })],
        )
        .ok_or(PublishError::InvalidBaseline)?;

        persist(self.guild_id, expected_seq, seq)
            .await
            .map_err(PublishError::Persist)?;
        self.baseline = Some(Baseline {
            seq,
            topics,
            snapshot,
            observed_at,
        });
        Ok(marker)
    }
}

#[cfg(test)]
mod tests;
