//! Acceptance: every `unmeasurable` rollback-delta table carries a recorded
//! per-table disposition in the JSON report, so a rollback decision never
//! meets an unmeasurable table with no named handling.
//!
//! Contract under test (JSON level, `docs/rollback-delta.md`):
//! - An `unmeasurable` entry gains `"disposition": "accept-with-reason"` or
//!   `"disposition": "manual-reconcile-required"` alongside its `reason`.
//! - `measured` and `missing` entries are byte-identical to before: no
//!   `disposition` key, same field order, same counts.
//! - The pinned map below covers exactly the unmeasurable specs in
//!   `TABLE_SPECS`: adding a new unmeasurable table without a disposition
//!   breaks the coverage test.
//! - The runbook names both disposition literals and their semantics.
//!
//! Disposition rule used for the pinned map: `accept-with-reason` only where
//! the spec reason or the reviewed voice-window disposition in
//! `docs/rollback-delta.md` names alternate measured evidence, an accepted
//! reason, seed reference data, or no durable state; every other
//! unmeasurable table is `manual-reconcile-required`.
//!
//! The database test follows `rollback_delta_db.rs`: real SQL against only
//! the disposable test service (agent-testdb or the CI service container),
//! never an application URL. Missing/refused DB access fails, not skips.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{Pool, Postgres, QueryBuilder};
use two_bot_cutover::rollback_delta::{report, TableMeasure, TABLE_SPECS};

const ACCEPT_WITH_REASON: &str = "accept-with-reason";
const MANUAL_RECONCILE_REQUIRED: &str = "manual-reconcile-required";

/// The pinned disposition for every unmeasurable table at the time of
/// writing. This is the coverage pin: it must cover exactly the
/// unmeasurable specs in `TABLE_SPECS`, no more and no fewer.
const EXPECTED_DISPOSITIONS: &[(&str, &str)] = &[
    // Catch-up inserts are invisible after the fact; no alternate measured
    // evidence is named for the window.
    ("backfill_history_messages", MANUAL_RECONCILE_REQUIRED),
    // A completed week is visible via community_scorecard_runs.generated_at.
    ("community_scorecard_attempts", ACCEPT_WITH_REASON),
    // Transient restart-recovery queue; rows hold no durable member state.
    ("gateway_onboarding_jobs", ACCEPT_WITH_REASON),
    // Changes are visible via guild_settings.updated_at.
    ("guild_settings_revision", ACCEPT_WITH_REASON),
    // Wholesale-replaced configuration with no alternate evidence named.
    ("level_role_rewards", MANUAL_RECONCILE_REQUIRED),
    // Child rows are created and updated with the parent lfg_posts row.
    ("lfg_roles", ACCEPT_WITH_REASON),
    // Written with its parent moderation_idempotency claim (claimed_at).
    ("moderation_channel_executions", ACCEPT_WITH_REASON),
    // Private report evidence is never exported or archived; no legacy
    // system reads it, so a rollback leaves the tables inert.
    ("member_reports", ACCEPT_WITH_REASON),
    // Seed reference data.
    ("rank_ladder", ACCEPT_WITH_REASON),
    ("report_reporters", ACCEPT_WITH_REASON),
    ("report_snapshots", ACCEPT_WITH_REASON),
    // Written with its exchange lifecycle (self_role_exchanges.created_at).
    ("self_role_exchange_baselines", ACCEPT_WITH_REASON),
    // Voice window: reviewed disposition accepts the reason (read-back pair).
    ("voice_access_controls", ACCEPT_WITH_REASON),
    // Voice window: wholesale-replaced configuration, accepted with the
    // freeze/rollback configuration export pair as alternate evidence.
    ("voice_channel_templates", ACCEPT_WITH_REASON),
    ("voice_command_role_members", ACCEPT_WITH_REASON),
    ("voice_command_roles", ACCEPT_WITH_REASON),
    // Voice window: accepted with the export pair plus full-row dump
    // comparison as alternate evidence.
    ("voice_creators", ACCEPT_WITH_REASON),
    ("voice_game_aliases", ACCEPT_WITH_REASON),
    ("voice_guild_settings", ACCEPT_WITH_REASON),
    ("voice_logging", ACCEPT_WITH_REASON),
    ("voice_logging_mention_members", ACCEPT_WITH_REASON),
    ("voice_logging_mention_roles", ACCEPT_WITH_REASON),
    // Voice window: reviewed disposition accepts the reason (read-back pair).
    ("voice_logging_settings", ACCEPT_WITH_REASON),
    ("voice_random_list_choices", ACCEPT_WITH_REASON),
    ("voice_random_lists", ACCEPT_WITH_REASON),
    // Singleton contract row; a window change is invisible and no alternate
    // measured evidence is named.
    ("web_contract_meta", MANUAL_RECONCILE_REQUIRED),
];

