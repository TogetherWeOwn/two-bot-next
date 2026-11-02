//! Event voice channel claims, presence transitions and reconcile passes.
//!
//! The SQL primitives live in `two_bot_cutover::event_voice`; this module
//! owns the worker hook and the Discord-verified orchestration plus the
//! audit trail: every create path logs event id, instance start, channel id
//! and actor tick. The reconcile procedures never delete: a 404'd channel
//! marks its row `orphaned` (lease cleared for one retry), and occupied
//! rooms are kept untouched.

use crate::voice_rooms::{RoomPersistence, REVALIDATE_TIMEOUT_MS};
use sqlx::PgPool;
use std::future::Future;
use std::time::Duration;
use time::OffsetDateTime;
use tracing::{debug, info};
use two_bot_core::Snowflake;
use two_bot_cutover::event_voice::{
    bind_event_voice_channel, bound_event_voice_channels, claim_event_voice_channel,
    clear_event_voice_lease, mark_event_voice_orphaned, revive_event_voice_orphan,
    stale_event_voice_claims,
};

/// Record one voice-channel transition (join, leave or move) as presence
/// spans. Called by the guild actor loop before room handling; each write
/// is bounded by `REVALIDATE_TIMEOUT_MS` (the same inbox-protection
/// bound as creator revalidation) so a slow statement cannot hold room
/// handling. Failures and timeouts are debug-logged (no member or channel
/// ids on this warm path) and the 60s guild repair heals them: it re-opens
/// a lost join and closes a lost leave against the live occupant snapshot
/// (see [`repair_guild_presence`]). Never fails the actor loop.
pub async fn record_transition(
    store: &impl RoomPersistence,
    guild: Snowflake,
    member: Snowflake,
    previous: Option<Snowflake>,
    current: Option<Snowflake>,
) {
    if previous == current {
        return;
    }
    if let Some(channel) = current {
        match tokio::time::timeout(
            Duration::from_millis(REVALIDATE_TIMEOUT_MS),
            store.record_event_voice_join(channel, member),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                debug!(
                    guild = guild.to_string(),
                    error = %error,
                    "event voice join span write failed; the 60s guild repair re-opens it"
                );
            }
            Err(_) => {
                debug!(
                    guild = guild.to_string(),
                    "event voice join span write timed out; the 60s guild repair re-opens it"
                );
            }
        }
    }
    if let Some(channel) = previous {
        if Some(channel) != current {
            match tokio::time::timeout(
                Duration::from_millis(REVALIDATE_TIMEOUT_MS),
                store.record_event_voice_leave(channel, member),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    debug!(
                        guild = guild.to_string(),
                        error = %error,
                        "event voice leave stamp write failed; the 60s guild repair closes it"
                    );
                }
                Err(_) => {
                    debug!(
                        guild = guild.to_string(),
                        "event voice leave stamp write timed out; the 60s guild repair closes it"
                    );
                }
            }
        }
    }
}

/// Minimum gap between guild presence repairs on one actor. The repair is a
/// backstop behind the packet-paced [`record_transition`] writes: a failed
/// join or leave heals on the next repair tick, at most this far out.
pub const EVENT_VOICE_REPAIR_INTERVAL_MS: u64 = 60_000;

/// Total wall-clock budget for the bootstrap occupant flush. Each write
/// carries the transition's own timeout bound, and this caps their sum, so
/// a saturated pool delays actor start by about one bound, not one per
/// occupant. Occupants skipped past the budget are picked up by the next
/// [`repair_guild_presence`] tick.
pub const EVENT_VOICE_BOOTSTRAP_BUDGET_MS: u64 = REVALIDATE_TIMEOUT_MS;

/// What one bootstrap occupant flush did, for the actor start log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapFlushReport {
    /// Occupants whose join transition was attempted.
    pub attempted: u64,
    /// Occupants skipped past the total budget (repaired next tick).
    pub skipped: u64,
}

