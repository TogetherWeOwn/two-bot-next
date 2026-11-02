//! sqlx [`FunnelStore`]: append-only events + members projection.
//!
//! Write semantics ported verbatim from the cutover seam
//! (`two-bot-cutover::db::{record_event, touch_activity}`), which itself
//! ports legacy `src/store/eventStore.ts`:
//!
//! * **Insert-first arbitration**: `ON CONFLICT (idempotency_key) DO NOTHING`
//!   decides the winner atomically; the members projection runs only for the
//!   winner, in the same transaction.
//! * **Joins** overwrite arrival columns but clear `left_at` (rejoin
//!   re-screens).
//! * **Milestone columns** are earliest/first-wins (`... AND col IS NULL`);
//!   recency (`last_active_at`) only ever moves forward.
//!
//! The one deliberate difference from cutover: the core [`FunnelEvent`]
//! carries typed `Snowflake` (u64) IDs and [`EventType`], formatted to the
//! legacy TEXT shapes at the query boundary. Metadata serializes with the
//! workspace `preserve_order` `serde_json` so funnel JSON blobs keep legacy
//! `JSON.stringify` byte order (startKnown, startedAt, durationSeconds) —
//! Postgres TEXT comparison is byte-wise. Row bytes match legacy.

use std::collections::HashSet;

use sqlx::{postgres::PgRow, Pool, Postgres, Row};
use two_bot_core::{
    analytics_family::AnalyticsCategory,
    membership::{
        advance_observation, is_membership, set_observation, valid_observation, Membership,
        MembershipProjection, MembershipStore,
    },
    metrics, EventType, FunnelEvent, FunnelStore, RecordOutcome, Snowflake, StoredRow,
    MESSAGE_RUNGS,
};

/// Membership projected across TOP-25 keyset pages. `truncated` means the
/// first page did not contain the whole history; the membership itself is
/// complete, including evidence in later pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipPage {
    pub membership: Membership,
    pub truncated: bool,
}

/// Postgres-backed [`FunnelStore`].
///
/// Holds the pool plus a `tokio::runtime::Handle` for the sync→async bridge
/// (see crate docs): each trait method dispatches its query with
/// `block_in_place`, so the dispatch worker waits while runtime threads
/// drive the I/O. Shard polling stays independent. Construct with [`PgFunnelStore::new`] from inside a tokio
/// runtime (the bot binary, tests); the handle is `Handle::current()`.
#[derive(Debug, Clone)]
pub struct PgFunnelStore {
    pool: Pool<Postgres>,
    handle: tokio::runtime::Handle,
}

impl PgFunnelStore {
    /// Build over `pool`. Must be called from inside a tokio runtime.
    #[must_use]
    pub fn new(pool: Pool<Postgres>) -> Self {
        Self {
            pool,
            handle: tokio::runtime::Handle::current(),
        }
    }

    /// Borrow the pool for direct queries.
    #[must_use]
    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }

    /// Full membership with a fixed-vocabulary receipt for a multi-page read.
    /// SQL holds at most TOP-25 rows plus one lookahead; the chronological
    /// fold keeps constant state and never silently drops older evidence.
    pub fn membership_paged(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
    ) -> Option<MembershipPage> {
        match crate::retry::block_on_retry(&self.handle, || {
            fetch_membership_paged(&self.pool, guild_id, member_id)
        }) {
            Ok(page) => page,
            Err(error) => {
                metrics::global().db_error("other");
                tracing::warn!(error = %error, "funnel membership degraded to None after retries");
                None
            }
        }
    }

    /// Fallible append + projection for async callers. The sync core seam
    /// fails closed on an error; async tools can handle the error explicitly.
    pub async fn try_record(&self, event: &FunnelEvent) -> Result<RecordOutcome, sqlx::Error> {
        record_async(&self.pool, event, None).await
    }

    /// Fallible append-or-reconfirm for async callers. Carries the
    /// observation hint through the same duplicate-convergence path as the
    /// sync [`MembershipStore`] seam.
    ///
    /// [`MembershipStore`]: two_bot_core::membership::MembershipStore
    pub async fn try_record_observed(
        &self,
        event: &FunnelEvent,
        observed_at: Option<&str>,
    ) -> Result<RecordOutcome, sqlx::Error> {
        record_async(&self.pool, event, observed_at).await
    }
}

/// Format a guild/member snowflake the way legacy stores it: decimal text.
fn snowflake_text(id: Snowflake) -> String {
    id.to_string()
}

