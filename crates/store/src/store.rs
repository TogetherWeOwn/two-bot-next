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

use sqlx::{Pool, Postgres};
use two_bot_core::{
    idempotency_key,
    membership::{
        advance_observation, is_membership, project, set_observation, valid_observation,
        Membership, MembershipStore,
    },
    EventType, FunnelEvent, FunnelStore, RecordOutcome, Snowflake, StoredRow, MESSAGE_RUNGS,
};

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
        record_observed_async(&self.pool, event, observed_at).await
    }

    /// Run an async query from the sync trait methods.
    fn block_on<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::task::block_in_place(|| self.handle.block_on(fut))
    }
}

/// Format a guild/member snowflake the way legacy stores it: decimal text.
fn snowflake_text(id: Snowflake) -> String {
    id.to_string()
}

/// Metadata JSON text, or NULL. `preserve_order` keeps insertion order so
/// bytes match legacy `JSON.stringify` (no key sorting).
fn metadata_text(event: &FunnelEvent) -> Option<String> {
    event.metadata.as_ref().map(|v| v.to_string())
}

async fn record_async(
    pool: &Pool<Postgres>,
    event: &FunnelEvent,
    observed_at: Option<&str>,
) -> Result<RecordOutcome, sqlx::Error> {
    // Build the insert row through the shared observation helpers so durable
    // inserts stamp exactly like memory: the hint lands verbatim in the blob,
    // invalid hints and non-membership events keep the event payload as-is.
    let mut new_row = StoredRow::from(event);
    if let Some(hint) = observed_at {
        set_observation(&mut new_row, hint);
    }
    let key = new_row.idempotency_key.clone();
    let member_id = new_row.member_id.map(snowflake_text);
    let guild_id = snowflake_text(new_row.guild_id);
    let metadata = new_row.metadata.as_ref().map(|v| v.to_string());
    let mut tx = pool.begin().await?;
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
    if inserted.is_none() {
        let outcome = advance_duplicate(&mut tx, event, &key, observed_at).await?;
        tx.commit().await?;
        return Ok(outcome);
    }
    project_event(&mut tx, event, &guild_id).await?;
    tx.commit().await?;
    Ok(RecordOutcome { inserted: true })
}

async fn record_observed_async(
    pool: &Pool<Postgres>,
    event: &FunnelEvent,
    observed_at: Option<&str>,
) -> Result<RecordOutcome, sqlx::Error> {
    record_async(pool, event, observed_at).await
}