/// Reconcile one guild's event-voice spans against the authoritative live
/// occupant snapshot: re-open a lost join per bound channel, and close the
/// spans of members no longer present. Runs on the actor maintenance tick
/// (at most every [`EVENT_VOICE_REPAIR_INTERVAL_MS`]); idempotent against
/// the packet-paced writes, so a repair racing a live transition converges
/// on the same spans. Every write carries the transition's own timeout
/// bound, failures are debug-logged per channel (counts only, no member or
/// channel ids on this warm path), and a failed channel never fails the
/// actor loop. A member who leaves between the snapshot read and the flush
/// gains a span the next repair closes: at most one interval of inflation,
/// self-healing.
pub async fn repair_guild_presence(
    store: &impl RoomPersistence,
    guild: Snowflake,
    snapshot: &[(Snowflake, Snowflake)],
) {
    let bindings = match tokio::time::timeout(
        Duration::from_millis(REVALIDATE_TIMEOUT_MS),
        store.event_voice_guild_bindings(guild),
    )
    .await
    {
        Ok(Ok(bindings)) => bindings,
        Ok(Err(error)) => {
            debug!(
                guild = guild.to_string(),
                error = %error,
                "event voice repair bindings read failed; the next repair retries it"
            );
            return;
        }
        Err(_) => {
            debug!(
                guild = guild.to_string(),
                "event voice repair bindings read timed out; the next repair retries it"
            );
            return;
        }
    };
    if bindings.is_empty() {
        return;
    }
    for binding in bindings {
        let present: Vec<Snowflake> = snapshot
            .iter()
            .filter(|(_, channel)| *channel == binding.channel)
            .map(|(member, _)| *member)
            .collect();
        match tokio::time::timeout(
            Duration::from_millis(REVALIDATE_TIMEOUT_MS),
            store.flush_event_voice_presence(&binding.event_id, binding.instance, &present),
        )
        .await
        {
            Ok(Ok(reopened)) => {
                if reopened > 0 {
                    debug!(
                        guild = guild.to_string(),
                        reopened, "event voice repair re-opened lost join spans"
                    );
                }
            }
            Ok(Err(error)) => {
                debug!(
                    guild = guild.to_string(),
                    error = %error,
                    "event voice repair flush failed; the next repair retries it"
                );
            }
            Err(_) => {
                debug!(
                    guild = guild.to_string(),
                    "event voice repair flush timed out; the next repair retries it"
                );
            }
        }
        match tokio::time::timeout(
            Duration::from_millis(REVALIDATE_TIMEOUT_MS),
            store.close_event_voice_spans_absent(&binding.event_id, binding.instance, &present),
        )
        .await
        {
            Ok(Ok(closed)) => {
                if closed > 0 {
                    debug!(
                        guild = guild.to_string(),
                        closed, "event voice repair closed spans of absent members"
                    );
                }
            }
            Ok(Err(error)) => {
                debug!(
                    guild = guild.to_string(),
                    error = %error,
                    "event voice repair close-absent failed; the next repair retries it"
                );
            }
            Err(_) => {
                debug!(
                    guild = guild.to_string(),
                    "event voice repair close-absent timed out; the next repair retries it"
                );
            }
        }
    }
}

/// Flush the actor-start occupant snapshot join-only, under one total
/// budget: snapshot occupants never sent a transition, so a member already
/// sitting in a bound event channel gains an open span here. Movers covered
/// by buffered transitions keep a single span (the join is idempotent), and
/// channels with no event mapping write nothing. Stops past
/// [`EVENT_VOICE_BOOTSTRAP_BUDGET_MS`] and counts the rest skipped; the next
/// [`repair_guild_presence`] tick picks them up.
pub async fn flush_bootstrap_presence(
    store: &impl RoomPersistence,
    guild: Snowflake,
    occupants: &[(Snowflake, Snowflake)],
) -> BootstrapFlushReport {
    let mut report = BootstrapFlushReport::default();
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(EVENT_VOICE_BOOTSTRAP_BUDGET_MS);
    for (member_id, channel) in occupants {
        if tokio::time::Instant::now() >= deadline {
            report.skipped += 1;
            continue;
        }
        report.attempted += 1;
        record_transition(store, guild, *member_id, None, Some(*channel)).await;
    }
    if report.skipped > 0 {
        debug!(
            guild = guild.to_string(),
            attempted = report.attempted,
            skipped = report.skipped,
            "event voice bootstrap flush hit its total budget; the 60s guild repair picks up the rest"
        );
    }
    report
}

/// Claim one event-voice create under `lease`, before any Discord call. The
/// winner (a fresh insert or a re-lease of a cleared or orphaned row)
/// proceeds to create; the loser returns false and must not call Discord.
/// Both outcomes are audit-logged with event id and instance.
pub async fn claim_channel(
    pool: &PgPool,
    event_id: &str,
    instance_starts_at: OffsetDateTime,
    starts_at: OffsetDateTime,
    ends_at: Option<OffsetDateTime>,
    lease: &str,
) -> Result<bool, sqlx::Error> {
    let won = claim_event_voice_channel(
        pool,
        event_id,
        instance_starts_at,
        starts_at,
        ends_at,
        lease,
    )
    .await?;
    let instance = instance_starts_at.to_string();
    if won {
        info!(
            voice_event = "event_voice_claim_won",
            event_id, instance, lease, "event voice channel claimed; this tick owns the create"
        );
    } else {
        debug!(
            voice_event = "event_voice_claim_lost",
            event_id, instance, lease, "event voice channel already claimed; skipping the create"
        );
    }
    Ok(won)
}