/// Parse a persisted TEXT JSON blob. Corrupt rows surface as a decode error
/// rather than a guessed projection.
fn parse_metadata(text: Option<&str>) -> Result<Option<serde_json::Value>, sqlx::Error> {
    text.map(serde_json::from_str)
        .transpose()
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

async fn record_async(
    pool: &Pool<Postgres>,
    event: &FunnelEvent,
    observed_at: Option<&str>,
) -> Result<RecordOutcome, sqlx::Error> {
    // Build the insert row through the shared observation helpers so durable
    // inserts stamp exactly like memory: the hint lands verbatim in the blob,
    // invalid hints and non-membership events keep the event payload as-is.
    // `preserve_order` keeps insertion order so metadata bytes match legacy
    // `JSON.stringify` (no key sorting).
    let mut new_row = StoredRow::from(event);
    if let Some(hint) = observed_at {
        set_observation(&mut new_row, hint);
    }
    let key = new_row.idempotency_key.clone();
    let member_id = new_row.member_id.map(snowflake_text);
    let guild_id = snowflake_text(new_row.guild_id);
    let mut tx = pool.begin().await?;
    // Serialize with concurrent opt-outs (which take SHARE ROW EXCLUSIVE
    // over these tables): whoever locks first wins, so a write that passes
    // the gate can never land after the purge. Lock order matches the
    // opt-out's sorted order, and ROW EXCLUSIVE never blocks other writers.
    sqlx::query("LOCK TABLE events, members, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    // Privacy gate, inside the write transaction: a suppressed pair or a
    // guild-disabled analytics family drops the write (and any duplicate
    // reconfirmation below) with no event row and no members projection.
    if two_bot_core::privacy::ingestion_blocked_in(
        &mut tx,
        &guild_id,
        member_id.as_deref(),
        event.event_type.as_str(),
    )
    .await?
    {
        tx.rollback().await?;
        return Ok(RecordOutcome { inserted: false });
    }
    // A suppressed inviter's attribution never re-enters through a join they
    // did not perform: strip `inviterId` from `member_join` metadata when the
    // inviter is suppressed, keeping the joiner's own row intact.
    let mut metadata = new_row.metadata.as_ref().map(|v| v.to_string());
    if event.event_type.as_str() == "member_join" {
        if let Some(inviter) = two_bot_core::privacy::inviter_in_metadata(metadata.as_deref()) {
            if two_bot_core::privacy::suppressed_in(&mut tx, &guild_id, &inviter).await? {
                metadata = two_bot_core::privacy::scrubbed_member_join_metadata(
                    metadata.as_deref(),
                    &inviter,
                );
            }
        }
    }
    let inserted: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
         VALUES ($1, $2, $3, $4::timestamptz, $5, $6, $7)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(event.event_type.as_str())
    .bind(member_id.as_deref())
    .bind(&guild_id)
    .bind(&new_row.occurred_at)
    .bind(&new_row.source)
    .bind(metadata.as_deref())
    .bind(&key)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((event_row_id,)) = inserted else {
        let outcome = advance_duplicate(&mut tx, event, &key, observed_at).await?;
        tx.commit().await?;
        // Duplicates write no event row, so they journal nothing.
        return Ok(outcome);
    };
    project_event(&mut tx, event, &guild_id).await?;
    // Rollback-journal writer for the `PgFunnelStore` seam (the production
    // gateway batch writer in `cutover::gateway_session` journals its own
    // appends the same way): each `PgFunnelStore` funnel append leaves one
    // `events` insert entry naming the inserted row, journaled inside the
    // primary transaction under a savepoint so the row and its evidence
    // commit atomically — no crash window between a primary commit and a
    // second journal transaction can leave a committed row unjournaled.
    // Evidence-only and best-effort: only journal statement errors are
    // isolated by rolling back the savepoint; they never fail or alter the
    // primary write.
    let entry = crate::journal::JournalEntry {
        table_name: "events".to_owned(),
        row_identity: event_row_id.to_string(),
        op: crate::journal::JournalOp::Insert,
        pre_image: None,
    };
    sqlx::query("SAVEPOINT funnel_journal")
        .execute(&mut *tx)
        .await?;
    match crate::journal::record_in(&mut tx, &entry).await {
        Ok(_) => {
            sqlx::query("RELEASE SAVEPOINT funnel_journal")
                .execute(&mut *tx)
                .await?;
        }
        Err(journal_error) => {
            sqlx::query("ROLLBACK TO SAVEPOINT funnel_journal")
                .execute(&mut *tx)
                .await?;
            sqlx::query("RELEASE SAVEPOINT funnel_journal")
                .execute(&mut *tx)
                .await?;
            tracing::warn!(
                table = "events",
                row_identity = %entry.row_identity,
                error = %journal_error,
                "rollback_journal record failed; primary funnel append kept",
            );
        }
    }
    tx.commit().await?;
    Ok(RecordOutcome { inserted: true })
}

/// Reconfirm an existing row: advance its stored observation maximum under
/// the event-row lock, never touching occurrence/source/identity. The winner
/// of `ON CONFLICT DO NOTHING` never reaches here; concurrent reconfirmations
/// serialize on `FOR UPDATE` and converge on the newest hint, so a delayed
/// stale duplicate cannot overwrite a newer maximum. The `IS NOT DISTINCT
/// FROM` predicate is the compare-and-swap revision check: the update lands
/// only against the value this transaction locked and read, and a lost update
/// (zero rows) fails loudly as `RowNotFound` instead of silently dropping the
/// maximum. Duplicates without a usable hint skip the lock entirely.
async fn advance_duplicate(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    event: &FunnelEvent,
    key: &str,
    observed_at: Option<&str>,
) -> Result<RecordOutcome, sqlx::Error> {
    let Some(hint) = observed_at else {
        return Ok(RecordOutcome { inserted: false });
    };
    if !is_membership(event.event_type) || valid_observation(hint).is_none() {
        return Ok(RecordOutcome { inserted: false });
    }
    let current: Option<(Option<String>,)> =
        sqlx::query_as("SELECT metadata FROM events WHERE idempotency_key = $1 FOR UPDATE")
            .bind(key)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((current_meta,)) = current else {
        // Unreachable: rows are append-only, so the lost insert race that
        // reached this path still left a row behind. Report the duplicate.
        return Ok(RecordOutcome { inserted: false });
    };
    // Only metadata participates in the maximum; the scratch row reuses the
    // shared helper so durable convergence matches memory exactly.
    let mut stored = StoredRow {
        metadata: parse_metadata(current_meta.as_deref())?,
        ..StoredRow::from(event)
    };
    advance_observation(&mut stored, hint);
    let next = stored.metadata.as_ref().map(|v| v.to_string());
    if next != current_meta {
        let updated = sqlx::query(
            "UPDATE events SET metadata = $1 WHERE idempotency_key = $2 AND metadata IS NOT DISTINCT FROM $3",
        )
        .bind(next.as_deref())
        .bind(key)
        .bind(current_meta.as_deref())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            // Another writer replaced the locked value out from under this
            // compare-and-swap: surface the lost update loudly instead of
            // reporting a quiet duplicate with a dropped maximum.
            return Err(sqlx::Error::RowNotFound);
        }
    }
    Ok(RecordOutcome { inserted: false })
}

async fn fetch_membership_paged(
    pool: &Pool<Postgres>,
    guild_id: Snowflake,
    member_id: Snowflake,
) -> Result<Option<MembershipPage>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // One snapshot across pages: a reconfirmation cannot move observation
    // evidence while the fold is crossing the page boundary.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let mut after = None::<i64>;
    let mut projection = MembershipProjection::default();
    let mut truncated = false;
    loop {
        let rows = sqlx::query(
            "SELECT id, event_type, member_id, guild_id,
                    to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS occurred_at,
                    source, metadata, idempotency_key
             FROM events WHERE guild_id = $1 AND member_id = $2
               AND ($3::bigint IS NULL OR id > $3)
             ORDER BY id LIMIT 26",
        )
        .bind(snowflake_text(guild_id))
        .bind(snowflake_text(member_id))
        .bind(after)
        .fetch_all(&mut *tx)
        .await?;
        let more = rows.len() > 25;
        truncated |= more;
        for row in rows.iter().take(25) {
            projection.observe(&stored_row(row)?);
            after = Some(row.try_get("id")?);
        }
        if !more {
            break;
        }
    }
    tx.commit().await?;
    Ok(projection.finish().map(|membership| MembershipPage {
        membership,
        truncated,
    }))
}

