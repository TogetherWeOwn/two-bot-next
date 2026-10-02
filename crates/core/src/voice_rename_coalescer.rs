//! Pure rename-coalescer decisions, written from `docs/voice-rooms.md`
//! (Discord API notes) only.
//!
//! Channel renames are limited to about 2 per 10 minutes per channel. The
//! runtime therefore keeps one pending name per channel, coalesces updates
//! and skips renames when the name hasn't changed, while create/delete paths
//! never consult the backlog. This module is that backlog: it performs no
//! I/O, holds no Discord, store, clock or timer types, and never sends,
//! retries or schedules a rename. Rate limiting, retry-after handling,
//! per-guild ordering and persistence belong to the parent runtime.

use std::collections::BTreeMap;

use crate::Snowflake;

/// Longest channel name this module will hold, in Unicode scalar values.
/// Matches the Discord API note's 100-character name limit.
pub const MAX_CHANNEL_NAME_CHARS: usize = 100;

/// Pending names stored per channel. The bound is structural: the map holds
/// at most one `String` per channel ID, so no sequence of updates can deepen
/// a channel's backlog beyond one.
pub const MAX_PENDING_PER_CHANNEL: usize = 1;

/// Typed refusals for [`RenameCoalescer::queue_rename`]. Errors never echo
/// the rejected name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RenameError {
    #[error("channel id must be nonzero")]
    InvalidChannelId,
    #[error("channel name cannot be empty")]
    EmptyName,
    #[error("channel name is {chars} characters long; the maximum is {max}")]
    NameTooLong { chars: usize, max: usize },
}

/// What one [`RenameCoalescer::queue_rename`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueOutcome {
    /// No pending name existed; one is now stored.
    Queued,
    /// A different pending name existed; it was replaced and is returned.
    Coalesced { previous: String },
    /// The pending name already equals the desired name; the map is unchanged.
    Unchanged,
}

/// Whether a rename call is needed: true exactly when the two names differ.
///
/// Comparison is exact string inequality. Case, spacing and normalisation
/// forms are significant: `"Lounge"` and `"lounge"` are different names, and
/// so are canonically equivalent but byte-distinct spellings. The caller
/// supplies the authoritative current name (from guild state or the last
/// confirmed rename) and the freshly rendered desired name.
#[must_use]
pub fn should_rename(current_name: &str, desired_name: &str) -> bool {
    current_name != desired_name
}

fn validate_name(desired_name: &str) -> Result<(), RenameError> {
    if desired_name.is_empty() {
        return Err(RenameError::EmptyName);
    }
    let chars = desired_name.chars().count();
    if chars > MAX_CHANNEL_NAME_CHARS {
        return Err(RenameError::NameTooLong {
            chars,
            max: MAX_CHANNEL_NAME_CHARS,
        });
    }
    Ok(())
}

/// One pending rename per channel, keyed by channel ID in ascending order.
///
/// The runtime owns the timing: it calls [`should_rename`] to decide whether
/// a freshly rendered name needs sending, [`queue_rename`](Self::queue_rename)
/// to coalesce it into the backlog, [`pending`](Self::pending) or
/// [`take_pending`](Self::take_pending) when rename budget allows, and
/// [`observe_current`](Self::observe_current) or [`forget`](Self::forget) as
/// guild state confirms or deletes channels. Creating a room never reads this
/// map; deleting one only drops its entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenameCoalescer {
    pending: BTreeMap<Snowflake, String>,
}

impl RenameCoalescer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Number of channels with a pending rename. Because depth is bounded at
    /// [`MAX_PENDING_PER_CHANNEL`], this is also the total number of pending
    /// renames.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Alias for [`len`](Self::len) under the name the lifecycle slice uses:
    /// how many channel slots currently hold a pending rename.
    #[must_use]
    pub fn pending_slots(&self) -> usize {
        self.pending.len()
    }

    /// The pending name for `channel_id`, if one is stored.
    #[must_use]
    pub fn pending(&self, channel_id: Snowflake) -> Option<&str> {
        self.pending.get(&channel_id).map(String::as_str)
    }

    /// Whether `channel_id` currently holds a pending rename.
    #[must_use]
    pub fn contains(&self, channel_id: Snowflake) -> bool {
        self.pending.contains_key(&channel_id)
    }

    /// Store `desired_name` as the pending rename for `channel_id`,
    /// coalescing with any existing entry:
    /// - no entry becomes [`QueueOutcome::Queued`];
    /// - a different entry is overwritten and returned as
    ///   [`QueueOutcome::Coalesced`];
    /// - an equal entry leaves the map unchanged as
    ///   [`QueueOutcome::Unchanged`].
    ///
    /// The name is stored exactly as given (no trimming or folding); the
    /// caller renders the final template output before queuing. Zero channel
    /// IDs, empty names and names over [`MAX_CHANNEL_NAME_CHARS`] scalars are
    /// refused and leave the map unchanged. There is no cap on the number of
    /// channels with pending renames, so queuing for one channel never fails
    /// because other channels have backlogs.
    pub fn queue_rename(
        &mut self,
        channel_id: Snowflake,
        desired_name: &str,
    ) -> Result<QueueOutcome, RenameError> {
        if channel_id == 0 {
            return Err(RenameError::InvalidChannelId);
        }
        validate_name(desired_name)?;
        match self.pending.get(&channel_id) {
            None => {
                self.pending.insert(channel_id, desired_name.to_owned());
                Ok(QueueOutcome::Queued)
            }
            Some(current) if current == desired_name => Ok(QueueOutcome::Unchanged),
            Some(_) => {
                let previous = self
                    .pending
                    .insert(channel_id, desired_name.to_owned())
                    .expect("pending entry exists");
                Ok(QueueOutcome::Coalesced { previous })
            }
        }
    }

    /// Remove and return the pending name for `channel_id`. The runtime calls
    /// this when it spends rename budget on the channel; if the send fails
    /// (for example a 429 the runtime must honour), it re-queues the returned
    /// name with [`queue_rename`](Self::queue_rename).
    pub fn take_pending(&mut self, channel_id: Snowflake) -> Option<String> {
        self.pending.remove(&channel_id)
    }

    /// Reconcile one channel against authoritative guild state: when the
    /// stored pending name equals `current_name` the rename is already in
    /// effect (confirmed or made by hand) and the entry is dropped. Returns
    /// true when an entry was dropped. Never stores anything.
    pub fn observe_current(&mut self, channel_id: Snowflake, current_name: &str) -> bool {
        match self.pending.get(&channel_id) {
            Some(pending) if pending == current_name => {
                self.pending.remove(&channel_id);
                true
            }
            _ => false,
        }
    }

    /// Drop any pending rename for `channel_id`. The delete path calls this
    /// unconditionally; it returns true when an entry existed. Creating a room
    /// never calls into this map at all.
    pub fn forget(&mut self, channel_id: Snowflake) -> bool {
        self.pending.remove(&channel_id).is_some()
    }

    /// Drop every pending rename, for example after a full reconciliation
    /// against the channels that actually exist.
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}