/// What the Discord GET behind one stale `creating` row reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventProbeOutcome {
    /// The event already names a voice channel: bind it, no create needed.
    FoundChannel(Snowflake),
    /// No channel (or no event): clear the lease so the next tick retries.
    NoChannel,
}

/// What the Discord channel GET behind one bound row reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelProbeOutcome {
    /// The channel exists; occupied rooms are always kept, never deleted.
    Exists { occupied: bool },
    /// The channel 404s: mark the row orphaned with exactly one retry GET.
    Missing,
}

/// A Discord probe that failed without an answer (timeout, 5xx, rate limit
/// exhaustion). The row is skipped untouched; the next tick retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError(pub String);

/// Discord verification behind the reconcile passes. Production probes back
/// these with scheduled-event and channel GETs plus the live occupancy
/// snapshot; tests script them.
pub trait EventVoiceProbe: Send + Sync {
    /// Resolve the voice channel the event currently names, if any.
    fn get_event_channel(
        &self,
        event_id: &str,
    ) -> impl Future<Output = Result<EventProbeOutcome, ProbeError>> + Send;
    /// Check one bound channel id.
    fn get_channel(
        &self,
        channel_id: Snowflake,
    ) -> impl Future<Output = Result<ChannelProbeOutcome, ProbeError>> + Send;
    /// The single bounded retry GET after a 404, before the orphan mark
    /// stands. A recovered channel revives the row.
    fn retry_get_channel(
        &self,
        channel_id: Snowflake,
    ) -> impl Future<Output = Result<ChannelProbeOutcome, ProbeError>> + Send;
}

/// What one stale-claim pass did, for the tick log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StaleReconcileReport {
    /// Stale rows selected.
    pub probed: u64,
    /// Rows bound to a Discord-confirmed channel.
    pub bound: u64,
    /// Rows whose lease was cleared for retry.
    pub cleared: u64,
    /// Rows skipped (probe transport failure, no lease to clear, lost race,
    /// bind write failure, clear write failure).
    pub skipped: u64,
}

/// Verify every stale `creating` row via Discord GET: bind the found
/// channel, else clear the lease for retry. Fresh rows are never probed.
pub async fn reconcile_stale_claims(
    pool: &PgPool,
    probe: &impl EventVoiceProbe,
    stale_secs: i64,
) -> Result<StaleReconcileReport, sqlx::Error> {
    let mut report = StaleReconcileReport::default();
    for (event_id, instance_starts_at, lease) in stale_event_voice_claims(pool, stale_secs).await? {
        report.probed += 1;
        let instance = instance_starts_at.to_string();
        match probe.get_event_channel(&event_id).await {
            Ok(EventProbeOutcome::FoundChannel(channel)) => {
                let channel_id = channel.to_string();
                // A probe answer can collide with another row's binding
                // (UNIQUE (voice_channel_id)): skip the row, never the pass.
                let bound = match bind_event_voice_channel(
                    pool,
                    &event_id,
                    instance_starts_at,
                    &channel_id,
                )
                .await
                {
                    Ok(bound) => bound,
                    Err(error) => {
                        report.skipped += 1;
                        debug!(
                            voice_event = "event_voice_bind_failed",
                            event_id,
                            instance,
                            channel_id,
                            error = %error,
                            "stale event voice bind failed; continuing with the next row"
                        );
                        continue;
                    }
                };
                if bound {
                    report.bound += 1;
                    info!(
                        voice_event = "event_voice_channel_bound",
                        event_id,
                        instance,
                        channel_id,
                        "stale event voice claim bound to its Discord channel"
                    );
                } else {
                    report.skipped += 1;
                }
            }
            Ok(EventProbeOutcome::NoChannel) => {
                let Some(lease) = lease else {
                    report.skipped += 1;
                    continue;
                };
                // Like the bind arm above: a failed clear skips the row,
                // never the pass.
                let cleared = match clear_event_voice_lease(
                    pool,
                    &event_id,
                    instance_starts_at,
                    &lease,
                )
                .await
                {
                    Ok(cleared) => cleared,
                    Err(error) => {
                        report.skipped += 1;
                        debug!(
                            voice_event = "event_voice_clear_failed",
                            event_id,
                            instance,
                            lease,
                            error = %error,
                            "stale event voice lease clear failed; continuing with the next row"
                        );
                        continue;
                    }
                };
                if cleared {
                    report.cleared += 1;
                    info!(
                        voice_event = "event_voice_lease_cleared",
                        event_id, instance, lease, "stale event voice claim released for retry"
                    );
                } else {
                    report.skipped += 1;
                }
            }
            Err(error) => {
                report.skipped += 1;
                debug!(
                    voice_event = "event_voice_probe_skipped",
                    event_id,
                    error = ?error,
                    "stale event voice probe failed without an answer; next tick retries"
                );
            }
        }
    }
    Ok(report)
}

