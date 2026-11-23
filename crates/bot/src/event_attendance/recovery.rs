//! Dormant, actor-owned attendance recovery turns.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use sqlx::PgPool;
use time::OffsetDateTime;
use two_bot_core::Snowflake;
use two_bot_cutover::event_attendance::finalization::{
    close_after_member_loss, close_observed, member_loss_page, open_page, CLOSES_PER_TURN,
};

const MAX_KNOWN_LOSSES: usize = 32;
const TURN_BUDGET: Duration = Duration::from_millis(1_000);

#[derive(Clone, Copy, PartialEq, Eq)]
struct KnownLoss {
    member: Snowflake,
    generation: u64,
}

#[derive(Default)]
struct LossState {
    known: Vec<KnownLoss>,
    generation: u64,
    capture_stopped: bool,
}

/// One guild's loss state, safe to notify from ingress while the actor awaits
/// storage. The capture stop is sticky for this fence's lifetime.
#[derive(Clone, Default)]
pub struct MemberLossFence(Arc<Mutex<LossState>>);

impl MemberLossFence {
    fn state(&self) -> std::sync::MutexGuard<'_, LossState> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn record_loss(&self, member: Snowflake) {
        let mut state = self.state();
        state.generation += 1;
        let loss = KnownLoss {
            member,
            generation: state.generation,
        };
        if let Some(known) = state.known.iter_mut().find(|known| known.member == member) {
            *known = loss;
        } else if state.known.len() < MAX_KNOWN_LOSSES {
            state.known.push(loss);
        } else {
            state.capture_stopped = true;
        }
    }

    /// A recovery gate for the actor. Capture also needs its own live,
    /// lifecycle and privacy authority.
    pub fn capture_allowed(&self) -> bool {
        let state = self.state();
        !state.capture_stopped && state.known.is_empty()
    }

    fn first(&self) -> Option<KnownLoss> {
        self.state().known.first().copied()
    }

    fn finish(&self, loss: KnownLoss) {
        let mut state = self.state();
        // An older page cannot acknowledge a newer loss of the same member.
        if state.known.first() == Some(&loss) {
            state.known.remove(0);
        }
    }
}

struct Departure {
    member: Snowflake,
    channel: String,
    observed_at: OffsetDateTime,
    after_id: i64,
}

#[derive(Debug)]
pub enum TurnError {
    Storage(sqlx::Error),
    Deadline,
}

/// Keep the original observed close ahead of conservative loss cleanup. One
/// turn services either that departure or one member's loss.
pub struct AttendanceRecovery {
    guild: String,
    fence: MemberLossFence,
    departure: Option<Departure>,
    recovery_cursor: Option<(KnownLoss, i64)>,
}

impl AttendanceRecovery {
    pub fn new(guild: Snowflake, fence: MemberLossFence) -> Self {
        Self {
            guild: guild.to_string(),
            fence,
            departure: None,
            recovery_cursor: None,
        }
    }

    pub fn capture_allowed(&self) -> bool {
        self.departure.is_none() && self.fence.capture_allowed()
    }

    /// Retain a trustworthy departure before any await. If one is already
    /// pending, only the member of the skipped observation loses continuity.
    pub fn retain_departure(
        &mut self,
        member: Snowflake,
        channel: Snowflake,
        observed_at: OffsetDateTime,
    ) {
        if !self.capture_allowed() {
            self.fence.record_loss(member);
            return;
        }
        self.departure = Some(Departure {
            member,
            channel: channel.to_string(),
            observed_at,
            after_id: 0,
        });
    }

    /// The deadline covers pool acquisition, reads, closes and commits. A
    /// timed-out write is retried by scoped ID without rewriting a close.
    pub async fn turn(&mut self, pool: &PgPool) -> Result<(), TurnError> {
        tokio::time::timeout(TURN_BUDGET, self.advance(pool))
            .await
            .map_err(|_| TurnError::Deadline)?
            .map_err(TurnError::Storage)
    }

    async fn advance(&mut self, pool: &PgPool) -> Result<(), sqlx::Error> {
        if let Some(departure) = self.departure.as_mut() {
            let member = departure.member.to_string();
            let page = open_page(pool, &self.guild, &member, departure.after_id).await?;
            let exhausted = page.len() <= CLOSES_PER_TURN;
            for row in page.into_iter().take(CLOSES_PER_TURN) {
                if row.channel_id.as_ref() == Some(&departure.channel) && row.joined_at.is_some() {
                    close_observed(pool, &self.guild, &member, row.id, departure.observed_at)
                        .await?;
                } else {
                    close_after_member_loss(pool, &self.guild, &member, &[row.id]).await?;
                }
                departure.after_id = row.id;
            }
            if exhausted {
                self.departure = None;
            }
            return Ok(());
        }

        let Some(loss) = self.fence.first() else {
            return Ok(());
        };
        let member = loss.member.to_string();
        let after_id = self
            .recovery_cursor
            .filter(|(active, _)| *active == loss)
            .map_or(0, |(_, after_id)| after_id);
        let mut ids = member_loss_page(pool, &self.guild, &member, after_id).await?;
        let exhausted = ids.len() <= CLOSES_PER_TURN;
        ids.truncate(CLOSES_PER_TURN);
        close_after_member_loss(pool, &self.guild, &member, &ids).await?;
        if exhausted {
            self.fence.finish(loss);
            self.recovery_cursor = None;
        } else {
            self.recovery_cursor = Some((loss, ids.last().copied().unwrap_or(after_id)));
        }
        Ok(())
    }
}
