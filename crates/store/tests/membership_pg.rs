//! Membership chronology contract against the durable store. Only
//! agent-testdb or a CI service container is admitted, with the agent_test
//! identity. DATABASE_URL is never consulted.
//!
//! Schema set-up runs once per test; each factory call truncates the log
//! back to empty, so the suite's many independent stores never see each
//! other's rows. The fixture owns one multi-thread tokio runtime and every
//! adapter shares it: the generic suite drives the sync seam from both the
//! plain test thread and `std::thread::scope` workers, which park outside
//! the runtime while its workers drive the I/O.
//!
//! The default-zone test preserves microseconds (`US` readback pattern) and
//! the offset-zone test pins every pooled connection to
//! `America/New_York` (`after_connect` + `SET TIME ZONE`): `AT TIME ZONE
//! 'UTC'` reads stay stable across both session zones.
//!
//! Run: `cargo test -p two-bot-store --locked --test membership_pg -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_core::membership::{normalize_timestamp, MembershipStore};
use two_bot_core::{EventType, FunnelEvent};
use two_bot_store::{migrate, PgFunnelStore, DB_POOL_MAX};

#[path = "../../core/tests/support/membership_contract.rs"]
mod contract;

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

fn test_options() -> PgConnectOptions {
    let url = std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL for DB tests");
    let options = PgConnectOptions::from_str(&url).expect("invalid TEST_DATABASE_URL");
    let ci_container = std::env::var("CI").as_deref() == Ok("true")
        && matches!(options.get_host(), "localhost" | "127.0.0.1" | "postgres");
    assert!(
        options.get_host() == "agent-testdb" || ci_container,
        "test containers only"
    );
    assert_eq!(options.get_username(), "agent_test", "test identity only");
    options
}

/// One migrated schema per test plus the shared runtime. Factory calls
/// truncate the log; the suite uses its stores sequentially, so a fresh
/// truncate per call is an empty, isolated namespace.
struct SchemaFixture {
    runtime: tokio::runtime::Runtime,
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl SchemaFixture {
    fn new(non_utc: bool) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("membership test runtime");
        let (admin, pool, schema) = runtime.block_on(async {
            let url =
                std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL for DB tests");
            let admin = two_bot_store::connect_pool(&url)
                .await
                .expect("test DB connect failed");
            let schema = format!(
                "s6_member_{}_{}",
                std::process::id(),
                NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
            );
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                .execute(&admin)
                .await
                .unwrap();
            let mut builder = PgPoolOptions::new().max_connections(DB_POOL_MAX);
            if non_utc {
                // Pin a non-UTC database session: every pooled connection
                // lands in America/New_York, so `AT TIME ZONE 'UTC'` reads
                // and microsecond round trips must hold under a foreign zone.
                builder = builder.after_connect(|conn, _| {
                    Box::pin(async move {
                        sqlx::query("SET TIME ZONE 'America/New_York'")
                            .execute(&mut *conn)
                            .await?;
                        Ok(())
                    })
                });
            }
            let pool = builder
                .connect_with(test_options().options([
                    ("search_path", schema.clone()),
                    ("statement_timeout", "15000ms".to_owned()),
                ]))
                .await
                .unwrap();
            migrate(&pool).await.unwrap();
            let zone: String = sqlx::query_scalar("SHOW TimeZone")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(
                zone,
                if non_utc { "America/New_York" } else { "UTC" },
                "session zone must match the adapter under test"
            );
            (admin, pool, schema)
        });
        Self {
            runtime,
            admin,
            pool,
            schema,
        }
    }

    /// Fresh, empty store. `PgFunnelStore::new` reads `Handle::current()`,
    /// so construction stays inside `block_on`; every later sync seam call
    /// parks the calling thread while fixture workers drive the I/O.
    fn make(&self) -> PgFunnelStore {
        self.runtime.block_on(async {
            sqlx::query("TRUNCATE events, members")
                .execute(&self.pool)
                .await
                .unwrap();
            PgFunnelStore::new(self.pool.clone())
        })
    }

    fn finish(self) {
        self.runtime.block_on(async {
            self.pool.close().await;
            // Only schemas created by this fixture; never shared tables.
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA IF EXISTS {}_web_v1 CASCADE; DROP SCHEMA {} CASCADE",
                self.schema, self.schema
            )))
            .execute(&self.admin)
            .await
            .unwrap();
            self.admin.close().await;
        });
    }
}