/// What one bound-channel pass did, for the tick log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OrphanReconcileReport {
    /// Bound non-orphaned rows probed.
    pub probed: u64,
    /// Live channels kept untouched (occupied or not: never deleted).
    pub kept: u64,
    /// 404 rows marked orphaned.
    pub orphaned: u64,
    /// Orphaned rows revived by the retry GET (transient 404).
    pub revived: u64,
    /// Rows skipped (probe transport failure, corrupt binding, orphan write
    /// failure, revive write failure).
    pub skipped: u64,
}

/// Verify every bound channel id via Discord GET: a 404 marks the row
/// `orphaned` (lease cleared for one retry) with exactly one retry GET; a
/// live channel is kept untouched. This procedure never deletes.
pub async fn reconcile_bound_channels(
    pool: &PgPool,
    probe: &impl EventVoiceProbe,
) -> Result<OrphanReconcileReport, sqlx::Error> {
    let mut report = OrphanReconcileReport::default();
    for (event_id, instance_starts_at, channel_id) in bound_event_voice_channels(pool).await? {
        report.probed += 1;
        let instance = instance_starts_at.to_string();
        let channel: Snowflake = match channel_id.parse() {
            Ok(channel) => channel,
            Err(_) => {
                report.skipped += 1;
                debug!(
                    voice_event = "event_voice_probe_skipped",
                    event_id,
                    instance,
                    "event voice binding is not a channel id; leaving it for an operator"
                );
                continue;
            }
        };
        match probe.get_channel(channel).await {
            Ok(ChannelProbeOutcome::Exists { .. }) => {
                report.kept += 1;
            }
            Ok(ChannelProbeOutcome::Missing) => {
                if let Err(error) =
                    mark_event_voice_orphaned(pool, &event_id, instance_starts_at).await
                {
                    report.skipped += 1;
                    debug!(
                        voice_event = "event_voice_orphan_failed",
                        event_id,
                        instance,
                        channel_id,
                        error = %error,
                        "event voice orphan mark failed; continuing with the next row"
                    );
                    continue;
                }
                report.orphaned += 1;
                info!(
                    voice_event = "event_voice_channel_orphaned",
                    event_id,
                    instance,
                    channel_id,
                    "event voice channel 404s; row orphaned for one retry"
                );
                match probe.retry_get_channel(channel).await {
                    Ok(ChannelProbeOutcome::Exists { .. }) => {
                        // Like the orphan-mark arm above: a failed revive
                        // skips the row, never the pass. The row stays
                        // orphaned (and counted so); the stale pass
                        // re-probes it once its lease age lapses.
                        if let Err(error) =
                            revive_event_voice_orphan(pool, &event_id, instance_starts_at).await
                        {
                            report.skipped += 1;
                            debug!(
                                voice_event = "event_voice_revive_failed",
                                event_id,
                                instance,
                                channel_id,
                                error = %error,
                                "event voice orphan revive failed; continuing with the next row"
                            );
                            continue;
                        }
                        report.revived += 1;
                        info!(
                            voice_event = "event_voice_channel_revived",
                            event_id,
                            instance,
                            channel_id,
                            "event voice channel recovered on retry; row revived"
                        );
                    }
                    Ok(ChannelProbeOutcome::Missing) => {}
                    Err(error) => {
                        report.skipped += 1;
                        debug!(
                            voice_event = "event_voice_probe_skipped",
                            event_id,
                            error = ?error,
                            "event voice orphan retry failed without an answer"
                        );
                    }
                }
            }
            Err(error) => {
                report.skipped += 1;
                debug!(
                    voice_event = "event_voice_probe_skipped",
                    event_id,
                    error = ?error,
                    "event voice channel probe failed without an answer; next tick retries"
                );
            }
        }
    }
    Ok(report)
}