fn unmeasurable_spec_tables() -> BTreeSet<&'static str> {
    TABLE_SPECS
        .iter()
        .filter(|spec| matches!(spec.measure, TableMeasure::Unmeasurable(..)))
        .map(|spec| spec.table)
        .collect()
}

#[test]
fn acceptance_every_unmeasurable_spec_has_a_pinned_disposition() {
    let pinned: BTreeSet<&str> = EXPECTED_DISPOSITIONS.iter().map(|(t, _)| *t).collect();
    assert_eq!(
        pinned.len(),
        EXPECTED_DISPOSITIONS.len(),
        "pinned disposition map has duplicate tables"
    );
    for (table, disposition) in EXPECTED_DISPOSITIONS {
        assert!(
            [ACCEPT_WITH_REASON, MANUAL_RECONCILE_REQUIRED].contains(disposition),
            "table {table} has unknown disposition {disposition}"
        );
    }
    // Both dispositions must stay represented, so a single hard-coded value
    // cannot satisfy the battery.
    assert!(
        EXPECTED_DISPOSITIONS
            .iter()
            .any(|(_, d)| *d == ACCEPT_WITH_REASON),
        "pinned map lost every accept-with-reason table"
    );
    assert!(
        EXPECTED_DISPOSITIONS
            .iter()
            .any(|(_, d)| *d == MANUAL_RECONCILE_REQUIRED),
        "pinned map lost every manual-reconcile-required table"
    );
    // Exact coverage: a new unmeasurable spec (or a spec becoming measured)
    // breaks this test until its disposition is pinned here.
    let specs = unmeasurable_spec_tables();
    assert_eq!(
        specs, pinned,
        "unmeasurable specs and pinned dispositions differ; \
         pin a disposition for every unmeasurable table"
    );
}

#[test]
fn acceptance_runbook_names_both_dispositions() {
    let runbook = include_str!("../../../docs/rollback-delta.md");
    assert!(
        runbook.contains(ACCEPT_WITH_REASON),
        "docs/rollback-delta.md must name the `{ACCEPT_WITH_REASON}` disposition"
    );
    assert!(
        runbook.contains(MANUAL_RECONCILE_REQUIRED),
        "docs/rollback-delta.md must name the `{MANUAL_RECONCILE_REQUIRED}` disposition"
    );
}

// ---------------------------------------------------------------------------
// Database acceptance (same disposable-test-service setup as
// rollback_delta_db.rs; seeds identical rows so measured counts are known).
// ---------------------------------------------------------------------------

static SCHEMA_SEQ: AtomicU64 = AtomicU64::new(0);

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const SINCE: &str = "2026-09-30T12:00:00Z";

