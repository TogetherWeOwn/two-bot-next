//! Guild-scoped sqlx ticket store. Router callers must first use core authorize.
//! Row locks fence close/recovery races; transcript capture and cleanup state
//! commit together, so a restart never reopens a ticket whose transcript saved.
//!
//! sqlx transaction executor and Drop rollback:
//! https://docs.rs/sqlx/0.9.0/sqlx/struct.Transaction.html
//! Per-member reservation serialization uses transaction-level advisory locks:
//! https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS

use sqlx::{PgConnection, PgPool, Row};
use two_bot_core::funnel::{format_iso_millis, parse_iso_millis};
use two_bot_core::tickets::{
    decide_open, OpenDecision, Ticket, TicketError, TicketStatus, Transcript, TranscriptSnapshot,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Domain(#[from] TicketError),
    #[error("Ticket row contains an invalid {0}")]
    InvalidRow(&'static str),
    #[error("Ticket was not found in the configured guild")]
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenResult {
    Created(Ticket),
    Refused(OpenDecision),
}

/// A mandatory guild fence applies even to recovery, purge, and erasure.
#[derive(Debug, Clone)]
pub struct TicketStore {
    pool: PgPool,
    guild_id: String,
}

impl TicketStore {
    pub fn new(pool: PgPool, guild_id: String) -> Result<Self, StoreError> {
        if guild_id.is_empty() {
            return Err(TicketError::WrongGuild.into());
        }
        Ok(Self { pool, guild_id })
    }

    /// Supply a unique reservation id before any Discord channel creation.
    /// Both the cooldown and active check run under the same member lock.
    pub async fn reserve(
        &self,
        ticket_id: &str,
        opener_id: &str,
        now: i64,
        cooldown_seconds: u64,
    ) -> Result<OpenResult, StoreError> {
        if ticket_id.is_empty() || opener_id.is_empty() {
            return Err(TicketError::InvalidTransition.into());
        }
        let mut tx = self.pool.begin().await?;
        // JSON avoids ambiguous concatenation of guild/opener IDs. A hash
        // collision only over-serializes; the partial index remains authoritative.
        let key = serde_json::to_string(&[self.guild_id.as_str(), opener_id])
            .expect("string array serializes");
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(key)
            .execute(&mut *tx)
            .await?;
        // ORDER BY instant, not TEXT: legacy rows may carry UTC offsets
        // (e.g. +02:00) that sort incorrectly as strings.
        let active = sqlx::query(
            "SELECT id, guild_id, channel_id, opener_id, claimed_by, status, created_at, closing_started_at, closed_at FROM tickets WHERE guild_id = $1 AND opener_id = $2 AND status <> 'closed' ORDER BY created_at::timestamptz DESC LIMIT 1"
        ).bind(&self.guild_id).bind(opener_id).fetch_optional(&mut *tx).await?
            .map(decode_ticket).transpose()?;
        let last: Option<(String,)> = sqlx::query_as(
            "SELECT created_at FROM tickets WHERE guild_id = $1 AND opener_id = $2 ORDER BY created_at::timestamptz DESC LIMIT 1"
        ).bind(&self.guild_id).bind(opener_id).fetch_optional(&mut *tx).await?;
        let last = last
            .map(|(value,)| timestamp(&value, "created_at"))
            .transpose()?;
        let decision = decide_open(active.as_ref(), last, now, cooldown_seconds);
        if decision != OpenDecision::Reserve {
            tx.commit().await?;
            return Ok(OpenResult::Refused(decision));
        }
        let row = sqlx::query(
            "INSERT INTO tickets (id, guild_id, opener_id, status, created_at) VALUES ($1, $2, $3, 'creating', $4) RETURNING id, guild_id, channel_id, opener_id, claimed_by, status, created_at, closing_started_at, closed_at"
        ).bind(ticket_id).bind(&self.guild_id).bind(opener_id).bind(format_iso_millis(now))
            .fetch_one(&mut *tx).await?;
        let ticket = decode_ticket(row)?;
        tx.commit().await?;
        Ok(OpenResult::Created(ticket))
    }

    pub async fn get(&self, id: &str) -> Result<Option<Ticket>, StoreError> {
        sqlx::query(
            "SELECT id, guild_id, channel_id, opener_id, claimed_by, status, created_at, closing_started_at, closed_at FROM tickets WHERE guild_id = $1 AND id = $2"
        )
        .bind(&self.guild_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(decode_ticket)
        .transpose()
    }

    pub async fn by_channel(&self, channel_id: &str) -> Result<Option<Ticket>, StoreError> {
        sqlx::query(
            "SELECT id, guild_id, channel_id, opener_id, claimed_by, status, created_at, closing_started_at, closed_at FROM tickets WHERE guild_id = $1 AND channel_id = $2"
        )
        .bind(&self.guild_id)
        .bind(channel_id)
        .fetch_optional(&self.pool)
        .await?
        .map(decode_ticket)
        .transpose()
    }

    /// Ready loads open tickets as well as interrupted workflows; no runtime
    /// map is authoritative. The pure recovery_action determines the next call.
    pub async fn recoverable(&self) -> Result<Vec<Ticket>, StoreError> {
        // ORDER BY instant, not TEXT: see reserve().
        sqlx::query("SELECT id, guild_id, channel_id, opener_id, claimed_by, status, created_at, closing_started_at, closed_at FROM tickets WHERE guild_id = $1 AND status <> 'closed' ORDER BY created_at::timestamptz, id")
            .bind(&self.guild_id).fetch_all(&self.pool).await?
            .into_iter().map(decode_ticket).collect()
    }

    async fn locked(&self, conn: &mut PgConnection, id: &str) -> Result<Ticket, StoreError> {
        let row = sqlx::query(
            "SELECT id, guild_id, channel_id, opener_id, claimed_by, status, created_at, closing_started_at, closed_at FROM tickets WHERE guild_id = $1 AND id = $2 FOR UPDATE"
        )
        .bind(&self.guild_id)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or(StoreError::NotFound)?;
        decode_ticket(row)
    }

    async fn persist(conn: &mut PgConnection, ticket: &Ticket) -> Result<(), StoreError> {
        sqlx::query("UPDATE tickets SET channel_id = $1, claimed_by = $2, status = $3, closing_started_at = $4, closed_at = $5 WHERE guild_id = $6 AND id = $7")
            .bind(&ticket.channel_id).bind(&ticket.claimed_by).bind(ticket.status.as_str())
            .bind(ticket.closing_started_at.map(format_iso_millis)).bind(ticket.closed_at.map(format_iso_millis))
            .bind(&ticket.guild_id).bind(&ticket.id).execute(conn).await?;
        Ok(())
    }

    async fn transition(
        &self,
        id: &str,
        apply: impl FnOnce(&mut Ticket) -> Result<(), TicketError>,
    ) -> Result<Ticket, StoreError> {
        let mut tx = self.pool.begin().await?;
        let mut ticket = self.locked(&mut tx, id).await?;
        apply(&mut ticket)?;
        Self::persist(&mut tx, &ticket).await?;
        tx.commit().await?;
        Ok(ticket)
    }

    pub async fn record_channel(&self, id: &str, channel_id: &str) -> Result<Ticket, StoreError> {
        self.transition(id, |ticket| ticket.record_channel(channel_id))
            .await
    }

    pub async fn activate(&self, id: &str, channel_id: &str) -> Result<Ticket, StoreError> {
        self.transition(id, |ticket| ticket.activate(channel_id))
            .await
    }

    pub async fn claim(&self, id: &str, staff_id: &str) -> Result<Ticket, StoreError> {
        self.transition(id, |ticket| ticket.claim(staff_id)).await
    }

    pub async fn begin_close(&self, id: &str, now: i64) -> Result<Ticket, StoreError> {
        self.transition(id, |ticket| ticket.begin_close(now)).await
    }

    /// Atomic capture: there is no saved-transcript / reopened-ticket window.
    /// Metadata is derived from the locked ticket, not supplied by the caller.
    pub async fn save_transcript(
        &self,
        id: &str,
        expected_started_at: i64,
        captured_at: i64,
        snapshot: TranscriptSnapshot,
    ) -> Result<Transcript, StoreError> {
        let mut tx = self.pool.begin().await?;
        let mut ticket = self.locked(&mut tx, id).await?;
        let transcript = ticket.capture_close(expected_started_at, captured_at, snapshot)?;
        sqlx::query("INSERT INTO ticket_transcripts (ticket_id, guild_id, channel_id, opener_id, claimed_by, content, message_count, created_at, purge_after) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
            .bind(&transcript.ticket_id).bind(&transcript.guild_id).bind(&transcript.channel_id)
            .bind(&transcript.opener_id).bind(&transcript.claimed_by).bind(&transcript.content)
            .bind(transcript.message_count).bind(format_iso_millis(transcript.created_at))
            .bind(format_iso_millis(transcript.purge_after)).execute(&mut *tx).await?;
        Self::persist(&mut tx, &ticket).await?;
        tx.commit().await?;
        Ok(transcript)
    }

    pub async fn transcript_exists(&self, id: &str) -> Result<bool, StoreError> {
        let (exists,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM ticket_transcripts WHERE guild_id = $1 AND ticket_id = $2)")
            .bind(&self.guild_id).bind(id).fetch_one(&self.pool).await?;
        Ok(exists)
    }

    pub async fn reopen_interrupted(
        &self,
        id: &str,
        expected_started_at: i64,
    ) -> Result<Ticket, StoreError> {
        let mut tx = self.pool.begin().await?;
        let mut ticket = self.locked(&mut tx, id).await?;
        let (exists,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM ticket_transcripts WHERE guild_id = $1 AND ticket_id = $2)")
            .bind(&self.guild_id).bind(id).fetch_one(&mut *tx).await?;
        ticket.reopen_interrupted(expected_started_at, exists)?;
        Self::persist(&mut tx, &ticket).await?;
        tx.commit().await?;
        Ok(ticket)
    }

    /// Legacy transcripts may have been committed without cleanup state.
    /// A transcript whose body was purged but whose ticket still awaits
    /// cleanup recovers from the locked ticket's closed timestamp: the
    /// durable captured-close state survives `purge_expired`.
    pub async fn recover_saved_close(
        &self,
        id: &str,
        expected_started_at: i64,
    ) -> Result<Ticket, StoreError> {
        let mut tx = self.pool.begin().await?;
        let mut ticket = self.locked(&mut tx, id).await?;
        let captured = match sqlx::query_as::<_, (String,)>(
            "SELECT created_at FROM ticket_transcripts WHERE guild_id = $1 AND ticket_id = $2",
        )
        .bind(&self.guild_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        {
            Some((created_at,)) => timestamp(&created_at, "created_at")?,
            // Purged bodies keep their captured-close rows: fall back to the
            // persisted cleanup timestamp, which the atomic body-purge sets.
            None => ticket
                .closed_at
                .filter(|_| ticket.status == TicketStatus::CleanupPending)
                .ok_or(StoreError::NotFound)?,
        };
        match ticket.recover_saved_close(expected_started_at, captured) {
            Ok(()) => {
                Self::persist(&mut tx, &ticket).await?;
            }
            // `purge_expired` already reconciled this close to cleanup in the
            // same commit that deleted the body: confirm the durable state
            // instead of failing, so a purged body can never reopen the close.
            Err(TicketError::StaleClose) => ticket.confirm_reconciled_close(expected_started_at)?,
            Err(other) => return Err(other.into()),
        }
        tx.commit().await?;
        Ok(ticket)
    }

    /// Fenced completion for an unsaved close whose channel is confirmed
    /// absent. Call only after the REST executor proves the channel is gone
    /// (successful DELETE or Discord code 10003), mirroring legacy
    /// `recoverClosing`; the expected close token fences a racing capture.
    pub async fn abandon_unsaved_close(
        &self,
        id: &str,
        expected_started_at: i64,
        now: i64,
    ) -> Result<Ticket, StoreError> {
        let mut tx = self.pool.begin().await?;
        let mut ticket = self.locked(&mut tx, id).await?;
        let (exists,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM ticket_transcripts WHERE guild_id = $1 AND ticket_id = $2)")
            .bind(&self.guild_id).bind(id).fetch_one(&mut *tx).await?;
        if exists {
            return Err(TicketError::InvalidTransition.into());
        }
        ticket.abandon_unsaved_close(expected_started_at, now)?;
        Self::persist(&mut tx, &ticket).await?;
        tx.commit().await?;
        Ok(ticket)
    }

    pub async fn queue_open_rollback(&self, id: &str, now: i64) -> Result<Ticket, StoreError> {
        self.transition(id, |ticket| ticket.queue_open_rollback(now))
            .await
    }

    /// Retire an open row only after typed channel absence. Lock and recheck
    /// the channel/state so stale recovery cannot retire a racing close/capture.
    /// No transcript is invented for a channel deleted outside the bot.
    pub async fn retire_missing_open(
        &self,
        id: &str,
        expected_channel: &str,
        now: i64,
    ) -> Result<Ticket, StoreError> {
        let mut tx = self.pool.begin().await?;
        let mut ticket = self.locked(&mut tx, id).await?;
        let (saved,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM ticket_transcripts WHERE guild_id = $1 AND ticket_id = $2)")
            .bind(&self.guild_id).bind(id).fetch_one(&mut *tx).await?;
        if ticket.status != TicketStatus::Open
            || ticket.channel_id.as_deref() != Some(expected_channel)
            || saved
        {
            return Err(TicketError::InvalidTransition.into());
        }
        ticket.queue_open_rollback(now)?;
        ticket.finish_cleanup(now)?;
        Self::persist(&mut tx, &ticket).await?;
        tx.commit().await?;
        Ok(ticket)
    }

    /// Call only after confirmed deletion or code 10003, never any other error.
    pub async fn finish_cleanup(&self, id: &str, now: i64) -> Result<Ticket, StoreError> {
        self.transition(id, |ticket| ticket.finish_cleanup(now))
            .await
    }

    /// Recovery may abandon an unactivated reservation only after a successful
    /// guild channel search found no orphan, or its recorded channel was deleted.
    pub async fn abandon_creating(&self, id: &str) -> Result<bool, StoreError> {
        Ok(sqlx::query(
            "DELETE FROM tickets WHERE guild_id = $1 AND id = $2 AND status = 'creating'",
        )
        .bind(&self.guild_id)
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn purge_expired(&self, now: i64) -> Result<u64, StoreError> {
        // Native comparisons handle legacy timestamps with different UTC offsets.
        // Retention is a privacy ceiling, even when channel cleanup is pending.
        // Reconcile first, atomically with the body delete: a closing row whose
        // only capture evidence is an expiring transcript (legacy crash between
        // INSERT and state UPDATE) must reach cleanup_pending in the same
        // commit, or a later reopen_interrupted would resurrect a captured
        // close. The ticket row itself is then the capture marker.
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE tickets AS t SET status = 'cleanup_pending', closed_at = tt.created_at \
             FROM ticket_transcripts AS tt \
             WHERE tt.guild_id = $1 AND t.guild_id = tt.guild_id AND t.id = tt.ticket_id \
             AND t.status = 'closing' AND tt.purge_after::timestamptz <= $2::timestamptz",
        )
        .bind(&self.guild_id)
        .bind(format_iso_millis(now))
        .execute(&mut *tx)
        .await?;
        let deleted = sqlx::query("DELETE FROM ticket_transcripts WHERE guild_id = $1 AND purge_after::timestamptz <= $2::timestamptz")
            .bind(&self.guild_id).bind(format_iso_millis(now)).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(deleted)
    }

    /// GDPR/member-erasure hook; FK cascades transcript bodies in the same write.
    pub async fn erase_member(&self, member_id: &str) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "DELETE FROM tickets WHERE guild_id = $1 AND (opener_id = $2 OR claimed_by = $2)",
        )
        .bind(&self.guild_id)
        .bind(member_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

fn timestamp(value: &str, field: &'static str) -> Result<i64, StoreError> {
    parse_iso_millis(value).ok_or(StoreError::InvalidRow(field))
}

fn decode_ticket(row: sqlx::postgres::PgRow) -> Result<Ticket, StoreError> {
    let status: String = row.try_get("status")?;
    let created: String = row.try_get("created_at")?;
    let closing: Option<String> = row.try_get("closing_started_at")?;
    let closed: Option<String> = row.try_get("closed_at")?;
    Ok(Ticket {
        id: row.try_get("id")?,
        guild_id: row.try_get("guild_id")?,
        channel_id: row.try_get("channel_id")?,
        opener_id: row.try_get("opener_id")?,
        claimed_by: row.try_get("claimed_by")?,
        status: TicketStatus::parse(&status).ok_or(StoreError::InvalidRow("status"))?,
        created_at: timestamp(&created, "created_at")?,
        closing_started_at: closing
            .map(|value| timestamp(&value, "closing_started_at"))
            .transpose()?,
        closed_at: closed
            .map(|value| timestamp(&value, "closed_at"))
            .transpose()?,
    })
}