/// Read back one member's event rows with microsecond UTC ISO text. The
/// `US` pattern (not `MS`) preserves the six fractional digits the contract
/// asserts through round trips; `AT TIME ZONE 'UTC'` keeps the text
/// independent of the connection's session time zone.
async fn fetch_member_rows(
    pool: &Pool<Postgres>,
    guild_id: Snowflake,
    member_id: Snowflake,
) -> Result<Vec<StoredRow>, sqlx::Error> {
    // `ORDER BY id` mirrors memory's insertion-ordered readback; projection
    // itself never uses arrival order.
    sqlx::query(
        "SELECT event_type, member_id, guild_id,
                to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS occurred_at,
                source, metadata, idempotency_key
         FROM events WHERE guild_id = $1 AND member_id = $2 ORDER BY id",
    )
    .bind(snowflake_text(guild_id))
    .bind(snowflake_text(member_id))
    .fetch_all(pool)
    .await?
    .iter()
    .map(stored_row)
    .collect()
}

/// Decode one `fetch_member_rows` row. Unknown event types and non-numeric
/// snowflakes are decode errors: the read model fails closed.
fn stored_row(row: &PgRow) -> Result<StoredRow, sqlx::Error> {
    let decode = |what: &str| sqlx::Error::Decode(format!("funnel row {what}").into());
    let event_type: String = row.try_get("event_type")?;
    let member_id: Option<String> = row.try_get("member_id")?;
    let guild_id: String = row.try_get("guild_id")?;
    let metadata: Option<String> = row.try_get("metadata")?;
    Ok(StoredRow {
        guild_id: guild_id.parse().map_err(|_| decode("guild_id"))?,
        member_id: member_id
            .map(|id| id.parse().map_err(|_| decode("member_id")))
            .transpose()?,
        event_type: EventType::from_wire(&event_type).ok_or_else(|| decode("event_type"))?,
        occurred_at: row.try_get("occurred_at")?,
        source: row.try_get("source")?,
        metadata: parse_metadata(metadata.as_deref())?,
        dedupe_token: None,
        idempotency_key: row.try_get("idempotency_key")?,
    })
}

