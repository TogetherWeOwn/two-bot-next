//! Game-activity guild-aggregate sqlx store.
//!
//! Ports the durable half of the game-activity analytics pipeline onto the
//! `0440`/`0441` table: [`upsert_guild_aggregates`] adds one flush of
//! [`GuildGameAggregate`] rows into
//! the per-(guild, game) counters, and
//! [`read_guild_aggregates`] reads a guild's rows back
//! for the operator trend report. There is deliberately no per-member row,
//! no per-member reader and no public-surface view: the table carries no
//! member column at all (presence-probe containment precedent).
//!
//! Behind the `db` feature so domain unit tests never need a Postgres driver.
//! The pure pipeline ([`sessionize`] /
//! [`aggregate_guild`]) stays in
//! [`crate::game_activity`]; this module only moves guild-level rows, plus
//! the opt-out gate ([`aggregate_filtered`]) that keeps opted-out members'
//! deltas and spans out of the persisted aggregates.
//!
//! `game` is stored NOT NULL with `''` meaning untitled (`None` in the
//! aggregate): Postgres treats NULL key parts as distinct, so a nullable
//! game would admit duplicate untitled rows per guild. The writer maps
//! `None` to `''` and the reader maps `''` back; `bounded()` never yields
//! `Some("")`, so the mapping is unambiguous.
//!
//! Upserts are additive and the flush buffers drain before the write, so each
//! interval persists at most once: a retried tick after a drained buffer
//! aggregates nothing and writes nothing. A persist failure requeues the
//! drained buffers (see the bot flush loop); the batch itself is atomic, so
//! the retried whole batch counts exactly once and never double-counts a
//! partially written prefix.

use std::collections::BTreeSet;

use sqlx::{PgConnection, Pool, Postgres};

use super::game_activity::{
    aggregate_guild, sessionize, GuildGameAggregate, PresenceDelta, VoiceSpan,
};
use super::privacy::{category_off_in, AnalyticsCategory};

