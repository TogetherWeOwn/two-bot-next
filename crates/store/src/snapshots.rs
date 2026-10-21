//! sqlx [`InviteSnapshotStore`]: `invite_snapshots` persistence.
//!
//! Ports the pipeline-owned mutex map (`PipelineSnapshots`) to Postgres:
//! `store_all` upserts each counter row, `delete_missing` removes codes that
//! vanished from the live listing, `load` reads the guild snapshot back.
//! Snowflakes persist as decimal TEXT, matching the funnel tables.

use std::collections::HashSet;

use sqlx::{Pool, Postgres};
use two_bot_core::{metrics, InviteSnapshotStore, InviteState, Snowflake};

/// Postgres-backed [`InviteSnapshotStore`]. Same sync→async bridge as
/// [`PgFunnelStore`](crate::PgFunnelStore): methods dispatch with
/// `block_in_place` and must not run on a `current_thread` runtime.
#[derive(Debug, Clone)]
pub struct PgInviteSnapshots {
    pool: Pool<Postgres>,
    handle: tokio::runtime::Handle,
}

impl PgInviteSnapshots {
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
}

/// Legacy `updated_at` shape: millis precision with a `Z` suffix.
/// A snapshot write is always "now"; the exact instant is bookkeeping, not
/// funnel data, so second precision is plenty and avoids a time dependency.
fn snapshot_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_millis().min(i64::MAX as u128)) as i64)
        .unwrap_or(0);
    two_bot_core::format_iso_millis(ms)
}

impl InviteSnapshotStore for PgInviteSnapshots {
    /// On a persistent DB error degrades to an empty snapshot with an error
    /// metric after bounded retries, never panicking — matching
    /// [`PgFunnelStore`](crate::PgFunnelStore) read behaviour.
    fn load(&self, guild_id: Snowflake) -> Vec<InviteState> {
        let guild_id = guild_id.to_string();
        match crate::retry::block_on_retry(&self.handle, || async {
            sqlx::query_as::<_, (String, i32, Option<String>, Option<String>)>(
                "SELECT code, uses, inviter_id, channel_id FROM invite_snapshots WHERE guild_id = $1",
            )
            .bind(&guild_id)
            .fetch_all(&self.pool)
            .await
        }) {
            Ok(rows) => rows,
            Err(error) => {
                metrics::global().db_error("other");
                tracing::warn!(
                    error = %error,
                    "invite snapshot load degraded to empty after retries"
                );
                // Fail closed as no snapshot: an empty baseline credits
                // nothing (growth needs a witnessed `before`), exactly like
                // the stale-baseline re-seed path — the outage
                // under-attributes joins instead of over-crediting them, and
                // the next healthy sync re-seeds the baseline. A rotation
                // against an empty read returns `None` having stored nothing.
                Vec::new()
            }
        }
        .into_iter()
        // `max_uses`/`temporary` are in-memory baseline pins for this slice:
        // the `invite_snapshots` table carries no such columns yet, so a
        // Postgres round-trip defaults them (a follow-up migration persists
        // them if rewards/fraud ever reads the cap). Attribution diffs only
        // `uses`, so the default loses nothing it diffs on.
        .map(|(code, uses, inviter_id, channel_id)| InviteState {
            code,
            uses: u64::try_from(uses).unwrap_or(0),
            inviter_id: inviter_id.and_then(|s| s.parse().ok()),
            channel_id: channel_id.and_then(|s| s.parse().ok()),
            max_uses: 0,
            temporary: false,
        })
        .collect()
    }

    /// # Panics
    ///
    /// Propagates a database failure as a panic after bounded retries are
    /// exhausted: the sync [`InviteSnapshotStore`] seam has no fallible
    /// channel. The message keeps its fixed vocabulary so the sync task's
    /// death is greppable; the release profile unwinds, so the panic kills
    /// only that task, never the supervisor.
    fn store_all(&self, guild_id: Snowflake, states: &[InviteState]) {
        let guild_id = guild_id.to_string();
        let stamp = snapshot_stamp();
        crate::retry::block_on_retry(&self.handle, || async {
            let mut tx = self.pool.begin().await?;
            // Serialize with concurrent opt-outs so the suppressed set below
            // is fresh. Lock order matches the opt-out's sorted order.
            sqlx::query("LOCK TABLE invite_snapshots, privacy_opt_outs IN ROW EXCLUSIVE MODE")
                .execute(&mut *tx)
                .await?;
            // Suppressed inviters keep the aggregate `uses` count but lose
            // the per-user attribution; the set is fetched once for the batch.
            let suppressed: std::collections::HashSet<String> = sqlx::query_scalar(
                "SELECT member_id FROM privacy_opt_outs WHERE guild_id = $1",
            )
            .bind(&guild_id)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();
            for s in states {
                let inviter = s
                    .inviter_id
                    .map(|id| id.to_string())
                    .filter(|id| !suppressed.contains(id.as_str()));
                // Discord-fed `uses` is `u64`; the column is INTEGER. Saturate
                // out-of-range counters instead of panicking: the snapshot
                // write proceeds with the clamped value. The warn names only
                // the guild and code identifiers, never the raw counter.
                let saturated = two_bot_core::saturate_invite_uses(s.uses);
                if saturated != s.uses {
                    tracing::warn!(
                        guild_id = %guild_id,
                        code = %s.code,
                        "invite uses saturated at i32::MAX",
                    );
                }
                // `saturated <= i32::MAX`, so the cast cannot truncate.
                let uses = saturated as i32;
                sqlx::query(
                    "INSERT INTO invite_snapshots (guild_id, code, uses, inviter_id, channel_id, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6::timestamptz)
                     ON CONFLICT (guild_id, code) DO UPDATE SET
                       uses = excluded.uses, inviter_id = excluded.inviter_id,
                       channel_id = excluded.channel_id, updated_at = excluded.updated_at",
                )
                .bind(&guild_id)
                .bind(&s.code)
                .bind(uses)
                .bind(inviter)
                .bind(s.channel_id.map(|id| id.to_string()))
                .bind(&stamp)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await
        })
        .expect("invite snapshot store_all failed");
    }

    /// # Panics
    ///
    /// Propagates a database failure as a panic after bounded retries are
    /// exhausted, with the same fixed-vocabulary, task-local unwind
    /// contract as [`InviteSnapshotStore::store_all`].
    fn delete_missing(&self, guild_id: Snowflake, live_codes: &HashSet<String>) {
        let guild_id = guild_id.to_string();
        let live: Vec<String> = live_codes.iter().cloned().collect();
        crate::retry::block_on_retry(&self.handle, || async {
            // `<> ALL(...)` with an empty array is true for every row, which
            // is exactly the "no invites live" case: clear the snapshot.
            sqlx::query("DELETE FROM invite_snapshots WHERE guild_id = $1 AND code <> ALL($2)")
                .bind(&guild_id)
                .bind(&live)
                .execute(&self.pool)
                .await
        })
        .expect("invite snapshot delete_missing failed");
    }
}

#[cfg(test)]
mod privacy_lock_tests {
    //! Pin the `store_all` table lock against concurrent opt-outs.
    //!
    //! The simulated opt-out takes the same SHARE ROW EXCLUSIVE lock the
    //! real `opt_out_member` takes, then purges and suppresses. Without the
    //! writer's ROW EXCLUSIVE lock the suppressed set is stale and the
    //! attribution lands after the purge: removing the lock (or the
    //! suppression filter) leaves a row naming the suppressed inviter.

    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Pool, Postgres};

