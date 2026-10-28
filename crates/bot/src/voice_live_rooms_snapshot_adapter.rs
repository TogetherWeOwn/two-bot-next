//! Editable test-only bridge to the production live-rooms snapshot reader.
//! Replace MissingFeature with conversions and a production call only. Do not
//! put snapshot locking, readiness, room selection, bot classification, role
//! lookup or presence policy here. The protected vocabulary is not a required
//! production API. Keep MissingFeature distinct from legitimate refusals.

#![cfg(test)]

use super::{LiveGuild, SnapshotFacts, Snowflake, VoiceRoom};

#[derive(Debug)]
pub(super) enum CaptureError {
    MissingFeature,
}

pub(super) fn capture(
    _live: &LiveGuild,
    _ledger: &[VoiceRoom],
    _viewer: Snowflake,
    _as_of_unix: i64,
    _presence_enabled: bool,
) -> Result<SnapshotFacts, CaptureError> {
    Err(CaptureError::MissingFeature)
}