/// Game-activity store failure: transport only. A malformed aggregate can
/// never come back — the column CHECKs plus the domain's
/// [`aggregate_guild`] clamp keep
/// rows clean.
#[derive(Debug, thiserror::Error)]
pub enum GameActivityStoreError {
    #[error("game-activity store database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// The stored `game` key: `None` (untitled) maps to `''`, every title maps
/// to itself. The reader inverts this with [`split_game_key`].
#[must_use]
pub fn join_game_key(game: Option<&str>) -> &str {
    game.unwrap_or("")
}

/// The inverse of [`join_game_key`]: `''` reads back as untitled.
#[must_use]
pub fn split_game_key(game: &str) -> Option<String> {
    if game.is_empty() {
        None
    } else {
        Some(game.to_owned())
    }
}

/// Add one flush of guild-level aggregates into the per-(guild, game)
/// counters. Empty input writes nothing and reports zero rows; every row
/// adds its play millis, in-voice millis and session count onto the stored
/// totals and refreshes `updated_at`.
///
/// The batch is atomic: all rows commit together inside one transaction, so
/// a mid-batch failure leaves every covered row exactly as it was and the
/// flush loop's whole-batch retry (via requeue) counts exactly once.
pub async fn upsert_guild_aggregates(
    pool: &Pool<Postgres>,
    aggregates: &[GuildGameAggregate],
) -> Result<u64, GameActivityStoreError> {
    if aggregates.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let written = upsert_guild_aggregates_in(&mut tx, aggregates).await?;
    tx.commit().await?;
    Ok(written)
}

/// [`upsert_guild_aggregates`] on a borrowed connection: the rows join the
/// caller's transaction instead of opening their own. The flush runs its
/// suppression gate reads and this batch in one serializing transaction, so
/// the gate and the upsert linearize atomically against opt-outs. Empty
/// input writes nothing and reports zero rows, as the pooled wrapper.
pub async fn upsert_guild_aggregates_in(
    connection: &mut PgConnection,
    aggregates: &[GuildGameAggregate],
) -> Result<u64, GameActivityStoreError> {
    if aggregates.is_empty() {
        return Ok(0);
    }
    let mut written = 0u64;
    for aggregate in aggregates {
        written = written.saturating_add(
            sqlx::query(
                "INSERT INTO game_activity_guild_aggregates
                     (guild_id, game, total_play_ms, in_voice_ms, session_count)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (guild_id, game) DO UPDATE SET
                   total_play_ms = game_activity_guild_aggregates.total_play_ms
                     + EXCLUDED.total_play_ms,
                   in_voice_ms = game_activity_guild_aggregates.in_voice_ms
                     + EXCLUDED.in_voice_ms,
                   session_count = game_activity_guild_aggregates.session_count
                     + EXCLUDED.session_count,
                   updated_at = clock_timestamp()",
            )
            .bind(&aggregate.guild_id)
            .bind(join_game_key(aggregate.game.as_deref()))
            .bind(aggregate.total_play_ms.max(0))
            .bind(
                aggregate
                    .in_voice_ms
                    .max(0)
                    .min(aggregate.total_play_ms.max(0)),
            )
            .bind(i64::try_from(aggregate.session_count).unwrap_or(i64::MAX))
            .execute(&mut *connection)
            .await?
            .rows_affected(),
        );
    }
    Ok(written)
}

/// How long a quiet guild's aggregate rows survive: rows whose `updated_at`
/// is older than this many days expire on the retention cadence. Long enough
/// to keep operator trend reports useful across quiet spells, short enough
/// that abandoned guilds stop accumulating rows forever.
pub const GAME_ACTIVITY_AGGREGATE_RETENTION_DAYS: i32 = 90;

/// Process-environment key overriding
/// [`GAME_ACTIVITY_AGGREGATE_RETENTION_DAYS`]. Read on the retention cadence,
/// never on the flush/send path.
pub const AGGREGATE_RETENTION_DAYS_SETTING: &str = "TWO_GAME_ACTIVITY_AGGREGATE_RETENTION_DAYS";

/// Widest horizon the parser accepts: a century, far beyond any sane retention
/// window yet safely inside the `timestamptz` range, so an absurd operator
/// value can never push `now() - make_interval(days => $2)` out of range and
/// fail every retention tick.
pub const MAX_AGGREGATE_RETENTION_DAYS: i32 = 36_500;

/// Parse the [`AGGREGATE_RETENTION_DAYS_SETTING`] value. Missing, malformed,
/// non-positive, or unrepresentably large values fall back to
/// [`GAME_ACTIVITY_AGGREGATE_RETENTION_DAYS`], never to a wider horizon: an
/// unreadable setting must not widen into a full-table wipe (out-of-range
/// values refuse below in [`expire_stale_guild_aggregates`]).
#[must_use]
pub fn aggregate_retention_days_from_setting(value: Option<&str>) -> i32 {
    match value.and_then(|raw| raw.trim().parse::<i32>().ok()) {
        Some(days) if (1..=MAX_AGGREGATE_RETENTION_DAYS).contains(&days) => days,
        _ => GAME_ACTIVITY_AGGREGATE_RETENTION_DAYS,
    }
}

/// Refuse horizons outside `1..=MAX_AGGREGATE_RETENTION_DAYS` (fail-closed):
/// a sub-day value must not widen into a full-guild wipe, and an
/// unrepresentable one must not push the interval arithmetic out of range.
fn check_horizon(retention_days: i32) -> Result<(), GameActivityStoreError> {
    if (1..=MAX_AGGREGATE_RETENTION_DAYS).contains(&retention_days) {
        Ok(())
    } else {
        Err(GameActivityStoreError::Db(sqlx::Error::InvalidArgument(
            "bad horizon".into(),
        )))
    }
}

/// Guilds holding stale aggregate rows plus each guild's stale row count,
/// oldest guild key first, so the retention cadence sweeps each stale guild
/// under its own guild scope without spending its bounded tick re-walking
/// guilds whose rows are all fresh (a fresh-only prefix would otherwise
/// starve later stale guilds when the attempt budget cancels the run).
/// The per-guild counts are the dry-run preview behind the delete in
/// [`expire_stale_guild_aggregates`]: same predicate, no writes.
/// Horizons outside `1..=MAX_AGGREGATE_RETENTION_DAYS` are refused
/// (fail-closed).
///
/// The per-pass sweep uses [`list_aggregate_guild_page`] instead: this full
/// listing stays for small exact reads (reports, tests), never for a tick.
pub async fn list_aggregate_guilds(
    pool: &Pool<Postgres>,
    retention_days: i32,
) -> Result<Vec<(String, u64)>, GameActivityStoreError> {
    check_horizon(retention_days)?;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT guild_id, count(*) FROM game_activity_guild_aggregates WHERE updated_at < now() - make_interval(days => $1) GROUP BY guild_id ORDER BY guild_id ASC",
    )
    .bind(retention_days)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(guild_id, stale)| (guild_id, u64::try_from(stale.max(0)).unwrap_or(u64::MAX)))
        .collect())
}

