//! sqlx store for the website-contract jobs (TOG-10090).
//!
//! Ports the write paths of `src/jobs/communitySnapshots.ts`
//! (`writeCounterTables`, `writeRankTables`) and `src/jobs/scheduledEvents.ts`
//! (`replaceEvents`), the raid-window grounding read (`readRaidWindows`), and
//! the `web_v1` view applier (legacy `src/store/webContract.ts`; the SQL is
//! the contract in `sql/web_v1.sql`).
//!
//! Tick recipe (the S4 REST executor supplies the fetches when it lands; until
//! then this module is storage behind plain-data outcomes — no dispatcher, no
//! HTTP client, no timers here):
//!
//! 1. Hold a [`crate::JobGate`] guard across the whole tick (single-flight: a
//!    tick that cannot acquire the gate skips instead of overlapping).
//! 2. Counter tick: [`read_raid_windows`] → `None` is
//!    [`crate::CounterSkip::RaidHistoryNotGrounded`]; else fetch the roster,
//!    [`crate::build_counter_reading`], then [`write_counter`].
//! 3. Rank tick: additionally [`crate::match_rank_roles`] (`None` is
//!    [`crate::RankSkip::RankRoleMissing`]), then
//!    [`crate::build_community_snapshot`]. A non-nested ladder self-heals
//!    first ([`crate::plan_rank_heal`]: grant the missing lower rungs with an
//!    audit reason, bounded and hierarchy-fenced), then rebuilds before
//!    [`write_rank_snapshot`]. The `Invariant` error below is the still-bad
//!    backstop ([`crate::RankSkip::RanksNotNested`]).
//! 4. Events tick: fetch, [`crate::normalize_events`], then
//!    [`replace_events`].
//!
//! A failed or ungrounded read returns before any write, so stale numbers age
//! out in `web_v1` on their own; a failed or malformed events read leaves the
//! last good mirror in place. Counts write as whole rows: a count and its
//! read time move together or not at all (the schema enforces the same rule).

use sqlx::{Pool, Postgres, Transaction};

use super::community_snapshots::{window_bounds, CommunitySnapshot, RaidWindow, RAID_ANOMALIES};
use super::scheduled_events::ScheduledEvent;

/// The contract version this build implements. Must match the row seeded in
/// migration 0300 and the changelog in `sql/web_v1.sql`.
pub const WEB_CONTRACT_VERSION: &str = "1.0";

/// Every view in the contract, in `sql/web_v1.sql` order. The website reads
/// exactly these; a view added here needs a `web_v1` edit, not a migration.
pub const WEB_CONTRACT_VIEWS: [&str; 9] = [
    "contract_meta",
    "live_counts",
    "rank_counts",
    "members",
    "member_milestones",
    "upcoming_events",
    "next_event",
    "funnel_daily",
    "funnel_by_source",
];

/// The `web_v1` contract DDL. `include_str!` (not a runtime file read) so the
/// release binary carries the exact SQL the tests exercise; a missing file
/// fails the build, never a boot.
const WEB_CONTRACT_SQL: &str = include_str!("../../../sql/web_v1.sql");

/// Store failure: database error vs the rank invariant backstop.
#[derive(Debug, thiserror::Error)]
pub enum WebsiteStoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    /// The snapshot is not publishable (non-nested ladder, or more ranked
    /// members than humans). Nothing was written; the tick maps this to
    /// [`crate::RankSkip::RanksNotNested`].
    #[error(
        "rank snapshot violates the website invariant (nested={nested}, ranked={ranked}, humans={humans}); wrote nothing"
    )]
    Invariant {
        nested: bool,
        ranked: usize,
        humans: usize,
    },
}