/// Members projection guards (legacy `EventStore.project`): joins overwrite
/// arrival columns but clear `left_at`; milestone columns are earliest-wins;
/// recency only ever moves forward.
async fn project_event(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    event: &FunnelEvent,
    guild_id: &str,
) -> Result<(), sqlx::Error> {
    let Some(member_id) = event.member_id.map(snowflake_text) else {
        return Ok(());
    };
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2) ON CONFLICT (guild_id, member_id) DO NOTHING")
        .bind(guild_id)
        .bind(&member_id)
        .execute(&mut **tx)
        .await?;

    match event.event_type {
        EventType::MemberJoin => {
            sqlx::query("UPDATE members SET joined_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3")
                .bind(&event.occurred_at)
                .bind(guild_id)
                .bind(&member_id)
                .execute(&mut **tx)
                .await?;
            sqlx::query(
                "UPDATE members SET join_source = $1 WHERE guild_id = $2 AND member_id = $3",
            )
            .bind(&event.source)
            .bind(guild_id)
            .bind(&member_id)
            .execute(&mut **tx)
            .await?;
            sqlx::query("UPDATE members SET left_at = NULL WHERE guild_id = $1 AND member_id = $2")
                .bind(guild_id)
                .bind(&member_id)
                .execute(&mut **tx)
                .await?;
            sqlx::query("UPDATE members SET inactive_flagged_at = NULL WHERE guild_id = $1 AND member_id = $2")
                .bind(guild_id)
                .bind(&member_id)
                .execute(&mut **tx)
                .await?;
        }
        EventType::GateCleared => {
            sqlx::query("UPDATE members SET gate_cleared_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND gate_cleared_at IS NULL")
                .bind(&event.occurred_at)
                .bind(guild_id)
                .bind(&member_id)
                .execute(&mut **tx)
                .await?;
        }
        EventType::FirstMessage => {
            sqlx::query("UPDATE members SET first_message_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND first_message_at IS NULL")
                .bind(&event.occurred_at)
                .bind(guild_id)
                .bind(&member_id)
                .execute(&mut **tx)
                .await?;
            advance_activity(tx, guild_id, &member_id, &event.occurred_at).await?;
        }
        EventType::SecondMessage | EventType::ThirdMessage | EventType::FirstVoiceSession => {
            if event.event_type == EventType::ThirdMessage {
                sqlx::query("UPDATE members SET third_message_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND third_message_at IS NULL")
                    .bind(&event.occurred_at)
                    .bind(guild_id)
                    .bind(&member_id)
                    .execute(&mut **tx)
                    .await?;
            }
            if event.event_type == EventType::FirstVoiceSession {
                sqlx::query("UPDATE members SET first_voice_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND first_voice_at IS NULL")
                    .bind(&event.occurred_at)
                    .bind(guild_id)
                    .bind(&member_id)
                    .execute(&mut **tx)
                    .await?;
            }
            advance_activity(tx, guild_id, &member_id, &event.occurred_at).await?;
        }
        EventType::MemberInactive => {
            sqlx::query("UPDATE members SET inactive_flagged_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3")
                .bind(&event.occurred_at)
                .bind(guild_id)
                .bind(&member_id)
                .execute(&mut **tx)
                .await?;
        }
        EventType::MemberLeave => {
            sqlx::query("UPDATE members SET left_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3")
                .bind(&event.occurred_at)
                .bind(guild_id)
                .bind(&member_id)
                .execute(&mut **tx)
                .await?;
        }
        // Log-only rows: no projection column, recency untouched (they carry
        // their own channel attribution via `source`).
        EventType::InviteClick
        | EventType::OnboardingPrompted
        | EventType::GameRolesSelected
        | EventType::ChannelRouted
        | EventType::VoiceSessionStart
        | EventType::VoiceSessionEnd => {}
    }
    Ok(())
}