/// One bounded keyset page of stale-guild candidates: at most `limit` guilds
/// holding stale rows, ordered by guild key ascending, resuming after
/// `after_guild` (exclusive; `None` starts at the first key). The per-guild
/// counts use the same stale predicate as [`expire_stale_guild_aggregates`],
/// so a dry-run pass is a same-predicate upper bound per pass for the visited
/// page: execute deletes at most the per-guild row batch per visited guild,
/// so over-batch guilds report fewer deleted rows per pass than previewed.
/// Stable under concurrent inserts: a guild sorts by
/// key, never by position, so a pass never re-walks already visited keys.
/// Horizons outside `1..=MAX_AGGREGATE_RETENTION_DAYS` are refused
/// (fail-closed); a zero `limit` reads one guild.
pub async fn list_aggregate_guild_page(
    pool: &Pool<Postgres>,
    retention_days: i32,
    after_guild: Option<&str>,
    limit: u32,
) -> Result<Vec<(String, u64)>, GameActivityStoreError> {
    check_horizon(retention_days)?;
    let limit = i64::from(limit.max(1));
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT guild_id, count(*) FROM game_activity_guild_aggregates \
         WHERE updated_at < now() - make_interval(days => $1) \
         AND ($2 IS NULL OR guild_id > $2) \
         GROUP BY guild_id ORDER BY guild_id ASC LIMIT $3",
    )
    .bind(retention_days)
    .bind(after_guild)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(guild_id, stale)| (guild_id, u64::try_from(stale.max(0)).unwrap_or(u64::MAX)))
        .collect())
}

/// How many stale-guild candidates sort after `after_guild` (exclusive;
/// `None` counts the whole eligible list): the `guilds_deferred` evidence a
/// pass reports, so a partial pass is distinguishable from a complete one.
/// A single scalar, never the rows themselves, so the count stays bounded no
/// matter how many guilds are stale. Same predicate and fail-closed horizon
/// as [`list_aggregate_guild_page`].
pub async fn count_stale_aggregate_guilds_after(
    pool: &Pool<Postgres>,
    retention_days: i32,
    after_guild: Option<&str>,
) -> Result<u64, GameActivityStoreError> {
    check_horizon(retention_days)?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT DISTINCT guild_id \
         FROM game_activity_guild_aggregates \
         WHERE updated_at < now() - make_interval(days => $1) \
         AND ($2 IS NULL OR guild_id > $2)) AS stale_guilds",
    )
    .bind(retention_days)
    .bind(after_guild)
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count.max(0)).unwrap_or(u64::MAX))
}