#[test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
fn sqlx_membership_chronology_contract() {
    let f = SchemaFixture::new(false);
    contract::run(|| f.make());
    f.finish();
}

#[test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
fn sqlx_membership_chronology_contract_non_utc_session() {
    let f = SchemaFixture::new(true);
    contract::run(|| f.make());
    f.finish();
}

/// Real parallel-writer compare-and-swap proof for the deferred
/// `docs/membership-contract.md` row (`membership-replay.test.ts:217`): two
/// writers race duplicate reconfirmations of the same once-per-member join
/// row behind a barrier, so both transactions overlap on the event-row lock.
/// The store serializes on `SELECT … FOR UPDATE` and converges on the newest
/// hint; a stale duplicate reconfirmation through the store then leaves the
/// maximum intact, failing loudly (assert) instead of losing the update.
#[test]
#[ignore = "requires authorized TEST_DATABASE_URL"]
fn sqlx_membership_parallel_writer_cas() {
    const GUILD: u64 = 1;
    const MEMBER: u64 = 2;
    const OCCURRED: &str = "2026-09-29T00:00:00.000Z";
    const SEED_HINT: &str = "2026-09-30T01:00:00.000010Z";
    const HINT_A: &str = "2026-09-30T01:00:00.000020Z";
    const HINT_B: &str = "2026-09-30T01:00:00.000030Z";
    const STALE_HINT: &str = "2026-09-30T01:00:00.000001Z";

    fn join() -> FunnelEvent {
        FunnelEvent {
            guild_id: GUILD,
            member_id: Some(MEMBER),
            event_type: EventType::MemberJoin,
            occurred_at: OCCURRED.into(),
            source: "invite:original".into(),
            metadata: None,
            dedupe_token: None,
        }
    }
    fn duplicate() -> FunnelEvent {
        FunnelEvent {
            source: "unknown".into(),
            ..join()
        }
    }
    fn observed(metadata: &Option<serde_json::Value>) -> Option<String> {
        metadata
            .as_ref()
            .and_then(|v| v.get("membershipObservedAt"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    let f = SchemaFixture::new(false);
    let s = f.make();
    // Seed the contested row: the once-per-member join key is shared by every
    // reconfirmation below, so both writers update the same events row.
    assert!(s.record_observed(join(), Some(SEED_HINT)).inserted);

    let barrier = std::sync::Barrier::new(2);
    let (a_inserted, b_inserted) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            s.record_observed(duplicate(), Some(HINT_A))
        });
        let b = scope.spawn(|| {
            barrier.wait();
            s.record_observed(duplicate(), Some(HINT_B))
        });
        (
            a.join().expect("writer A").inserted,
            b.join().expect("writer B").inserted,
        )
    });
    assert!(
        !a_inserted && !b_inserted,
        "racing reconfirmations are duplicates, not inserts"
    );

    // Newest hint wins; occurrence, source and identity stay with the seed.
    let rows = s.membership_rows(GUILD, MEMBER);
    assert_eq!(rows.len(), 1, "one join row, raced in place");
    assert_eq!(rows[0].event_type, EventType::MemberJoin);
    assert_eq!(rows[0].source, "invite:original");
    assert_eq!(
        normalize_timestamp(&rows[0].occurred_at).expect("row timestamp"),
        normalize_timestamp(OCCURRED).expect("fixture timestamp"),
        "occurrence never moves on reconfirmation"
    );
    assert_eq!(
        observed(&rows[0].metadata).as_deref(),
        Some(HINT_B),
        "parallel writers converge on the newest hint"
    );

    // A stale duplicate through the store cannot overwrite the converged
    // maximum: the reconfirmation takes the same `advance_duplicate` path as
    // the racing writers (row lock plus the `IS NOT DISTINCT FROM` revision
    // check), the older hint loses the maximum comparison, and the asserts
    // fail loudly on a lost update.
    assert!(
        !s.record_observed(duplicate(), Some(STALE_HINT)).inserted,
        "stale reconfirmation is a duplicate, not an insert"
    );
    let rows = s.membership_rows(GUILD, MEMBER);
    assert_eq!(
        observed(&rows[0].metadata).as_deref(),
        Some(HINT_B),
        "stale writer left the converged maximum intact"
    );
    f.finish();
}