/// Recency never moves backwards (legacy `advance`).
async fn advance_activity(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    guild_id: &str,
    member_id: &str,
    at: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE members SET last_active_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3
          AND (last_active_at IS NULL OR last_active_at < $4::timestamptz)",
    )
    .bind(at)
    .bind(guild_id)
    .bind(member_id)
    .bind(at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl FunnelStore for PgFunnelStore {
    fn mark_bot(&self, guild_id: Snowflake, member_id: Snowflake) {
        crate::retry::block_on_retry(&self.handle, || async {
            // Suppressed pairs stay unwritten: the flag would create a
            // per-user `members` row for a member who opted out. The gate
            // runs inside the write transaction under the shared table lock
            // so a concurrent opt-out purge cannot slip between gate and
            // write.
            let mut tx = self.pool.begin().await?;
            sqlx::query("LOCK TABLE members, privacy_opt_outs IN ROW EXCLUSIVE MODE")
                .execute(&mut *tx)
                .await?;
            if two_bot_core::privacy::suppressed_in(
                &mut tx,
                &snowflake_text(guild_id),
                &snowflake_text(member_id),
            )
            .await?
            {
                tx.rollback().await?;
                return Ok::<(), sqlx::Error>(());
            }
            sqlx::query(
                "INSERT INTO members (guild_id, member_id, is_bot) VALUES ($1, $2, TRUE)
                 ON CONFLICT (guild_id, member_id) DO UPDATE SET is_bot = TRUE",
            )
            .bind(snowflake_text(guild_id))
            .bind(snowflake_text(member_id))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<(), sqlx::Error>(())
        })
        .expect("funnel mark_bot failed");
    }

    /// Insert-or-ignore by idempotency key; projection runs for the winner.
    ///
    /// # Panics
    ///
    /// Propagates a database failure as a panic after bounded retries are
    /// exhausted: the sync [`FunnelStore`] seam has no fallible channel, and
    /// a write failure must stop the pipeline rather than silently drop data.
    /// The release profile unwinds (`panic = "unwind"` in the workspace
    /// `Cargo.toml`), so the panic kills only the task that made the call —
    /// never the supervisor — but it poisons any mutex that task holds
    /// across the call; holders must recover from poisoning instead of
    /// `expect`ing the lock.
    fn record(&self, event: FunnelEvent) -> RecordOutcome {
        crate::retry::block_on_retry(&self.handle, || record_async(&self.pool, &event, None))
            .expect("funnel record failed")
    }

    /// Monotonic recency bump (never moves backwards). Failures panic — see
    /// [`FunnelStore::record`]. Like `record`, the bump honors the guild
    /// analytics toggle for its originating family: a disabled family's
    /// touch advances nothing.
    fn touch_activity(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        at: &str,
        family: AnalyticsCategory,
    ) {
        let guild_id = snowflake_text(guild_id);
        let member_id = snowflake_text(member_id);
        let at = at.to_owned();
        crate::retry::block_on_retry(&self.handle, || async {
            // Suppressed pairs stay untouched: the bump would create or
            // refresh a per-user `members` row for a member who opted out.
            // Like `mark_bot`, the gate runs under the shared table lock so
            // a concurrent opt-out cannot slip between gate and write.
            let mut tx = self.pool.begin().await?;
            sqlx::query("LOCK TABLE members, privacy_opt_outs IN ROW EXCLUSIVE MODE")
                .execute(&mut *tx)
                .await?;
            if two_bot_core::privacy::suppressed_in(&mut tx, &guild_id, &member_id).await? {
                tx.rollback().await?;
                return Ok::<(), sqlx::Error>(());
            }
            if two_bot_core::privacy::category_off_in(&mut tx, &guild_id, family).await? {
                tx.rollback().await?;
                return Ok::<(), sqlx::Error>(());
            }
            sqlx::query(
                "INSERT INTO members (guild_id, member_id, last_active_at) VALUES ($1, $2, $3::timestamptz)
                 ON CONFLICT (guild_id, member_id) DO UPDATE SET last_active_at = excluded.last_active_at
                   WHERE members.last_active_at IS NULL OR members.last_active_at < excluded.last_active_at",
            )
            .bind(&guild_id)
            .bind(&member_id)
            .bind(&at)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<(), sqlx::Error>(())
        })
        .expect("funnel touch_activity failed");
    }

    /// Lowest empty message rung for a message arriving at `at`, or `None`
    /// when the ladder is full — including the redelivery guard (a rung is
    /// filled only strictly after the rung below). Mirrors [`MemStore`]
    /// exactly on the healthy path; on a persistent DB error degrades to
    /// `None` with an error metric after bounded retries, never panicking.
    ///
    /// [`MemStore`]: two_bot_core::MemStore
    fn next_message_rung(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        at: &str,
    ) -> Option<EventType> {
        let guild_id = snowflake_text(guild_id);
        let member_id = snowflake_text(member_id);
        let at = at.to_owned();
        let rows: Vec<(String, String)> = match crate::retry::block_on_retry(
            &self.handle,
            || async {
                sqlx::query_as(
                    "SELECT event_type, to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
                     FROM events WHERE guild_id = $1 AND member_id = $2
                       AND event_type IN ('first_message', 'second_message', 'third_message')",
                )
                .bind(&guild_id)
                .bind(&member_id)
                .fetch_all(&self.pool)
                .await
            },
        ) {
            Ok(rows) => rows,
            Err(error) => {
                metrics::global().db_error("other");
                tracing::warn!(
                    error = %error,
                    "funnel next_message_rung degraded to None after retries"
                );
                // Fail closed as ladder-full: `None` fills no rung this
                // window, so an outage under-counts instead of inventing
                // milestones. This is the opposite of the empty-ladder
                // answer (`Some(FirstMessage)`): every message during the
                // outage fills nothing, and the next healthy message
                // re-reads the true ladder — no stuck state.
                return None;
            }
        };
        // ISO-8601 UTC compares lexicographically (legacy string compare).
        let mut filled: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for (event_type, stamp) in &rows {
            filled.entry(event_type.as_str()).or_insert(stamp.as_str());
        }
        let mut below: Option<&str> = None;
        for rung in MESSAGE_RUNGS {
            match filled.get(rung.as_str()) {
                None => {
                    if below.is_some_and(|b| at.as_str() <= b) {
                        return None;
                    }
                    return Some(rung);
                }
                Some(stamp) => below = Some(stamp),
            }
        }
        None
    }

    /// Whether the member already has a row of this type. On a persistent DB
    /// error degrades to `false` with an error metric after bounded retries,
    /// never panicking.
    fn has_event(&self, guild_id: Snowflake, member_id: Snowflake, event_type: EventType) -> bool {
        let guild_id = snowflake_text(guild_id);
        let member_id = snowflake_text(member_id);
        let event_type = event_type.as_str();
        match crate::retry::block_on_retry(&self.handle, || async {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = $3)",
            )
            .bind(&guild_id)
            .bind(&member_id)
            .bind(event_type)
            .fetch_one(&self.pool)
            .await
        }) {
            Ok(outcome) => outcome,
            Err(error) => {
                metrics::global().db_error("other");
                tracing::warn!(
                    error = %error,
                    "funnel has_event degraded to false after retries"
                );
                // Fail closed as unseen: `false` re-runs the follow-up write,
                // which is idempotent (insert-first arbitration dedupes),
                // rather than skipping a genuinely new event. Concurrent
                // callers already race this way on the healthy path, so
                // downstream tolerates the duplicate claim; the store itself
                // stays exact.
                false
            }
        }
    }
}