/// Reconfirm an existing row: advance its stored observation maximum under
/// the event-row lock, never touching occurrence/source/identity. The winner
/// of `ON CONFLICT DO NOTHING` never reaches here; concurrent reconfirmations
/// serialize on `FOR UPDATE` and converge on the newest hint, so a delayed
/// stale duplicate cannot overwrite a newer maximum. The `IS NOT DISTINCT
/// FROM` predicate is the compare-and-swap fallback: the update lands only
/// against the value this transaction locked and read. Duplicates without a
/// usable hint skip the lock entirely.
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
        guild_id: event.guild_id,
        member_id: event.member_id,
        event_type: event.event_type,
        occurred_at: event.occurred_at.clone(),
        source: event.source.clone(),
        metadata: current_meta
            .as_deref()
            .map(|s| serde_json::from_str(s).expect("funnel observation metadata parses")),
        dedupe_token: event.dedupe_token.clone(),
        idempotency_key: key.to_owned(),
    };
    advance_observation(&mut stored, hint);
    let next = stored.metadata.as_ref().map(|v| v.to_string());
    if next != current_meta {
        sqlx::query(
            "UPDATE events SET metadata = $1 WHERE idempotency_key = $2 AND metadata IS NOT DISTINCT FROM $3",
        )
        .bind(next.as_deref())
        .bind(key)
        .bind(current_meta.as_deref())
        .execute(&mut **tx)
        .await?;
    }
    Ok(RecordOutcome { inserted: false })
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
    let rows: Vec<(
        String,
        Option<String>,
        String,
        String,
        String,
        Option<String>,
        String,
    )> = sqlx::query_as(
        "SELECT event_type, member_id, guild_id,
                to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),
                source, metadata, idempotency_key
         FROM events WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(snowflake_text(guild_id))
    .bind(snowflake_text(member_id))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)| {
                StoredRow {
                    guild_id: guild_id.parse().expect("funnel row guild id"),
                    member_id: member_id
                        .as_deref()
                        .map(|s| s.parse().expect("funnel row member id")),
                    event_type: EventType::from_wire(&event_type)
                        .expect("funnel row event type"),
                    occurred_at,
                    source,
                    metadata: metadata
                        .as_deref()
                        .map(|s| serde_json::from_str(s).expect("funnel row metadata parses")),
                    dedupe_token: None,
                    idempotency_key,
                }
            },
        )
        .collect())
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
        self.block_on(async {
            sqlx::query(
                "INSERT INTO members (guild_id, member_id, is_bot) VALUES ($1, $2, TRUE)
                 ON CONFLICT (guild_id, member_id) DO UPDATE SET is_bot = TRUE",
            )
            .bind(snowflake_text(guild_id))
            .bind(snowflake_text(member_id))
            .execute(&self.pool)
            .await
        })
        .expect("funnel mark_bot failed");
    }

    /// Insert-or-ignore by idempotency key; projection runs for the winner.
    ///
    /// # Panics
    ///
    /// Propagates a database failure as a panic: the sync [`FunnelStore`]
    /// seam has no fallible channel. A write failure must stop the pipeline
    /// rather than silently drop data; the release profile aborts the process
    /// so the container can restart without partially advanced in-memory state.
    fn record(&self, event: FunnelEvent) -> RecordOutcome {
        self.block_on(record_async(&self.pool, &event, None))
            .expect("funnel record failed")
    }

    /// Monotonic recency bump (never moves backwards). Failures panic — see
    /// [`FunnelStore::record`].
    fn touch_activity(&self, guild_id: Snowflake, member_id: Snowflake, at: &str) {
        let guild_id = snowflake_text(guild_id);
        let member_id = snowflake_text(member_id);
        let at = at.to_owned();
        self.block_on(async {
            sqlx::query(
                "INSERT INTO members (guild_id, member_id, last_active_at) VALUES ($1, $2, $3::timestamptz)
                 ON CONFLICT (guild_id, member_id) DO UPDATE SET last_active_at = excluded.last_active_at
                   WHERE members.last_active_at IS NULL OR members.last_active_at < excluded.last_active_at",
            )
            .bind(&guild_id)
            .bind(&member_id)
            .bind(&at)
            .execute(&self.pool)
            .await
        })
        .expect("funnel touch_activity failed");
    }

    /// Lowest empty message rung for a message arriving at `at`, or `None`
    /// when the ladder is full — including the redelivery guard (a rung is
    /// filled only strictly after the rung below). Mirrors [`MemStore`]
    /// exactly; failures panic — see [`FunnelStore::record`].
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
        let rows: Vec<(String, String)> = self
            .block_on(async {
                sqlx::query_as(
                    "SELECT event_type, to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
                     FROM events WHERE guild_id = $1 AND member_id = $2
                       AND event_type IN ('first_message', 'second_message', 'third_message')",
                )
                .bind(&guild_id)
                .bind(&member_id)
                .fetch_all(&self.pool)
                .await
            })
            .expect("funnel next_message_rung failed");
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

    /// Whether the member already has a row of this type. Failures panic —
    /// see [`FunnelStore::record`].
    fn has_event(&self, guild_id: Snowflake, member_id: Snowflake, event_type: EventType) -> bool {
        let guild_id = snowflake_text(guild_id);
        let member_id = snowflake_text(member_id);
        let event_type = event_type.as_str();
        self.block_on(async {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = $3)",
            )
            .bind(&guild_id)
            .bind(&member_id)
            .bind(event_type)
            .fetch_one(&self.pool)
            .await
        })
        .expect("funnel has_event failed")
    }
}

/// Chronological membership read model over the durable event log. The
/// projection derives from persisted event rows (not arrival-ordered
/// `members` columns), so historical joins and any-order replays converge
/// exactly like memory. Duplicate reconfirmations advance only the stored
/// observation maximum. Failures panic — see [`FunnelStore::record`].
impl MembershipStore for PgFunnelStore {
    fn membership(&self, guild_id: Snowflake, member_id: Snowflake) -> Option<Membership> {
        let rows = self
            .block_on(fetch_member_rows(&self.pool, guild_id, member_id))
            .expect("funnel membership failed");
        project(rows.iter())
    }

    fn membership_rows(&self, guild_id: Snowflake, member_id: Snowflake) -> Vec<StoredRow> {
        self.block_on(fetch_member_rows(&self.pool, guild_id, member_id))
            .expect("funnel membership rows failed")
    }

    /// Insert or reconfirm a membership event. Invalid/non-membership hints
    /// are ignored; an existing row's occurrence/source/other metadata stay
    /// put. Failures panic — see [`FunnelStore::record`].
    fn record_observed(&self, event: FunnelEvent, observed_at: Option<&str>) -> RecordOutcome {
        let observed_at = observed_at.map(str::to_owned);
        self.block_on(record_async(&self.pool, &event, observed_at.as_deref()))
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
