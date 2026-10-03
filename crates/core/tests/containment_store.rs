//! Explicit Postgres acceptance; never uses DATABASE_URL or operational secrets.
//! cargo test -p two-bot-core --features db --test containment_store --locked -- --ignored
#![cfg(feature = "db")]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, QueryBuilder};
use tokio::sync::Barrier;
use two_bot_core::containment::{
    ClaimedContainmentEvent, ContainmentEventState, ContainmentIncidentState, ContainmentPolicy,
    ContainmentReason, ContainmentSignal, DestructiveAction, DestructiveAuditEvent,
};
use two_bot_core::containment_store::{ContainmentStore, EventClaim, IncidentClaim};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const MIGRATION: &str = include_str!("../../cutover/migrations/0370_containment_claims.sql");
const LEGACY_0015: &str =
    include_str!("../../cutover/tests/fixtures/legacy_migrations/0015_anti_nuke_containment.sql");
/// 2026-08-01T10:00:00.000Z.
const NOW: i64 = 1_785_578_400_000;
static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

fn schema_name_at(nanos: u128) -> String {
    format!(
        "containment_test_{}_{}_{}",
        std::process::id(),
        NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

#[test]
fn schema_names_are_distinct_when_the_clock_repeats() {
    assert_ne!(schema_name_at(42), schema_name_at(42));
}

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    options: PgConnectOptions,
    schema: String,
}

impl TestDb {
    async fn new(setup: &[&'static str]) -> Result<Self, Box<dyn std::error::Error>> {
        // Fixed approved services, empty test password. Auth/ownership failures
        // propagate; no alternate credentials or fallback endpoints are tried.
        let host = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let options = PgConnectOptions::new()
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
        let schema = schema_name_at(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
        QueryBuilder::<sqlx::Postgres>::new("CREATE SCHEMA ")
            .push(&schema)
            .build()
            .execute(&admin)
            .await?;
        let options = options.application_name(&schema);
        let pool = Self::connect(&options, &schema).await?;
        for sql in setup {
            sqlx::raw_sql(*sql).execute(&pool).await?;
        }
        Ok(Self {
            admin,
            pool,
            options,
            schema,
        })
    }

    async fn connect(options: &PgConnectOptions, schema: &str) -> Result<PgPool, sqlx::Error> {
        let path = schema.to_owned();
        PgPoolOptions::new()
            .max_connections(12)
            .acquire_timeout(Duration::from_secs(10))
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
            .connect_with(options.clone())
            .await
    }

    async fn peer(&self) -> Result<PgPool, sqlx::Error> {
        Self::connect(&self.options, &self.schema).await
    }

    fn store(&self) -> ContainmentStore {
        ContainmentStore::from_pool(self.pool.clone())
    }

    /// Advisory locks are cluster-wide; per-schema guild IDs keep suites apart.
    fn guild(&self, suffix: &str) -> String {
        format!("{}-{suffix}", self.schema)
    }

    async fn finish(self) -> TestResult {
        self.pool.close().await;
        // Only the generated, run-owned schema is removed.
        QueryBuilder::<sqlx::Postgres>::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        Ok(())
    }
}

fn audit(
    id: &str,
    guild: &str,
    executor: Option<&str>,
    action: DestructiveAction,
    occurred_at_ms: i64,
) -> DestructiveAuditEvent {
    DestructiveAuditEvent {
        audit_entry_id: id.to_owned(),
        guild_id: guild.to_owned(),
        executor_id: executor.map(str::to_owned),
        action,
        target_id: Some(format!("target-{id}")),
        occurred_at_ms: Some(occurred_at_ms),
    }
}

/// Claim and return the trigger with its heat; a duplicate is an error.
async fn claimed(
    store: &ContainmentStore,
    policy: &ContainmentPolicy,
    event: &DestructiveAuditEvent,
    now_ms: i64,
) -> Result<(ClaimedContainmentEvent, u64), Box<dyn std::error::Error>> {
    match store.claim_event(policy, event, now_ms).await? {
        EventClaim::Claimed { event, heat, .. } => Ok((event, heat)),
        EventClaim::Duplicate(recorded) => Err(format!("unexpected duplicate {recorded:?}").into()),
    }
}

async fn event_state(pool: &PgPool, id: &str) -> Result<(String, String), sqlx::Error> {
    sqlx::query_as("SELECT state, reason FROM containment_events WHERE audit_entry_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn audit_id_is_claimed_exactly_once_under_concurrency() -> TestResult {
    let db = TestDb::new(&[MIGRATION]).await?;
    let policy = Arc::new(ContainmentPolicy::default());
    let guild = db.guild("once");
    for executor in [Some("executor"), None] {
        let id = format!("audit-{}", executor.unwrap_or("none"));
        let event = audit(&id, &guild, executor, DestructiveAction::ChannelDelete, NOW);
        let barrier = Arc::new(Barrier::new(10));
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let (store, policy, event, barrier) =
                (db.store(), policy.clone(), event.clone(), barrier.clone());
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                store.claim_event(&policy, &event, NOW).await
            }));
        }
        let mut winners = 0;
        for task in tasks {
            match task.await?? {
                EventClaim::Claimed { heat, .. } => {
                    winners += 1;
                    assert_eq!(heat, if executor.is_some() { 3 } else { 0 });
                }
                EventClaim::Duplicate(recorded) => {
                    assert_eq!(recorded.claimed.event, event);
                }
            }
        }
        assert_eq!(winners, 1, "{id} must be claimed exactly once");
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM containment_events WHERE audit_entry_id = $1")
                .bind(id.as_str())
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(rows, 1);
    }
    db.finish().await
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn guild_executor_event_claims_are_serialized_for_heat() -> TestResult {
    let db = TestDb::new(&[MIGRATION]).await?;
    let policy = Arc::new(ContainmentPolicy::default());
    let guild = db.guild("heat");
    let barrier = Arc::new(Barrier::new(10));
    let mut tasks = Vec::new();
    for n in 0..10 {
        let event = audit(
            &format!("burst-{n:02}"),
            &guild,
            Some("executor"),
            DestructiveAction::MemberKick,
            NOW - 1_000,
        );
        let (store, policy, barrier) = (db.store(), policy.clone(), barrier.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            store.claim_event(&policy, &event, NOW).await
        }));
    }
    let mut heats = BTreeSet::new();
    for task in tasks {
        let EventClaim::Claimed { heat, .. } = task.await?? else {
            return Err("distinct audit IDs must all be claimed".into());
        };
        heats.insert(heat);
    }
    // Each claim sees every earlier same-executor claim exactly once.
    assert_eq!(heats, (1..=10).collect::<BTreeSet<u64>>());

    // Interval bounds: exact-window separation is excluded and ignored, stale
    // or other-executor rows never count.
    let other = db.guild("interval");
    let store = db.store();
    claimed(
        &store,
        &policy,
        &audit(
            "old",
            &other,
            Some("e"),
            DestructiveAction::RoleDelete,
            NOW - 60_000,
        ),
        NOW,
    )
    .await?;
    claimed(
        &store,
        &policy,
        &audit(
            "peer",
            &other,
            Some("f"),
            DestructiveAction::RoleDelete,
            NOW,
        ),
        NOW,
    )
    .await?;
    let (future, heat) = claimed(
        &store,
        &policy,
        &audit(
            "future",
            &other,
            Some("e"),
            DestructiveAction::RoleDelete,
            NOW + 5_001,
        ),
        NOW,
    )
    .await?;
    assert_eq!((future.state, heat), (ContainmentEventState::Ignored, 0));
    let (_, heat) = claimed(
        &store,
        &policy,
        &audit(
            "edge",
            &other,
            Some("e"),
            DestructiveAction::ChannelDelete,
            NOW,
        ),
        NOW,
    )
    .await?;
    assert_eq!(heat, 3, "the row exactly one window earlier is excluded");
    db.finish().await
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn two_incidents_cannot_start_concurrently_for_one_guild_executor() -> TestResult {
    let db = TestDb::new(&[MIGRATION]).await?;
    let policy = Arc::new(ContainmentPolicy::default());
    let store = db.store();
    let guild = db.guild("incident");
    let mut triggers = Vec::new();
    for n in 0..6 {
        let event = audit(
            &format!("nuke-{n}"),
            &guild,
            Some("executor"),
            DestructiveAction::ChannelDelete,
            NOW,
        );
        triggers.push(claimed(&store, &policy, &event, NOW).await?);
    }
    // Same trigger twice plus every other trigger, all at once.
    let mut attempts = triggers.clone();
    attempts.push(triggers[5].clone());
    let barrier = Arc::new(Barrier::new(attempts.len()));
    let mut tasks = Vec::new();
    for (trigger, heat) in attempts {
        let (store, policy, barrier) = (db.store(), policy.clone(), barrier.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .begin_incident(&policy, &trigger, heat.max(5), NOW)
                .await
        }));
    }
    let mut started = Vec::new();
    for task in tasks {
        match task.await?? {
            IncidentClaim::Started(incident) => started.push(incident),
            IncidentClaim::Blocked { state, .. } => {
                assert_eq!(state, ContainmentIncidentState::Containing);
            }
            IncidentClaim::NotEligible => {}
        }
    }
    assert_eq!(started.len(), 1, "exactly one incident starts");
    let incident = &started[0];
    assert_eq!(incident.cooldown_until_ms, NOW + 60_000);
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, state, started_at, cooldown_until FROM containment_incidents WHERE guild_id = $1",
    )
    .bind(&guild)
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![(
            incident.id.clone(),
            "containing".to_owned(),
            "2026-08-01T10:00:00.000Z".to_owned(),
            Some("2026-08-01T10:01:00.000Z".to_owned()),
        )]
    );
    // The trigger is persisted as contain; the other claims remain observe.
    let states: Vec<(String, String)> = sqlx::query_as(
        "SELECT audit_entry_id, state FROM containment_events WHERE guild_id = $1 ORDER BY audit_entry_id",
    )
    .bind(&guild)
    .fetch_all(&db.pool)
    .await?;
    for (id, state) in states {
        let expected = if id == incident.id {
            "contain"
        } else {
            "observe"
        };
        assert_eq!(state, expected, "{id}");
    }

    // A different executor in the same guild is isolated (legacy parity).
    let other = audit(
        "other-admin",
        &guild,
        Some("second"),
        DestructiveAction::ChannelDelete,
        NOW,
    );
    let (trigger, _) = claimed(&store, &policy, &other, NOW).await?;
    assert!(matches!(
        store.begin_incident(&policy, &trigger, 5, NOW).await?,
        IncidentClaim::Started(_)
    ));
    db.finish().await
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn blocker_recheck_inside_the_incident_transaction_refuses() -> TestResult {
    let db = TestDb::new(&[MIGRATION]).await?;
    let policy = ContainmentPolicy::default();
    let store = db.store();
    let guild = db.guild("blocker");
    let mut burst = Vec::new();
    for n in 0..3 {
        let event = audit(
            &format!("b-{n}"),
            &guild,
            Some("executor"),
            DestructiveAction::RoleDelete,
            NOW,
        );
        burst.push(claimed(&store, &policy, &event, NOW).await?);
    }
    let (first, heat) = burst[1].clone();
    assert_eq!(heat, 6);
    let IncidentClaim::Started(incident) = store.begin_incident(&policy, &first, heat, NOW).await?
    else {
        return Err("first incident must start".into());
    };

    // The pure proposal knows no incidents and would alert; the store refuses.
    let (second, heat) = burst[2].clone();
    let evidence: Vec<_> = burst.iter().map(|(event, _)| event.clone()).collect();
    assert!(matches!(
        policy.signal(&second, &evidence, &[], NOW + 1_000),
        ContainmentSignal::Alert(_)
    ));
    assert_eq!(
        store
            .begin_incident(&policy, &second, heat, NOW + 1_000)
            .await?,
        IncidentClaim::Blocked {
            incident_id: incident.id.clone(),
            state: ContainmentIncidentState::Containing,
        }
    );
    // Uncertain blocks indefinitely, well past the cooldown.
    assert!(
        store
            .complete_incident(
                &incident.id,
                ContainmentIncidentState::Uncertain,
                &serde_json::json!({"removed": ["r1"]}),
                NOW + 2_000
            )
            .await?
    );
    assert_eq!(
        store
            .begin_incident(&policy, &second, heat, NOW + 600_000)
            .await?,
        IncidentClaim::Blocked {
            incident_id: incident.id.clone(),
            state: ContainmentIncidentState::Uncertain,
        }
    );
    // Completion happens once; a retry cannot overwrite (release) uncertain.
    assert!(
        !store
            .complete_incident(
                &incident.id,
                ContainmentIncidentState::Contained,
                &serde_json::json!({}),
                NOW + 3_000
            )
            .await?
    );
    let recorded: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state, result_json, completed_at FROM containment_incidents WHERE id = $1",
    )
    .bind(&incident.id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        recorded,
        (
            "uncertain".to_owned(),
            Some(r#"{"removed":["r1"]}"#.to_owned()),
            Some("2026-08-01T10:00:02.000Z".to_owned()),
        )
    );
    assert!(store
        .complete_incident(
            &incident.id,
            ContainmentIncidentState::Containing,
            &serde_json::json!({}),
            NOW
        )
        .await
        .is_err());

    // A non-uncertain incident releases strictly after its cooldown.
    let guild = db.guild("cooldown");
    let a = audit(
        "c-a",
        &guild,
        Some("executor"),
        DestructiveAction::ChannelDelete,
        NOW,
    );
    let (a, _) = claimed(&store, &policy, &a, NOW).await?;
    let IncidentClaim::Started(done) = store.begin_incident(&policy, &a, 5, NOW).await? else {
        return Err("cooldown incident must start".into());
    };
    assert!(
        store
            .complete_incident(
                &done.id,
                ContainmentIncidentState::Contained,
                &serde_json::json!({}),
                NOW
            )
            .await?
    );
    let b = audit(
        "c-b",
        &guild,
        Some("executor"),
        DestructiveAction::ChannelDelete,
        NOW + 60_000,
    );
    let (b, _) = claimed(&store, &policy, &b, NOW + 60_000).await?;
    assert!(matches!(
        store.begin_incident(&policy, &b, 5, NOW + 59_999).await?,
        IncidentClaim::Blocked { .. }
    ));
    assert!(matches!(
        store.begin_incident(&policy, &b, 5, NOW + 60_000).await?,
        IncidentClaim::Started(_)
    ));

    // Ineligible triggers never start: below threshold, unrecorded, ignored,
    // already contained, or mismatched executor.
    let below = audit(
        "low",
        &guild,
        Some("third"),
        DestructiveAction::MemberKick,
        NOW,
    );
    let (below, heat) = claimed(&store, &policy, &below, NOW).await?;
    assert_eq!(
        store.begin_incident(&policy, &below, heat, NOW).await?,
        IncidentClaim::NotEligible
    );
    let unrecorded = ClaimedContainmentEvent {
        event: audit(
            "never-claimed",
            &guild,
            Some("third"),
            DestructiveAction::RoleDelete,
            NOW,
        ),
        state: ContainmentEventState::Observe,
    };
    assert_eq!(
        store.begin_incident(&policy, &unrecorded, 9, NOW).await?,
        IncidentClaim::NotEligible
    );
    let mut spoofed = below.clone();
    spoofed.event.executor_id = Some("fourth".to_owned());
    assert_eq!(
        store.begin_incident(&policy, &spoofed, 9, NOW).await?,
        IncidentClaim::NotEligible
    );
    assert_eq!(
        store.begin_incident(&policy, &a, 9, NOW + 120_000).await?,
        IncidentClaim::NotEligible
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn dispositions_survive_a_reconnect() -> TestResult {
    let db = TestDb::new(&[MIGRATION]).await?;
    let mut policy = ContainmentPolicy::default();
    policy.trusted_user_ids.insert("trusted".to_owned());
    let guild = db.guild("restart");
    let cases = [
        (
            audit(
                "future",
                &guild,
                Some("executor"),
                DestructiveAction::RoleDelete,
                NOW + 10_000,
            ),
            ContainmentEventState::Ignored,
            ContainmentReason::Future,
        ),
        (
            audit(
                "stale",
                &guild,
                Some("executor"),
                DestructiveAction::RoleDelete,
                NOW - 120_001,
            ),
            ContainmentEventState::Stale,
            ContainmentReason::Stale,
        ),
        (
            audit("orphan", &guild, None, DestructiveAction::RoleDelete, NOW),
            ContainmentEventState::Ignored,
            ContainmentReason::MissingExecutor,
        ),
        (
            audit(
                "trusted",
                &guild,
                Some("trusted"),
                DestructiveAction::RoleDelete,
                NOW,
            ),
            ContainmentEventState::Ignored,
            ContainmentReason::TrustedExecutor,
        ),
        (
            audit(
                "counted",
                &guild,
                Some("executor"),
                DestructiveAction::RoleDelete,
                NOW,
            ),
            ContainmentEventState::Observe,
            ContainmentReason::Counted,
        ),
    ];
    {
        let store = db.store();
        for (event, state, reason) in &cases {
            let EventClaim::Claimed { disposition, .. } =
                store.claim_event(&policy, event, NOW).await?
            else {
                return Err("first claim must win".into());
            };
            assert_eq!((disposition.state, disposition.reason), (*state, *reason));
        }
    }
    db.pool.close().await;

    // New pool, later clock, no trust list: every recorded disposition stands.
    let fresh = ContainmentStore::from_pool(db.peer().await?);
    let later = ContainmentPolicy::default();
    for (event, state, reason) in &cases {
        let recorded = fresh
            .recorded_event(&event.audit_entry_id)
            .await?
            .ok_or("recorded event must survive")?;
        assert_eq!(recorded.claimed.event, *event);
        assert_eq!(recorded.claimed.state, *state);
        assert_eq!(recorded.reason, reason.description());
        assert_eq!(recorded.created_at_ms, NOW);
        let EventClaim::Duplicate(again) = fresh.claim_event(&later, event, NOW + 15_000).await?
        else {
            return Err("a redelivered audit ID must not be reclaimed".into());
        };
        assert_eq!(again, recorded);
    }
    // The recorded future entry still never becomes heat once its time arrives.
    let next = audit(
        "next",
        &guild,
        Some("executor"),
        DestructiveAction::RoleDelete,
        NOW + 10_000,
    );
    let (_, heat) = claimed(&fresh, &later, &next, NOW + 15_000).await?;
    assert_eq!(heat, 6, "counted + next only");
    fresh.pool().close().await;

    // Invalid occurrence times are refused before anything is persisted.
    let reopened = ContainmentStore::from_pool(db.peer().await?);
    let mut invalid = audit(
        "invalid",
        &guild,
        Some("executor"),
        DestructiveAction::RoleDelete,
        NOW,
    );
    invalid.occurred_at_ms = None;
    assert!(reopened.claim_event(&later, &invalid, NOW).await.is_err());
    invalid.occurred_at_ms = Some(253_402_300_800_000);
    assert!(reopened.claim_event(&later, &invalid, NOW).await.is_err());
    assert!(reopened.recorded_event("invalid").await?.is_none());
    reopened.pool().close().await;
    db.finish().await
}

const LEGACY_ROWS: &str = r#"
INSERT INTO containment_events
  (audit_entry_id, guild_id, executor_id, action, target_id, weight, occurred_at, state, reason, created_at)
VALUES
  ('legacy-1', 'legacy-guild', 'legacy-exec', 'channel.delete', 'c1', 3, '2026-08-01T09:59:30.000Z', 'contain', 'counted toward destructive-action heat', '2026-08-01T09:59:30.120Z'),
  ('legacy-2', 'legacy-guild', 'legacy-exec', 'member.ban', NULL, 1, '2026-08-01T09:59:40.000Z', 'observe', 'counted toward destructive-action heat', '2026-08-01T09:59:40.004Z'),
  ('legacy-3', 'legacy-guild', NULL, 'webhook.create', 'w1', 1, '2026-08-01T09:59:41.000Z', 'ignored', 'audit entry has no executor; refusing to guess', '2026-08-01T09:59:41.000Z'),
  ('legacy-4', 'legacy-guild', 'legacy-exec', 'role.delete', 'r1', 3, '2026-08-01T09:50:00.000Z', 'stale', 'audit entry is too old to trigger a fresh incident', '2026-08-01T09:59:42.000Z');
INSERT INTO containment_incidents
  (id, guild_id, executor_id, trigger_audit_entry_id, heat, state, result_json, started_at, cooldown_until, completed_at)
VALUES
  ('legacy-1', 'legacy-guild', 'legacy-exec', 'legacy-1', 7, 'uncertain', '{"removedRoleIds":["r9"],"failure":"timeout | 雪"}', '2026-08-01T09:59:30.200Z', '2026-08-01T10:00:30.200Z', '2026-08-01T09:59:31.000Z');
"#;

async fn snapshot(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let mut rows: Vec<String> = sqlx::query_scalar(
        "SELECT 'e:' || row_to_json(e)::text FROM (SELECT * FROM containment_events ORDER BY audit_entry_id) e \
         UNION ALL \
         SELECT 'i:' || row_to_json(i)::text FROM (SELECT * FROM containment_incidents ORDER BY id) i",
    )
    .fetch_all(pool)
    .await?;
    rows.sort();
    Ok(rows)
}

async fn shape(pool: &PgPool, schema: &str) -> Result<Vec<String>, sqlx::Error> {
    let mut shape: Vec<String> = sqlx::query_scalar(
        "SELECT table_name || '.' || column_name || ':' || data_type || ':' || is_nullable || ':' || ordinal_position \
         FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name LIKE 'containment_%' \
         ORDER BY table_name, ordinal_position",
    )
    .fetch_all(pool)
    .await?;
    shape.extend(
        sqlx::query_scalar::<_, String>(
            "SELECT c.conname || ':' || pg_get_constraintdef(c.oid) FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = current_schema() AND t.relname LIKE 'containment_%' ORDER BY c.conname",
        )
        .fetch_all(pool)
        .await?,
    );
    shape.extend(
        sqlx::query_scalar::<_, String>(
            "SELECT indexdef FROM pg_indexes \
             WHERE schemaname = current_schema() AND tablename LIKE 'containment_%' ORDER BY indexname",
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|definition| definition.replace(schema, "<schema>")),
    );
    Ok(shape)
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn upgrading_from_legacy_0015_loses_nothing() -> TestResult {
    let legacy = TestDb::new(&[LEGACY_0015, LEGACY_ROWS]).await?;
    let before = snapshot(&legacy.pool).await?;
    assert_eq!(before.len(), 5);
    let legacy_shape = shape(&legacy.pool, &legacy.schema).await?;
    sqlx::raw_sql(MIGRATION).execute(&legacy.pool).await?;
    assert_eq!(
        snapshot(&legacy.pool).await?,
        before,
        "0370 must not alter legacy rows"
    );
    assert_eq!(shape(&legacy.pool, &legacy.schema).await?, legacy_shape);

    let fresh = TestDb::new(&[MIGRATION]).await?;
    assert_eq!(
        shape(&fresh.pool, &fresh.schema).await?,
        legacy_shape,
        "a fresh 0370 schema matches the legacy shape"
    );
    fresh.finish().await?;

    // The store reads legacy rows: dispositions stand, contain/observe rows
    // count toward heat, and the legacy uncertain incident still blocks.
    let store = legacy.store();
    let policy = ContainmentPolicy::default();
    let EventClaim::Duplicate(recorded) = store
        .claim_event(
            &policy,
            &audit(
                "legacy-3",
                "legacy-guild",
                None,
                DestructiveAction::WebhookCreate,
                NOW,
            ),
            NOW,
        )
        .await?
    else {
        return Err("legacy audit IDs stay claimed".into());
    };
    assert_eq!(recorded.claimed.state, ContainmentEventState::Ignored);
    assert_eq!(
        recorded.reason,
        "audit entry has no executor; refusing to guess"
    );
    let event = audit(
        "rust-1",
        "legacy-guild",
        Some("legacy-exec"),
        DestructiveAction::RoleDelete,
        NOW,
    );
    let (trigger, heat) = claimed(&store, &policy, &event, NOW).await?;
    assert_eq!(
        heat, 7,
        "legacy contain(3) + observe(1) + new(3); stale excluded"
    );
    assert_eq!(
        store
            .begin_incident(&policy, &trigger, heat, NOW + 600_000)
            .await?,
        IncidentClaim::Blocked {
            incident_id: "legacy-1".to_owned(),
            state: ContainmentIncidentState::Uncertain,
        }
    );
    assert_eq!(event_state(&legacy.pool, "rust-1").await?.0, "observe");
    let mut after = snapshot(&legacy.pool).await?;
    after.retain(|row| !row.contains("\"rust-1\""));
    assert_eq!(after, before, "store activity leaves legacy rows untouched");
    legacy.finish().await
}