/// Chronological membership read model over the durable event log. The
/// projection derives from persisted event rows (not arrival-ordered
/// `members` columns), so historical joins and any-order replays converge
/// exactly like memory. Duplicate reconfirmations advance only the stored
/// observation maximum. Reads degrade fail-closed with an error metric after
/// bounded retries, never panicking; writes panic — see
/// [`FunnelStore::record`].
impl MembershipStore for PgFunnelStore {
    fn membership(&self, guild_id: Snowflake, member_id: Snowflake) -> Option<Membership> {
        self.membership_paged(guild_id, member_id)
            .map(|page| page.membership)
    }

    fn membership_rows(&self, guild_id: Snowflake, member_id: Snowflake) -> Vec<StoredRow> {
        match crate::retry::block_on_retry(&self.handle, || {
            fetch_member_rows(&self.pool, guild_id, member_id)
        }) {
            Ok(rows) => rows,
            Err(error) => {
                metrics::global().db_error("other");
                tracing::warn!(
                    error = %error,
                    "funnel membership_rows degraded to empty after retries"
                );
                // Fail closed as no rows: projecting zero rows yields `None`,
                // the same unknown-member answer as a never-seen member —
                // never a fabricated row.
                Vec::new()
            }
        }
    }

    /// Insert or reconfirm a membership event. Invalid/non-membership hints
    /// are ignored; an existing row's occurrence/source/other metadata stay
    /// put. Failures panic — see [`FunnelStore::record`].
    fn record_observed(&self, event: FunnelEvent, observed_at: Option<&str>) -> RecordOutcome {
        crate::retry::block_on_retry(&self.handle, || {
            record_async(&self.pool, &event, observed_at)
        })
        .expect("funnel record_observed failed")
    }
}

/// Read back the set of rung event types a member holds (test helper).
pub async fn member_rungs(
    pool: &Pool<Postgres>,
    guild_id: Snowflake,
    member_id: Snowflake,
) -> Result<HashSet<String>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT event_type FROM events WHERE guild_id = $1 AND member_id = $2
          AND event_type IN ('first_message', 'second_message', 'third_message')",
    )
    .bind(snowflake_text(guild_id))
    .bind(snowflake_text(member_id))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(t,)| t).collect())
}