struct TestDb {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl TestDb {
    async fn new() -> TestResult<Self> {
        for key in ["PGOPTIONS", "PGSSLCERT", "PGSSLKEY", "PGSSLROOTCERT"] {
            assert!(
                std::env::var_os(key).is_none(),
                "inherited PG settings refused"
            );
        }
        let host = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let options = PgConnectOptions::new_without_pgpass()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("postgres")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone())
            .await?;
        let schema = format!(
            "rollback_disp_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            SCHEMA_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
            .push(&schema)
            .build()
            .execute(&admin)
            .await?;
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options.application_name(&schema))
            .await?;
        for migration in [
            include_str!("../migrations/0001_funnel.sql"),
            include_str!("../migrations/0002_leveling.sql"),
            include_str!("../migrations/0210_tickets.sql"),
            include_str!("../migrations/0224_voice_rooms.sql"),
            include_str!("../migrations/0330_guild_settings.sql"),
            include_str!("../migrations/0412_voice_rooms_ownership_touched.sql"),
            include_str!("../migrations/0414_voice_rooms_custom_name.sql"),
            include_str!("../migrations/0416_voice_room_privacy.sql"),
            include_str!("../migrations/0422_voice_create_reservations.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await?;
        }
        Self::seed(&pool).await?;
        Ok(Self {
            admin,
            pool,
            schema,
        })
    }

    /// Identical seeds to `rollback_delta_db.rs`, so the measured counts
    /// asserted below are the pre-change counts.
    async fn seed(pool: &Pool<Postgres>) -> TestResult {
        sqlx::raw_sql(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, recorded_at, source, idempotency_key) VALUES
             ('member_join', 'm1', 'g', '2026-09-29T12:00:00Z', '2026-09-29T12:00:00Z', 'test', 'e-before'),
             ('member_join', 'm2', 'g', '2026-10-01T12:00:00Z', '2026-10-01T12:00:00Z', 'test', 'e-after');
             INSERT INTO members (guild_id, member_id, joined_at, last_active_at) VALUES
             ('g', 'm-before', '2026-09-29T12:00:00Z', NULL),
             ('g', 'm-after', '2026-09-29T12:00:00Z', '2026-10-01T12:00:00Z');
             INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at) VALUES
             ('g', 'm1', 15, 15, '2026-09-29T12:00:00Z'),
             ('g', 'm2', 30, 30, '2026-10-01T12:00:00Z'),
             ('g', 'm3', 45, 45, '2026-10-01T12:00:00Z');
             INSERT INTO tickets (id, guild_id, channel_id, opener_id, status, created_at) VALUES
             ('t-before', 'g', 'ch-before', 'o1', 'closed', '2026-09-29T12:00:00Z'),
             ('t-after', 'g', 'ch-after', 'o2', 'open', '2026-10-01T12:00:00Z');
             INSERT INTO guild_settings (guild_id, key, value, version, updated_by) VALUES
             ('g', 'ROLLBACK_DELTA_PROBE', '\"probe\"', 1, 'test');
             INSERT INTO voice_rooms (guild_id, channel_id, creator_channel_id, owner_id, original_creator_id, name_seed, created_at, owner_touched_at, custom_name, name_touched_at, privacy_touched_at) VALUES
             ('g', 'room-before', 'creator-1', 'owner-1', 'owner-1', '7', '2026-09-29T12:00:00Z', '2026-09-29T12:00:00Z', 'Old name', '2026-09-29T12:00:00Z', NULL),
             ('g', 'room-handoff', 'creator-1', 'owner-2', 'owner-1', '8', '2026-09-29T12:00:00Z', '2026-10-01T12:00:00Z', NULL, NULL, NULL),
             ('g', 'room-renamed', 'creator-1', 'owner-3', 'owner-3', '9', '2026-09-29T12:00:00Z', '2026-09-29T12:00:00Z', 'New name', '2026-10-01T12:00:00Z', NULL),
             ('g', 'room-private', 'creator-1', 'owner-1', 'owner-1', '10', '2026-09-29T12:00:00Z', '2026-09-29T12:00:00Z', NULL, NULL, '2026-10-01T12:00:00Z');
             INSERT INTO voice_create_reservations (id, guild_id, user_id, created_at, channel_id, settled_at) VALUES
             ('claim-before', '100', '300', '2026-09-29T12:00:00Z', NULL, '2026-09-29T12:00:00Z'),
             ('claim-bound', '100', '301', '2026-09-29T12:00:00Z', '500', '2026-10-01T12:00:00Z'),
             ('claim-rollback', '100', '302', '2026-09-29T12:00:00Z', NULL, '2026-10-01T12:00:00Z');",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn finish(self) -> TestResult {
        self.pool.close().await;
        QueryBuilder::<Postgres>::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        Ok(())
    }
}