/// Ground the raid windows against the funnel history (legacy
/// `readRaidWindows`): for each known raid window, members who joined inside
/// it and are still present without ever posting or entering voice are the
/// exclusion set (the exact `scripts/raid-list.ts` removal rule).
///
/// Returns `None` when any window has no joins on file — the history the
/// exclusion grounds on is not there yet, so the tick must publish nothing
/// (legacy `raid_history_not_grounded`).
pub async fn read_raid_windows(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<Option<Vec<RaidWindow>>, sqlx::Error> {
    let mut windows = Vec::with_capacity(RAID_ANOMALIES.len());
    for anomaly in RAID_ANOMALIES {
        let Some((from, to)) = window_bounds(anomaly.start, anomaly.end) else {
            return Ok(None);
        };
        let rows: Vec<(String, bool, bool, bool)> = sqlx::query_as(
            "SELECT member_id,
                    first_message_at IS NULL AS no_message,
                    first_voice_at IS NULL AS no_voice,
                    left_at IS NULL AS present
               FROM members
              WHERE guild_id = $1
                AND joined_at >= $2::timestamptz
                AND joined_at < $3::timestamptz
                AND NOT is_bot",
        )
        .bind(guild_id)
        .bind(&from)
        .bind(&to)
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        windows.push(RaidWindow {
            id: anomaly.id.to_owned(),
            excluded_member_ids: rows
                .into_iter()
                .filter(|(_, no_message, no_voice, present)| *no_message && *no_voice && *present)
                .map(|(member_id, _, _, _)| member_id)
                .collect(),
        });
    }
    Ok(Some(windows))
}

/// Counter tick write (legacy `writeCommunitySnapshot` → `writeCounterTables`):
/// the same reading lands in `guild_counters` (what `web_v1.live_counts`
/// serves) and `counter_snapshots` (the audit trail) in one transaction, and
/// the contract pins to this guild.
pub async fn write_counter(
    pool: &Pool<Postgres>,
    guild_id: &str,
    observed_at: &str,
    human_member_count: i32,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    write_counter_tables(&mut tx, guild_id, observed_at, human_member_count).await?;
    tx.commit().await
}

async fn write_counter_tables(
    tx: &mut Transaction<'_, Postgres>,
    guild_id: &str,
    observed_at: &str,
    human_member_count: i32,
) -> Result<(), sqlx::Error> {
    // Two static statements, not one formatted loop: the table names stay
    // reviewable and sqlx-visible, matching the cutover `db.rs` convention.
    sqlx::query(
        "INSERT INTO counter_snapshots (guild_id, human_member_count, human_member_count_at)
         VALUES ($1, $2, $3)
         ON CONFLICT (guild_id) DO UPDATE SET
           human_member_count = excluded.human_member_count,
           human_member_count_at = excluded.human_member_count_at",
    )
    .bind(guild_id)
    .bind(human_member_count)
    .bind(observed_at)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO guild_counters (guild_id, human_member_count, human_member_count_at)
         VALUES ($1, $2, $3)
         ON CONFLICT (guild_id) DO UPDATE SET
           human_member_count = excluded.human_member_count,
           human_member_count_at = excluded.human_member_count_at",
    )
    .bind(guild_id)
    .bind(human_member_count)
    .bind(observed_at)
    .execute(&mut **tx)
    .await?;
    // Pin the contract to the guild whose aggregates this transaction wrote.
    sqlx::query("UPDATE web_contract_meta SET guild_id = $1 WHERE singleton = TRUE")
        .bind(guild_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Rank tick write (legacy `runRankSnapshotCycle` transaction): the counter,
/// the five rank aggregates, the per-member highest-rank cache and the public
/// exclusions commit together. Publishing a new denominator with yesterday's
/// ladder would make the cross-view invariant unprovable at the exact moment
/// it matters, so this is one commit or nothing.
///
/// Refuses a non-nested ladder or a ranked-over-human count before opening
/// the transaction (legacy `rank_snapshot_invariant_failed`): a finding, not
/// a row. The rank tick heals a non-cumulative roster before calling this, so
/// reaching the refusal means the ladder is still bad after the heal.
pub async fn write_rank_snapshot(
    pool: &Pool<Postgres>,
    guild_id: &str,
    observed_at: &str,
    snapshot: &CommunitySnapshot,
) -> Result<(), WebsiteStoreError> {
    if !snapshot.nested || snapshot.ranked_member_count > snapshot.human_member_count {
        return Err(WebsiteStoreError::Invariant {
            nested: snapshot.nested,
            ranked: snapshot.ranked_member_count,
            humans: snapshot.human_member_count,
        });
    }
    let mut tx = pool.begin().await?;
    write_counter_tables(
        &mut tx,
        guild_id,
        observed_at,
        snapshot.human_member_count as i32,
    )
    .await?;
    for row in &snapshot.rank_rows {
        sqlx::query("UPDATE rank_ladder SET rank_label = $1, role_id = $2 WHERE rank_key = $3")
            .bind(row.key.label())
            .bind(&row.role_id)
            .bind(row.key.key())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO rank_snapshots (guild_id, rank_key, member_count, holders_count, snapshot_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (guild_id, rank_key) DO UPDATE SET
               member_count = excluded.member_count,
               holders_count = excluded.holders_count,
               snapshot_at = excluded.snapshot_at",
        )
        .bind(guild_id)
        .bind(row.key.key())
        .bind(row.member_count as i32)
        .bind(row.holders_count as i32)
        .bind(observed_at)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM member_ranks WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut *tx)
        .await?;
    for member in &snapshot.member_ranks {
        sqlx::query(
            "INSERT INTO member_ranks (guild_id, member_id, rank_key, updated_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(guild_id)
        .bind(&member.member_id)
        .bind(member.rank_key.map(|k| k.key()))
        .bind(observed_at)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM member_exclusions WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut *tx)
        .await?;
    for member_id in &snapshot.excluded_member_ids {
        sqlx::query(
            "INSERT INTO member_exclusions (guild_id, member_id, reason, updated_at)
             VALUES ($1, $2, 'raid', $3)",
        )
        .bind(guild_id)
        .bind(member_id)
        .bind(observed_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Events tick write (legacy `replaceEvents`): a successful Discord read
/// replaces the guild's mirror atomically — including replacing it with zero
/// rows. The tick only calls this after [`crate::normalize_events`] accepts
/// the whole response, so a failed or malformed read never reaches here and
/// the last good snapshot stays in place.
///
/// Ordering against event mutations (TOG-20273): every mirror row carries the
/// writer's UTC-millis `observed_at` in `updated_at`, and all writers use the
/// same fixed-width rendering (`format_iso_millis` / `now_iso`), so TEXT
/// comparison is chronological. Stamps describe when the observation began:
/// the poller stamps before its GET and the mutation executor stamps after
/// Discord returns, so a mutation that lands while a snapshot GET is in flight
/// keeps its newer row (and a legitimately removed event is still deleted once
/// the snapshot observing the removal is the newest writer). A stale snapshot
/// replayed after a newer write changes nothing.
pub async fn replace_events(
    pool: &Pool<Postgres>,
    guild_id: &str,
    observed_at: &str,
    events: &[ScheduledEvent],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM scheduled_events WHERE guild_id = $1 AND updated_at <= $2")
        .bind(guild_id)
        .bind(observed_at)
        .execute(&mut *tx)
        .await?;
    for event in events {
        sqlx::query(
            "INSERT INTO scheduled_events
               (guild_id, event_id, name, starts_at, channel_id, description, status, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (guild_id, event_id) DO UPDATE SET
               name = EXCLUDED.name, starts_at = EXCLUDED.starts_at,
               channel_id = EXCLUDED.channel_id, description = EXCLUDED.description,
               status = EXCLUDED.status, updated_at = EXCLUDED.updated_at
             WHERE scheduled_events.updated_at <= EXCLUDED.updated_at",
        )
        .bind(guild_id)
        .bind(&event.id)
        .bind(&event.name)
        .bind(&event.starts_at)
        .bind(&event.channel_id)
        .bind(&event.description)
        .bind(event.status.as_str())
        .bind(observed_at)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE web_contract_meta SET guild_id = $1 WHERE singleton = TRUE")
        .bind(guild_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Internal event action write: refresh exactly one row before acknowledging
/// the action. Unlike the poller's whole-guild swap, unrelated events survive.
///
/// Ordering against the poller (TOG-20273): the row is only overwritten when
/// the stored `updated_at` is at or below this mutation's `observed_at` (same
/// fixed-width UTC-millis rendering, so TEXT comparison is chronological).
/// The executor stamps after Discord returns, so the instant is the mutation's
/// own completion — never a pre-send reading that a newer snapshot could beat
/// despite landing earlier. A stale mutation whose mirror write loses the race
/// against a newer poller snapshot (or a newer mutation) becomes a silent
/// no-op: the Discord effect already happened, so this still returns `Ok` —
/// the mirror simply keeps the newer row. Inserts (no conflicting row) always
/// apply.
pub async fn upsert_event(
    pool: &Pool<Postgres>,
    guild_id: &str,
    observed_at: &str,
    event: &ScheduledEvent,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO scheduled_events
           (guild_id, event_id, name, starts_at, channel_id, description, status, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (guild_id, event_id) DO UPDATE SET
           name = EXCLUDED.name, starts_at = EXCLUDED.starts_at,
           channel_id = EXCLUDED.channel_id, description = EXCLUDED.description,
           status = EXCLUDED.status, updated_at = EXCLUDED.updated_at
         WHERE scheduled_events.updated_at <= EXCLUDED.updated_at",
    )
    .bind(guild_id)
    .bind(&event.id)
    .bind(&event.name)
    .bind(&event.starts_at)
    .bind(&event.channel_id)
    .bind(&event.description)
    .bind(event.status.as_str())
    .bind(observed_at)
    .execute(pool)
    .await?;
    Ok(())
}

impl crate::scheduled_events::ScheduledEventMirror for Pool<Postgres> {
    async fn upsert(
        &self,
        guild_id: &str,
        observed_at: &str,
        event: &ScheduledEvent,
    ) -> Result<(), String> {
        upsert_event(self, guild_id, observed_at, event)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Apply the `web_v1` contract views (legacy `applyWebContract`: the SQL file
/// is the contract, this only runs it). Idempotent — everything in the file
/// is `CREATE OR REPLACE` / `IF NOT EXISTS` — so the boot path can run it on
/// every start; a `web_v1` edit that renames, reorders, removes or retypes a
/// column fails here loudly instead of silently breaking the website.
pub async fn apply_web_contract(pool: &Pool<Postgres>) -> Result<(), sqlx::Error> {
    // Resolve and apply on the same connection: a pool can hand out different
    // sessions, each with its own search_path.
    let mut connection = pool.acquire().await?;
    let bot_schema: Option<String> = sqlx::query_scalar("SELECT current_schema()::text")
        .fetch_one(&mut *connection)
        .await?;
    let bot_schema = bot_schema.ok_or_else(|| {
        contract_configuration_error("current_schema() is null: no existing schema in search_path")
    })?;
    let web_schema = web_schema_for(&bot_schema)?;
    let sql = rewrite_contract_schema(WEB_CONTRACT_SQL, &web_schema);
    // Multi-statement DDL with plpgsql bodies: `raw_sql`, not `query` (the
    // extended protocol runs one statement; `;`-splitting would shred the
    // function bodies). https://docs.rs/sqlx/0.9.0/sqlx/fn.raw_sql.html
    // SQLx 0.9 requires an explicit audit of dynamic SQL. The only dynamic
    // token is web_schema, strictly validated by web_schema_for above.
    // https://docs.rs/sqlx/0.9.0/sqlx/struct.AssertSqlSafe.html
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(&mut *connection)
        .await?;
    Ok(())
}

fn contract_configuration_error(message: &'static str) -> sqlx::Error {
    sqlx::Error::Configuration(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    )))
}

fn web_schema_for(bot_schema: &str) -> Result<String, sqlx::Error> {
    // Legacy assertSafeSchema: [a-z_][a-z0-9_]{0,58}. Validate the
    // derived identifier too, before interpolation (never allow truncation).
    fn safe_schema(name: &str) -> bool {
        let bytes = name.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 59
            && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
    }
    if !safe_schema(bot_schema) {
        return Err(contract_configuration_error("unsafe bot schema name"));
    }
    let web_schema = if bot_schema == "public" {
        "web_v1".to_owned()
    } else {
        format!("{bot_schema}_web_v1")
    };
    if !safe_schema(&web_schema) {
        return Err(contract_configuration_error("unsafe contract schema name"));
    }
    Ok(web_schema)
}

fn rewrite_contract_schema(source: &str, web_schema: &str) -> String {
    // Port the legacy whole-word replacement without adding a regex dependency.
    fn word_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }
    let mut sql = String::with_capacity(source.len());
    let mut copied_until = 0;
    for (start, token) in source.match_indices("web_v1") {
        let end = start + token.len();
        let before = start.checked_sub(1).map(|i| source.as_bytes()[i]);
        let after = source.as_bytes().get(end).copied();
        if before.is_some_and(word_byte) || after.is_some_and(word_byte) {
            continue;
        }
        sql.push_str(&source[copied_until..start]);
        sql.push_str(web_schema);
        copied_until = end;
    }
    sql.push_str(&source[copied_until..]);
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_contract_schema_matches_legacy_and_rejects_unsafe_identifiers() {
        assert_eq!(web_schema_for("public").unwrap(), "web_v1");
        assert_eq!(web_schema_for("bot_a").unwrap(), "bot_a_web_v1");
        assert_eq!(web_schema_for("_bot2").unwrap(), "_bot2_web_v1");
        for name in ["", "Bot", "1bot", "bot-a", "bot; DROP SCHEMA public", "böt"] {
            assert!(web_schema_for(name).is_err(), "unsafe identifier accepted");
        }
        assert!(web_schema_for(&"a".repeat(52)).is_ok());
        assert!(web_schema_for(&"a".repeat(53)).is_err());
        assert!(web_schema_for(&"a".repeat(60)).is_err());
    }

    #[test]
    fn contract_schema_replacement_matches_whole_words_only() {
        let source = "web_v1._ts web_v1 aweb_v1 web_v1x _web_v1 web_v1_ (web_v1)";
        assert_eq!(
            rewrite_contract_schema(source, "bot_a_web_v1"),
            "bot_a_web_v1._ts bot_a_web_v1 aweb_v1 web_v1x _web_v1 web_v1_ (bot_a_web_v1)"
        );
        assert_eq!(rewrite_contract_schema(source, "web_v1"), source);
    }
}