#[cfg(test)]
mod privacy_lock_tests {
    //! Pin the writer-side table locks against concurrent opt-outs.
    //!
    //! The simulated opt-out below takes the same SHARE ROW EXCLUSIVE lock
    //! the real `opt_out_member` takes, then purges and suppresses. Without
    //! the writer's ROW EXCLUSIVE lock the gate check and the insert do not
    //! serialize with it, and rows survive the race: removing any lock below
    //! fails these tests.

    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Pool, Postgres};

    use two_bot_core::{analytics_family::AnalyticsCategory, EventType, FunnelEvent, FunnelStore};

    use super::PgFunnelStore;

    const GUILD: &str = "1545644954272137297";
    const MEMBER_A: &str = "123456789012345678";
    const MEMBER_B: &str = "123456789012345679";

    async fn test_pool(prefix: &str) -> Option<(Pool<Postgres>, String)> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                assert_ne!(
                    std::env::var("GITHUB_ACTIONS").as_deref(),
                    Ok("true"),
                    "CI must configure the store privacy database suite"
                );
                eprintln!("SKIP store privacy lock test: TWO_TEST_DATABASE_URL not set");
                return None;
            }
        };
        let options: PgConnectOptions = url.parse().expect("test database URL");
        let host = options.get_host();
        assert!(
            host == "agent-testdb"
                || (host == "localhost"
                    && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
            "test-container-only URL"
        );
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let schema = format!("{prefix}_{nonce}_{}", std::process::id());
        assert!(
            !schema.is_empty()
                && schema.len() <= 63
                && schema
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "safe unique test schema"
        );
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("test database admin");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create owned test schema");
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await
            .expect("test database pool");
        sqlx::migrate!("../cutover/migrations")
            .run(&pool)
            .await
            .expect("cutover migrations");
        Some((pool, schema))
    }

    async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }

    /// The opt-out's lock, purge and suppression in one transaction, without
    /// the cutover crate (which this crate must not depend on).
    async fn simulate_opt_out(pool: &Pool<Postgres>, guild: &str, member: &str) {
        let mut tx = pool.begin().await.expect("opt-out tx");
        sqlx::query(
            "LOCK TABLE community_facts, events, invite_snapshots, members, \
             privacy_opt_outs, privacy_opt_out_audit IN SHARE ROW EXCLUSIVE MODE",
        )
        .execute(&mut *tx)
        .await
        .expect("opt-out lock");
        for (table, predicate) in [
            ("events", "member_id = $2"),
            ("members", "member_id = $2"),
            ("community_facts", "actor_id = $2"),
        ] {
            let sql = format!("DELETE FROM {table} WHERE guild_id = $1 AND {predicate}");
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(guild)
                .bind(member)
                .execute(&mut *tx)
                .await
                .expect("opt-out purge");
        }
        sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
            .bind(guild)
            .bind(member)
            .execute(&mut *tx)
            .await
            .expect("suppression install");
        sqlx::query(
            "INSERT INTO privacy_opt_out_audit (guild_id, member_id, actor) VALUES ($1, $2, $3)",
        )
        .bind(guild)
        .bind(member)
        .bind("operator:store-privacy-lock-test")
        .execute(&mut *tx)
        .await
        .expect("opt-out receipt");
        tx.commit().await.expect("opt-out commit");
    }

    fn join_event(guild: u64, member: u64, at: &str) -> FunnelEvent {
        FunnelEvent {
            guild_id: guild,
            member_id: Some(member),
            event_type: EventType::MemberJoin,
            occurred_at: at.to_owned(),
            source: "store-privacy-lock-test".to_owned(),
            metadata: None,
            dedupe_token: None,
        }
    }

    /// `record` racing an opt-out never leaves rows behind: removing the
    /// `record_async` table lock lets the insert land after the purge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn record_races_opt_out_never_persists() {
        let Some((pool, schema)) = test_pool("store_privacy_record").await else {
            return;
        };
        let guild: u64 = GUILD.parse().unwrap();
        let member: u64 = MEMBER_A.parse().unwrap();

        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let store = PgFunnelStore::new(writer_pool);
            for i in 0..30 {
                let at = format!("2026-10-13T00:{i:02}:00.000Z");
                // `try_record` is async (no sync bridge); serialization
                // errors on the rarest timing are retried once.
                let event = join_event(guild, member, &at);
                if store.try_record(&event).await.is_err() {
                    let _ = store.try_record(&event).await;
                }
            }
        });
        let racer_pool = pool.clone();
        let opt_out = tokio::spawn(async move {
            simulate_opt_out(&racer_pool, GUILD, MEMBER_A).await;
        });
        let (writer_out, _) = tokio::join!(writer, opt_out);
        writer_out.expect("writer task");

        let events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 0, "no in-flight record survives the opt-out");
        let members: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(members, 0, "no projection row survives either");

        drop_schema(&pool, &schema).await;
    }

    /// `touch_activity` / `mark_bot` racing an opt-out create no `members`
    /// row for the pair while other members still bump: removing either
    /// table lock lets the write land after the purge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn touch_and_mark_race_opt_out_create_no_row() {
        let Some((pool, schema)) = test_pool("store_privacy_touch").await else {
            return;
        };
        let guild: u64 = GUILD.parse().unwrap();
        let member: u64 = MEMBER_A.parse().unwrap();
        let other: u64 = MEMBER_B.parse().unwrap();

        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let store = PgFunnelStore::new(writer_pool);
            for i in 0..20 {
                let at = format!("2026-10-13T00:{i:02}:00.000Z");
                store.touch_activity(guild, member, &at, AnalyticsCategory::Activity);
                store.mark_bot(guild, member);
            }
        });
        let racer_pool = pool.clone();
        let opt_out = tokio::spawn(async move {
            simulate_opt_out(&racer_pool, GUILD, MEMBER_A).await;
        });
        let (writer_out, _) = tokio::join!(writer, opt_out);
        writer_out.expect("writer task");

        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "no bump or flag row survives the opt-out");

        // Other members still bump after the race.
        let store = PgFunnelStore::new(pool.clone());
        store.touch_activity(
            guild,
            other,
            "2026-10-13T01:00:00.000Z",
            AnalyticsCategory::Activity,
        );
        let other_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_B)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(other_rows, 1, "other members still bump");

        drop_schema(&pool, &schema).await;
    }

    /// Deterministic pin for the `touch_activity` table lock: hold the
    /// opt-out's lock set open with the suppression installed but
    /// uncommitted, run one bump (its gate reads before the commit, then it
    /// blocks on the table lock), commit, and assert no row. Without the
    /// lock the gate passes and the upsert lands after the purge — a single
    /// iteration fails with no timing dependence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn touch_activity_deterministic_race_create_no_row() {
        let Some((pool, schema)) = test_pool("store_privacy_touch_deterministic").await else {
            return;
        };
        let guild: u64 = GUILD.parse().unwrap();
        let member: u64 = MEMBER_A.parse().unwrap();

        let mut opt_tx = pool.begin().await.expect("opt-out tx");
        sqlx::query(
            "LOCK TABLE community_facts, events, invite_snapshots, members, \
             privacy_opt_outs, privacy_opt_out_audit IN SHARE ROW EXCLUSIVE MODE",
        )
        .execute(&mut *opt_tx)
        .await
        .expect("opt-out lock");
        sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&mut *opt_tx)
            .await
            .expect("suppression install");
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let store = PgFunnelStore::new(writer_pool);
            store.touch_activity(
                guild,
                member,
                "2026-10-13T00:00:00.000Z",
                AnalyticsCategory::Activity,
            );
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        writer.await.expect("writer task");

        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "single bump racing the opt-out creates no row");

        drop_schema(&pool, &schema).await;
    }

    /// Deterministic pin for the `mark_bot` table lock: same shape as the
    /// bump pin, racing only the flag so the `mark_bot` lock is the one that
    /// absorbs the wait. A combined bump-plus-flag task would let the bump
    /// absorb the wait and leave this lock unpinned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mark_bot_deterministic_race_create_no_row() {
        let Some((pool, schema)) = test_pool("store_privacy_mark_deterministic").await else {
            return;
        };
        let guild: u64 = GUILD.parse().unwrap();
        let member: u64 = MEMBER_A.parse().unwrap();

        let mut opt_tx = pool.begin().await.expect("opt-out tx");
        sqlx::query(
            "LOCK TABLE community_facts, events, invite_snapshots, members, \
             privacy_opt_outs, privacy_opt_out_audit IN SHARE ROW EXCLUSIVE MODE",
        )
        .execute(&mut *opt_tx)
        .await
        .expect("opt-out lock");
        sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&mut *opt_tx)
            .await
            .expect("suppression install");
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let store = PgFunnelStore::new(writer_pool);
            store.mark_bot(guild, member);
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        writer.await.expect("writer task");

        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "single flag racing the opt-out creates no row");

        drop_schema(&pool, &schema).await;
    }

    /// Deterministic pin for the `record_async` table lock: one event while
    /// the opt-out holds its locks open. Without the lock the gate passes and
    /// the insert lands after the purge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn record_deterministic_race_never_persists() {
        let Some((pool, schema)) = test_pool("store_privacy_record_deterministic").await else {
            return;
        };
        let guild: u64 = GUILD.parse().unwrap();
        let member: u64 = MEMBER_A.parse().unwrap();

        let mut opt_tx = pool.begin().await.expect("opt-out tx");
        sqlx::query(
            "LOCK TABLE community_facts, events, invite_snapshots, members, \
             privacy_opt_outs, privacy_opt_out_audit IN SHARE ROW EXCLUSIVE MODE",
        )
        .execute(&mut *opt_tx)
        .await
        .expect("opt-out lock");
        sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&mut *opt_tx)
            .await
            .expect("suppression install");
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let store = PgFunnelStore::new(writer_pool);
            let event = join_event(guild, member, "2026-10-13T00:00:00.000Z");
            let _ = store.try_record(&event).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        writer.await.expect("writer task");

        let events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 0, "single record racing the opt-out never persists");

        drop_schema(&pool, &schema).await;
    }
}