    use two_bot_core::{InviteSnapshotStore, InviteState};

    use super::PgInviteSnapshots;

    const GUILD: &str = "1545644954272137297";
    const MEMBER_A: &str = "123456789012345678";

    async fn test_pool(prefix: &str) -> Option<(Pool<Postgres>, String)> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                assert_ne!(
                    std::env::var("GITHUB_ACTIONS").as_deref(),
                    Ok("true"),
                    "CI must configure the store privacy database suite"
                );
                eprintln!("SKIP snapshot privacy lock test: TWO_TEST_DATABASE_URL not set");
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn store_all_races_opt_out_never_attributing() {
        let Some((pool, schema)) = test_pool("snapshot_privacy_race").await else {
            return;
        };
        let guild: u64 = GUILD.parse().unwrap();
        let member: u64 = MEMBER_A.parse().unwrap();

        // Distinct codes so a late write cannot be overwritten by a later
        // iteration that sees the suppression; any post-commit iteration
        // leaves a row naming the inviter. The deterministic test below is
        // the true lock pin (single write, no timing dependence).
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let store = PgInviteSnapshots::new(writer_pool);
            for i in 0..30_u64 {
                store.store_all(
                    guild,
                    &[InviteState {
                        code: format!("race-{i}"),
                        uses: i,
                        inviter_id: Some(member),
                        channel_id: None,
                        max_uses: 0,
                        temporary: false,
                    }],
                );
            }
        });
        let racer_pool = pool.clone();
        let opt_out = tokio::spawn(async move {
            let mut tx = racer_pool.begin().await.expect("opt-out tx");
            sqlx::query(
                "LOCK TABLE community_facts, events, invite_snapshots, members, \
                 privacy_opt_outs, privacy_opt_out_audit IN SHARE ROW EXCLUSIVE MODE",
            )
            .execute(&mut *tx)
            .await
            .expect("opt-out lock");
            sqlx::query("DELETE FROM invite_snapshots WHERE guild_id = $1 AND inviter_id = $2")
                .bind(GUILD)
                .bind(MEMBER_A)
                .execute(&mut *tx)
                .await
                .expect("opt-out purge");
            sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
                .bind(GUILD)
                .bind(MEMBER_A)
                .execute(&mut *tx)
                .await
                .expect("suppression install");
            tx.commit().await.expect("opt-out commit");
        });
        let (writer_out, _) = tokio::join!(writer, opt_out);
        writer_out.expect("writer task");

        let attributed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM invite_snapshots WHERE guild_id = $1 AND inviter_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            attributed, 0,
            "no snapshot row names the suppressed inviter after the race"
        );

        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }

    /// Deterministic pin for the `store_all` table lock: hold the opt-out's
    /// lock set open with the suppression installed but uncommitted, run one
    /// snapshot write (its gate reads before the commit, then it blocks on
    /// the table lock), commit, and assert nothing names the inviter. Without
    /// the writer's `LOCK TABLE` the gate passes and the upsert lands after
    /// the purge — a single iteration fails with no timing dependence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn store_all_deterministic_race_never_attributing() {
        let Some((pool, schema)) = test_pool("snapshot_privacy_deterministic").await else {
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
            let store = PgInviteSnapshots::new(writer_pool);
            store.store_all(
                guild,
                &[InviteState {
                    code: "race-single".to_owned(),
                    uses: 7,
                    inviter_id: Some(member),
                    channel_id: None,
                    max_uses: 0,
                    temporary: false,
                }],
            );
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        writer.await.expect("writer task");

        let attributed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM invite_snapshots WHERE guild_id = $1 AND inviter_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            attributed, 0,
            "single snapshot write racing the opt-out never attributes"
        );

        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }
}