/// Delete one guild's aggregate rows older than `retention_days`, leaving
/// every other guild untouched. Only deletes: opt-out filtering stays at the
/// write-time gate ([`aggregate_filtered`]/[`upsert_guild_aggregates`]), so
/// expiry can neither persist an opted-out member's data nor resurrect it.
/// Horizons outside `1..=MAX_AGGREGATE_RETENTION_DAYS` are refused
/// (fail-closed): a sub-day value must not widen into a full-guild wipe, and
/// an unrepresentable one must not push the interval arithmetic out of range.
pub async fn expire_stale_guild_aggregates(
    pool: &Pool<Postgres>,
    guild_id: &str,
    retention_days: i32,
) -> Result<u64, GameActivityStoreError> {
    check_horizon(retention_days)?;
    Ok(sqlx::query(
        "DELETE FROM game_activity_guild_aggregates WHERE guild_id = $1 AND updated_at < now() - make_interval(days => $2)",
    )
    .bind(guild_id)
    .bind(retention_days)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Delete at most `limit` of one guild's stale aggregate rows, oldest game
/// key first, leaving every other guild untouched. Same predicate as
/// [`expire_stale_guild_aggregates`] plus the row cap, so an oversized guild
/// drains across passes: remaining stale rows stay eligible for the next
/// pass and are never silently lost. Same fail-closed horizon; a zero
/// `limit` deletes one row.
///
/// Each batch runs in its own transaction with `lock_timeout = '5s'` and
/// `statement_timeout = '15s'` (the cutover retention `SET LOCAL`
/// precedent): a row lock held by a stuck session fails this guild fast as
/// an isolated retryable error instead of blocking the pass until the
/// supervisor cancels it. Cancelling a blocked delete would pin its pooled
/// connection (sqlx 0.9 pings the dropped connection with no timeout), so
/// without these bounds two cancelled passes would exhaust the retention
/// pool and stop every later tick at `pool.begin()`.
pub async fn expire_stale_guild_aggregates_batch(
    pool: &Pool<Postgres>,
    guild_id: &str,
    retention_days: i32,
    limit: u32,
) -> Result<u64, GameActivityStoreError> {
    check_horizon(retention_days)?;
    let limit = i64::from(limit.max(1));
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    let deleted = sqlx::query(
        "DELETE FROM game_activity_guild_aggregates WHERE ctid IN \
         (SELECT ctid FROM game_activity_guild_aggregates \
         WHERE guild_id = $1 AND updated_at < now() - make_interval(days => $2) \
         ORDER BY game ASC LIMIT $3)",
    )
    .bind(guild_id)
    .bind(retention_days)
    .bind(limit)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(deleted)
}

/// A guild's stored rows, oldest game key first (`''`/untitled first), for
/// the operator trend report. No per-member reader exists by design.
pub async fn read_guild_aggregates(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<Vec<GuildGameAggregate>, GameActivityStoreError> {
    let rows: Vec<(String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT guild_id, game, total_play_ms, in_voice_ms, session_count
           FROM game_activity_guild_aggregates
          WHERE guild_id = $1
          ORDER BY game ASC",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(guild_id, game, total_play_ms, in_voice_ms, session_count)| GuildGameAggregate {
                guild_id,
                game: split_game_key(&game),
                total_play_ms,
                in_voice_ms,
                session_count: u64::try_from(session_count.max(0)).unwrap_or(u64::MAX),
            },
        )
        .collect())
}

/// Every member holding a guild-scoped opt-out suppression row for `guild`,
/// in one query so a flush never pays per-member round trips.
pub async fn opted_out_user_keys(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<BTreeSet<String>, GameActivityStoreError> {
    opted_out_user_keys_in(&mut *pool.acquire().await?, guild_id).await
}

/// [`opted_out_user_keys`] on a borrowed connection: the flush reads
/// suppression inside its serializing transaction, so the gate read and the
/// upsert share one lock span against concurrent opt-outs.
pub async fn opted_out_user_keys_in(
    connection: &mut PgConnection,
    guild_id: &str,
) -> Result<BTreeSet<String>, GameActivityStoreError> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT member_id FROM privacy_opt_outs WHERE guild_id = $1")
            .bind(guild_id)
            .fetch_all(&mut *connection)
            .await?;
    Ok(rows.into_iter().map(|(member_id,)| member_id).collect())
}

/// True when the guild disabled the presence analytics family, which also
/// gates game-activity persistence: the aggregates are presence-update
/// derived, mirroring the `presence_update` row gate in [`crate::privacy`].
/// A missing settings row means enabled.
pub async fn game_analytics_disabled(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<bool, GameActivityStoreError> {
    game_analytics_disabled_in(&mut *pool.acquire().await?, guild_id).await
}

/// [`game_analytics_disabled`] on a borrowed connection: the flush reads the
/// toggle inside its serializing transaction.
pub async fn game_analytics_disabled_in(
    connection: &mut PgConnection,
    guild_id: &str,
) -> Result<bool, GameActivityStoreError> {
    Ok(category_off_in(connection, guild_id, AnalyticsCategory::Presence).await?)
}

/// True when the guild disabled the voice analytics family. Each toggle is
/// independent per `docs/privacy.md`: with voice off but presence on, play
/// aggregates still persist while voice-derived `in_voice_ms` must stay 0.
/// A missing settings row means enabled.
pub async fn voice_analytics_disabled(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<bool, GameActivityStoreError> {
    voice_analytics_disabled_in(&mut *pool.acquire().await?, guild_id).await
}

/// [`voice_analytics_disabled`] on a borrowed connection: the flush reads the
/// toggle inside its serializing transaction.
pub async fn voice_analytics_disabled_in(
    connection: &mut PgConnection,
    guild_id: &str,
) -> Result<bool, GameActivityStoreError> {
    Ok(category_off_in(connection, guild_id, AnalyticsCategory::Voice).await?)
}

/// Drop every delta and span whose member opted out. Both sides filter on
/// the same key set so an opted-out member's voice time can never leak into
/// another member's session overlap.
#[must_use]
pub fn filter_opted_out(
    deltas: &[PresenceDelta],
    spans: &[VoiceSpan],
    opted_out: &BTreeSet<String>,
) -> (Vec<PresenceDelta>, Vec<VoiceSpan>) {
    (
        deltas
            .iter()
            .filter(|delta| !opted_out.contains(&delta.user_key))
            .cloned()
            .collect(),
        spans
            .iter()
            .filter(|span| !opted_out.contains(&span.user_key))
            .cloned()
            .collect(),
    )
}

/// Sessionize and aggregate one flush after the opt-out gate: opted-out
/// members' deltas and spans are filtered first, so they neither persist
/// nor influence another member's aggregates. Pure: the durable write is
/// [`upsert_guild_aggregates`].
#[must_use]
pub fn aggregate_filtered(
    guild_id: &str,
    deltas: &[PresenceDelta],
    spans: &[VoiceSpan],
    gap_timeout_ms: i64,
    opted_out: &BTreeSet<String>,
) -> Vec<GuildGameAggregate> {
    let (deltas, spans) = filter_opted_out(deltas, spans, opted_out);
    aggregate_guild(guild_id, &sessionize(&deltas, &spans, gap_timeout_ms))
}

#[cfg(test)]
mod tests {
    use super::super::game_activity::{ActivitySnapshot, ActivityType};
    use super::*;

    const MIN_MS: i64 = 60_000;
    const T0: i64 = 1_750_000_000_000;

    fn playing_delta(user_key: &str, at_ms: i64, game: &str) -> PresenceDelta {
        PresenceDelta {
            user_key: user_key.to_owned(),
            observed_at_ms: at_ms,
            activities: vec![ActivitySnapshot {
                activity_type: ActivityType::Playing,
                name: game.to_owned(),
                application_id: None,
                details: None,
                state: None,
                party_size: None,
                assets: false,
                button_labels: Vec::new(),
                button_urls: Vec::new(),
                stream_url: None,
            }],
        }
    }

    fn voice_span(user_key: &str, join_ms: i64, leave_ms: i64) -> VoiceSpan {
        VoiceSpan {
            user_key: user_key.to_owned(),
            join_ms,
            leave_ms,
            self_stream: false,
        }
    }

    #[test]
    fn opted_out_members_leave_no_session_and_no_voice_behind() {
        // Member 7 opts out after playing Apex for 30 minutes fully in voice;
        // member 8 keeps a 10-minute session with no voice. The persisted
        // aggregates must be member 8's alone: one session, 10 minutes play,
        // zero in-voice.
        let deltas = vec![
            playing_delta("user-7", T0, "Apex"),
            playing_delta("user-8", T0, "Apex"),
            playing_delta("user-7", T0 + 30 * MIN_MS, "Apex"),
            playing_delta("user-8", T0 + 10 * MIN_MS, "Apex"),
        ];
        let spans = vec![voice_span("user-7", T0, T0 + 30 * MIN_MS)];
        let opted_out: BTreeSet<String> = ["user-7".to_owned()].into_iter().collect();
        let rows = aggregate_filtered("guild-1", &deltas, &spans, 60 * MIN_MS, &opted_out);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].guild_id, "guild-1");
        assert_eq!(rows[0].game.as_deref(), Some("Apex"));
        assert_eq!(rows[0].session_count, 1, "only member 8 persists");
        assert_eq!(rows[0].total_play_ms, 10 * MIN_MS);
        assert_eq!(
            rows[0].in_voice_ms, 0,
            "member 7's voice span must not leak into member 8's session"
        );
    }

    #[test]
    fn empty_opt_out_set_keeps_every_member() {
        let deltas = vec![
            playing_delta("user-7", T0, "Apex"),
            playing_delta("user-7", T0 + 30 * MIN_MS, "Apex"),
        ];
        let spans = vec![voice_span("user-7", T0, T0 + MIN_MS)];
        let rows = aggregate_filtered("guild-1", &deltas, &spans, 60 * MIN_MS, &BTreeSet::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_count, 1);
        assert_eq!(rows[0].total_play_ms, 30 * MIN_MS);
        assert_eq!(rows[0].in_voice_ms, MIN_MS);
    }

    #[test]
    fn filter_drops_opted_out_deltas_and_spans_together() {
        let deltas = vec![
            playing_delta("user-7", T0, "Apex"),
            playing_delta("user-8", T0, "Apex"),
        ];
        let spans = vec![
            voice_span("user-7", T0, T0 + MIN_MS),
            voice_span("user-8", T0, T0 + MIN_MS),
        ];
        let opted_out: BTreeSet<String> = ["user-7".to_owned()].into_iter().collect();
        let (kept_deltas, kept_spans) = filter_opted_out(&deltas, &spans, &opted_out);
        assert!(kept_deltas.iter().all(|delta| delta.user_key != "user-7"));
        assert!(kept_spans.iter().all(|span| span.user_key != "user-7"));
        assert_eq!(kept_deltas.len(), 1);
        assert_eq!(kept_spans.len(), 1);
    }

    #[test]
    fn untitled_game_round_trips_through_the_empty_key() {
        assert_eq!(join_game_key(None), "");
        assert_eq!(join_game_key(Some("Apex")), "Apex");
        assert_eq!(split_game_key(""), None);
        assert_eq!(split_game_key("Apex"), Some("Apex".to_owned()));
    }

    /// Only an absent URL skips the test. Configured setup failures are errors.
    async fn test_pool(prefix: &str) -> Result<Option<(Pool<Postgres>, String)>, sqlx::Error> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => panic!("TWO_TEST_DATABASE_URL must be Unicode"),
        };
        test_pool_with_url(prefix, &url).await.map(Some)
    }

    async fn test_pool_with_url(
        prefix: &str,
        url: &str,
    ) -> Result<(Pool<Postgres>, String), sqlx::Error> {
        use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

        let options: PgConnectOptions = url.parse()?;
        // No DATABASE_URL or inherited credentials: tests accept only the
        // approved agent container or the ephemeral Postgres service in
        // GitHub Actions (see the `community-db` CI job).
        let host = options.get_host();
        assert!(
            host == "agent-testdb"
                || (host == "localhost"
                    && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
            "test-container-only URL"
        );
        assert_eq!(
            options.get_username(),
            "agent_test",
            "test-container-only user"
        );
        // No shared fixed-name fixtures: each invocation owns its schema,
        // including concurrent invocations of the same test in other processes.
        static NEXT_SCHEMA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let schema = format!(
            "{prefix}_{}_{nonce}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        assert!(is_test_schema_name(&schema), "safe unique test schema");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await?;
        // SAFETY: only the guarded identifier interpolates; values bind.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        // The aggregates table plus the opt-out control plane it gates on.
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0433_privacy_opt_out.sql"
        ))
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0441_game_activity_guild_aggregates.sql"
        ))
        .execute(&pool)
        .await?;
        Ok((pool, schema))
    }

    /// Test-only schema names: lowercase identifier characters, so DDL
    /// interpolation cannot break out of the identifier position.
    fn is_test_schema_name(schema: &str) -> bool {
        !schema.is_empty()
            && schema.len() <= 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
        assert!(is_test_schema_name(schema));
        // SAFETY: guarded by `is_test_schema_name` (see `test_pool`).
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }

    fn aggregate(
        guild_id: &str,
        game: Option<&str>,
        total_play_ms: i64,
        in_voice_ms: i64,
        session_count: u64,
    ) -> GuildGameAggregate {
        GuildGameAggregate {
            guild_id: guild_id.to_owned(),
            game: game.map(str::to_owned),
            total_play_ms,
            in_voice_ms,
            session_count,
        }
    }

    #[tokio::test]
    async fn flushes_accumulate_and_untitled_maps_to_the_empty_key() {
        let Some((pool, schema)) = test_pool("tog_23424_game")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping game_activity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // Empty input writes nothing.
        assert_eq!(
            upsert_guild_aggregates(&pool, &[])
                .await
                .expect("empty write"),
            0
        );
        // First flush: one titled row plus one untitled row.
        assert_eq!(
            upsert_guild_aggregates(
                &pool,
                &[
                    aggregate("guild-a", Some("Apex"), 30 * MIN_MS, MIN_MS, 1),
                    aggregate("guild-a", None, MIN_MS, 0, 1),
                ],
            )
            .await
            .expect("first flush"),
            2
        );
        // Second flush adds onto the same keys; another guild stays isolated.
        assert_eq!(
            upsert_guild_aggregates(
                &pool,
                &[
                    aggregate("guild-a", Some("Apex"), 10 * MIN_MS, 0, 2),
                    aggregate("guild-b", Some("Apex"), 99 * MIN_MS, 0, 9),
                ],
            )
            .await
            .expect("second flush"),
            2
        );
        let rows = read_guild_aggregates(&pool, "guild-a")
            .await
            .expect("read back");
        assert_eq!(rows.len(), 2);
        // Ordered by game key, untitled (`''`) first.
        assert_eq!(rows[0].game, None, "untitled reads back as None");
        assert_eq!((rows[0].total_play_ms, rows[0].session_count), (MIN_MS, 1));
        assert_eq!(rows[1].game.as_deref(), Some("Apex"));
        assert_eq!(rows[1].total_play_ms, 40 * MIN_MS, "flushes accumulate");
        assert_eq!(rows[1].in_voice_ms, MIN_MS);
        assert_eq!(rows[1].session_count, 3);
        // Guild isolation: the other guild's row is neither here nor hidden
        // from its own read, and an unknown guild reads empty.
        assert_eq!(
            read_guild_aggregates(&pool, "guild-b")
                .await
                .expect("other guild")
                .len(),
            1
        );
        assert!(read_guild_aggregates(&pool, "guild-absent")
            .await
            .expect("absent guild")
            .is_empty());
        // No member key anywhere in the persisted form.
        let stored: Vec<(String, String)> = sqlx::query_as(
            "SELECT guild_id, game FROM game_activity_guild_aggregates WHERE guild_id = 'guild-a'",
        )
        .fetch_all(&pool)
        .await
        .expect("raw read");
        assert!(
            stored
                .iter()
                .all(|(guild, game)| guild == "guild-a" && (game == "Apex" || game.is_empty())),
            "rows carry guild and game only, got {stored:?}"
        );
        drop_schema(&pool, &schema).await;
    }

    #[test]
    fn aggregate_retention_horizon_defaults_safe_and_never_widens() {
        assert_eq!(aggregate_retention_days_from_setting(None), 90);
        assert_eq!(
            aggregate_retention_days_from_setting(None),
            GAME_ACTIVITY_AGGREGATE_RETENTION_DAYS
        );
        assert_eq!(aggregate_retention_days_from_setting(Some("30")), 30);
        assert_eq!(aggregate_retention_days_from_setting(Some(" 45 ")), 45);
        // Malformed, non-positive, or unrepresentably large input falls back
        // to the safe default; nothing parses into a wider-than-default
        // horizon here, and sane explicit values stay exact.
        for fallback in [
            None,
            Some(""),
            Some("soon"),
            Some("0"),
            Some("-7"),
            Some("36501"),
            Some("2147483647"),
        ] {
            assert_eq!(
                aggregate_retention_days_from_setting(fallback),
                GAME_ACTIVITY_AGGREGATE_RETENTION_DAYS,
                "unreadable horizon must fall back, got {fallback:?}"
            );
        }
        assert_eq!(aggregate_retention_days_from_setting(Some("365")), 365);
        assert_eq!(
            aggregate_retention_days_from_setting(Some("36500")),
            MAX_AGGREGATE_RETENTION_DAYS
        );
    }

    #[tokio::test]
    async fn stale_rows_expire_per_guild_and_fresh_rows_survive() {
        let Some((pool, schema)) = test_pool("tog_28386_expire")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping game_activity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // Guild-a holds one stale game and one fresh game; guild-b holds one
        // stale game; guild-c holds one fresh game only. Backdating
        // `updated_at` stands in for a quiet guild: the flush path always
        // refreshes it on write.
        upsert_guild_aggregates(
            &pool,
            &[
                aggregate("guild-a", Some("Apex"), 30 * MIN_MS, MIN_MS, 1),
                aggregate("guild-a", Some("Valorant"), 10 * MIN_MS, 0, 1),
                aggregate("guild-b", Some("Apex"), 99 * MIN_MS, 0, 9),
                aggregate("guild-c", Some("Apex"), 5 * MIN_MS, 0, 1),
            ],
        )
        .await
        .expect("seed flush");
        for (guild, game) in [("guild-a", "Apex"), ("guild-b", "Apex")] {
            sqlx::query(
                "UPDATE game_activity_guild_aggregates
                    SET updated_at = now() - make_interval(days => 100)
                  WHERE guild_id = $1 AND game = $2",
            )
            .bind(guild)
            .bind(game)
            .execute(&pool)
            .await
            .expect("backdate stale row");
        }
        // The sweep listing names only guilds with stale rows with each
        // guild's stale count: the fresh-only guild never spends the
        // bounded tick. The counts are the dry-run preview of the same
        // rows the delete takes, without writing.
        assert_eq!(
            list_aggregate_guilds(&pool, 90)
                .await
                .expect("guild listing"),
            vec![("guild-a".to_owned(), 1), ("guild-b".to_owned(), 1),],
        );
        // Out-of-range horizons refuse instead of wiping the guild or
        // pushing interval arithmetic out of range.
        for bad in [0, -7, MAX_AGGREGATE_RETENTION_DAYS + 1, i32::MAX] {
            assert!(
                expire_stale_guild_aggregates(&pool, "guild-a", bad)
                    .await
                    .is_err(),
                "{bad}-day horizon must fail closed"
            );
            assert!(
                list_aggregate_guilds(&pool, bad).await.is_err(),
                "{bad}-day listing must fail closed"
            );
        }
        // Expiring guild-a deletes only its stale row: the fresh row stays,
        // and guild-b's equally stale row is untouched (per-guild isolation).
        assert_eq!(
            expire_stale_guild_aggregates(&pool, "guild-a", 90)
                .await
                .expect("expire guild-a"),
            1
        );
        let kept = read_guild_aggregates(&pool, "guild-a")
            .await
            .expect("read guild-a");
        assert_eq!(kept.len(), 1, "fresh row survives, got {kept:?}");
        assert_eq!(kept[0].game.as_deref(), Some("Valorant"));
        assert_eq!(kept[0].total_play_ms, 10 * MIN_MS);
        assert_eq!(
            read_guild_aggregates(&pool, "guild-b")
                .await
                .expect("read guild-b")
                .len(),
            1,
            "other guilds keep their rows until their own expiry"
        );
        // Guild-b's own expiry then clears it, and re-expiring an empty or
        // fresh guild deletes nothing.
        assert_eq!(
            expire_stale_guild_aggregates(&pool, "guild-b", 90)
                .await
                .expect("expire guild-b"),
            1
        );
        assert!(read_guild_aggregates(&pool, "guild-b")
            .await
            .expect("read emptied guild")
            .is_empty());
        assert_eq!(
            expire_stale_guild_aggregates(&pool, "guild-a", 90)
                .await
                .expect("re-expire fresh guild"),
            0
        );
        // The fresh-only guild was never listed, never counted stale, and
        // keeps its row.
        assert_eq!(
            read_guild_aggregates(&pool, "guild-c")
                .await
                .expect("read fresh-only guild")
                .len(),
            1
        );
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn opt_out_gate_reads_suppression_and_the_presence_toggle() {
        let Some((pool, schema)) = test_pool("tog_23424_gate")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping game_activity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // A missing settings row means enabled; no suppressions yet.
        assert!(!game_analytics_disabled(&pool, "guild-a")
            .await
            .expect("default enabled"));
        assert!(opted_out_user_keys(&pool, "guild-a")
            .await
            .expect("empty set")
            .is_empty());
        sqlx::query(
            "INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ('guild-a', 'user-7')",
        )
        .execute(&pool)
        .await
        .expect("suppress pair");
        sqlx::query(
            "INSERT INTO guild_analytics_settings (guild_id, presence_enabled)
             VALUES ('guild-a', FALSE)",
        )
        .execute(&pool)
        .await
        .expect("disable presence");
        assert_eq!(
            opted_out_user_keys(&pool, "guild-a")
                .await
                .expect("one suppression"),
            ["user-7".to_owned()].into_iter().collect(),
            "suppression is guild-scoped and exact"
        );
        assert!(game_analytics_disabled(&pool, "guild-a")
            .await
            .expect("disabled reads back"));
        // Other guilds are unaffected by either gate.
        assert!(!game_analytics_disabled(&pool, "guild-b")
            .await
            .expect("other guild enabled"));
        assert!(opted_out_user_keys(&pool, "guild-b")
            .await
            .expect("other guild empty")
            .is_empty());
        drop_schema(&pool, &schema).await;
    }
}
