//! Dormant post-replay boundary for the Web client's `live.snapshot` read.

use std::num::NonZeroU64;

use crate::realtime_wire::{
    encode_snapshot,
    recovery::{Generation, Recovery},
    Id, Topics, WireTime,
};

use super::{
    rate_limited_body, require_field_str, ActionError, ErrorCode, NonceBurnedRequest, TokenBuckets,
};

/// Server-owned scope for a dedicated snapshot signing key. An absent scope
/// keeps the read disabled, even for an otherwise valid internal-action key.
pub struct LiveSnapshotReadAuthority<'a> {
    pub key_id: &'a str,
    pub guild_id: Id,
}

impl NonceBurnedRequest<'_> {
    /// Grant one read only after checking authority and the exact signed request.
    pub fn authorize_live_snapshot(
        self,
        authority: Option<&LiveSnapshotReadAuthority<'_>>,
        buckets: &mut TokenBuckets,
    ) -> Result<AuthorizedLiveSnapshotRead, ActionError> {
        let authority = authority
            .filter(|scope| scope.key_id == self.0.key_id)
            .ok_or_else(not_allowed)?;
        let body = rate_limited_body(&self.0, buckets)?;
        if require_field_str(&body, "action")? != "live.snapshot" {
            return Err(not_allowed());
        }
        let raw_guild = require_field_str(&body, "guild_id")?;
        let guild = raw_guild
            .parse::<NonZeroU64>()
            .ok()
            .filter(|id| body.len() == 2 && raw_guild == id.to_string())
            .map(Id)
            .ok_or_else(|| {
                ActionError::new(
                    ErrorCode::Malformed,
                    "Malformed live snapshot request",
                    "bad_live_snapshot",
                )
            })?;
        if guild != authority.guild_id {
            return Err(not_allowed());
        }
        Ok(AuthorizedLiveSnapshotRead(guild))
    }
}

/// One authenticated, replay-guarded, guild-scoped read. The ordered source
/// can consume this grant without retaining the signed request or its headers.
pub struct AuthorizedLiveSnapshotRead(Id);

impl AuthorizedLiveSnapshotRead {
    /// The source supplies an ordered view and its generation. Keep the reply
    /// under the same read guard when handing it off. `now` samples the clock
    /// with the view held so a lookup delay cannot extend the observation lease.
    pub fn read<'state>(
        self,
        load: impl FnOnce(Id) -> Option<(&'state Recovery, &'state Generation)>,
        now: impl FnOnce() -> WireTime,
    ) -> Option<&'state [u8]> {
        let (recovery, generation) = load(self.0)?;
        recovery.read_snapshot(self.0, generation, now())
    }

    /// Encode an advancing cut. The owner must hold one ordered view across
    /// the cursor, clock sample, and materialization of its complete topics.
    pub fn read_current(
        self,
        recovery: &Recovery,
        generation: &Generation,
        (seq, observed_at): (u64, WireTime),
        now: WireTime,
        topics: impl FnOnce() -> Topics,
    ) -> Option<Vec<u8>> {
        if seq < recovery.baseline()?.seq
            || !recovery.allows_snapshot(self.0, generation, observed_at, now)
        {
            return None;
        }
        encode_snapshot(self.0, seq, observed_at, &topics())
    }
}

fn not_allowed() -> ActionError {
    ActionError::new(
        ErrorCode::ActionNotAllowed,
        "Live snapshot read is not allowed",
        "live_snapshot_not_allowed",
    )
}

#[cfg(test)]
mod tests;