#[tokio::test]
async fn acceptance_report_carries_disposition_per_unmeasurable_table() -> TestResult {
    let db = TestDb::new().await?;
    let result = async {
        let summary = report(&db.pool, SINCE).await?;
        assert_eq!(summary.tables.len(), TABLE_SPECS.len());

        // Every unmeasurable entry carries its pinned disposition, keeps its
        // reason, and gains no count/columns.
        for (table, expected) in EXPECTED_DISPOSITIONS {
            let entry = summary
                .tables
                .iter()
                .find(|t| t.table == *table)
                .unwrap_or_else(|| panic!("classified table {table} missing from report"));
            assert_eq!(entry.status, "unmeasurable", "table {table}");
            assert!(entry.count.is_none(), "table {table}");
            let json: serde_json::Value = serde_json::to_value(entry)?;
            assert_eq!(
                json.get("disposition").and_then(|v| v.as_str()),
                Some(*expected),
                "table {table} must carry disposition {expected} in the report JSON"
            );
            assert!(
                json.get("reason")
                    .and_then(|v| v.as_str())
                    .is_some_and(|r| !r.is_empty()),
                "table {table} must keep its unmeasurable reason"
            );
            assert!(json.get("count").is_none(), "table {table}");
            assert!(json.get("columns").is_none(), "table {table}");
        }

        // No unmeasurable entry escapes the pinned map, and no measured or
        // missing entry gains a disposition key.
        let pinned: BTreeSet<&str> = EXPECTED_DISPOSITIONS.iter().map(|(t, _)| *t).collect();
        for entry in &summary.tables {
            let json: serde_json::Value = serde_json::to_value(entry)?;
            if entry.status == "unmeasurable" {
                assert!(
                    pinned.contains(entry.table.as_str()),
                    "unmeasurable table {} has no pinned disposition",
                    entry.table
                );
            } else {
                assert!(
                    json.get("disposition").is_none(),
                    "{} table {} must not carry a disposition",
                    entry.status,
                    entry.table
                );
            }
        }

        // Measured entries are byte-identical to the pre-change report:
        // same field order, same columns, same counts, no new keys.
        for (table, expected_json) in [
            (
                "events",
                r#"{"table":"events","status":"measured","columns":["recorded_at"],"count":1}"#,
            ),
            (
                "members",
                r#"{"table":"members","status":"measured","columns":["joined_at","first_message_at","third_message_at","first_voice_at","last_active_at","left_at","inactive_flagged_at","gate_cleared_at"],"count":1}"#,
            ),
            (
                "member_levels",
                r#"{"table":"member_levels","status":"measured","columns":["updated_at"],"count":2}"#,
            ),
            (
                "tickets",
                r#"{"table":"tickets","status":"measured","columns":["created_at","closing_started_at","closed_at"],"count":1}"#,
            ),
            (
                "guild_settings",
                r#"{"table":"guild_settings","status":"measured","columns":["updated_at"],"count":1}"#,
            ),
            (
                "voice_rooms",
                r#"{"table":"voice_rooms","status":"measured","columns":["created_at","owner_touched_at","name_touched_at","privacy_touched_at"],"count":3}"#,
            ),
            (
                "voice_create_reservations",
                r#"{"table":"voice_create_reservations","status":"measured","columns":["created_at","settled_at"],"count":2}"#,
            ),
            // A missing entry is likewise byte-identical to before.
            (
                "moderation_audit",
                r#"{"table":"moderation_audit","status":"missing","reason":"table not present in this database"}"#,
            ),
        ] {
            let entry = summary
                .tables
                .iter()
                .find(|t| t.table == table)
                .unwrap_or_else(|| panic!("table {table} missing from report"));
            assert_eq!(
                serde_json::to_string(entry)?,
                expected_json,
                "table {table} entry must stay byte-identical"
            );
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    db.finish().await?;
    result
}
