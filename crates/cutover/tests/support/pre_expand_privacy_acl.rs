//! Disposable privacy checks across the 0480 schema and grant transition.

use sqlx::{
    migrate::Migrator, postgres::PgPoolOptions, Acquire, Connection, PgConnection, PgPool,
    Postgres, Transaction,
};
use two_bot_core::backup::dump::{dump, restore};
use two_bot_core::database_roles::{plan_for_phase, verification_sql, Phase};
use two_bot_cutover::legacy_verify::read_only_transaction;
use two_bot_cutover::member_erasure::{erase_member, ErasureMode};
use two_bot_cutover::retention::{
    fulfill_user_deletion, handle_guild_delete, request_user_deletion, run_retention,
    RetentionMode, RetentionReport,
};
use two_bot_cutover::rollback_candidate::{
    roles_sha256, verify, ArtifactEvidence, CandidatePin, DatabaseEvidence, PrivacyAccess, Refusal,
};
use two_bot_cutover::staging_migrate::{manifest_hash, Chain, LedgerRow};
use two_bot_cutover::{member_erasure, staging_migrate::MIGRATOR as CUTOVER};
use two_bot_testsupport::TestDatabase;

fn through(source: &Migrator, version: i64) -> Migrator {
    Migrator::with_migrations(
        source
            .iter()
            .filter(|migration| migration.version <= version)
            .cloned()
            .collect(),
    )
}

#[tokio::test]
async fn pre_expand_runtime_cannot_delete_reservations_or_write_operator_audit() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    let db = TestDatabase::create(&url, &through(&CUTOVER, 473))
        .await
        .expect("create historical cutover fixture");
    let pool = db.pool().clone();
    let result = tokio::spawn(async move { probe(&pool).await }).await;
    db.close().await.expect("verify disposable teardown");
    match result {
        Ok(result) => result.unwrap(),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => panic!("ACL probe task failed to join: {error}"),
    }
}

async fn legacy_store(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    let mut store = through(&two_bot_store::MIGRATOR, 465);
    store.dangerous_set_table_name(two_bot_store::TABLE_NAME);
    store.run(pool).await
}

async fn install_legacy_contract(tx: &mut PgConnection) -> Result<(), sqlx::Error> {
    let contract = include_str!("../../../../sql/web_v1.sql")
        .split_once("-- Voice heat contract 1.1:")
        .expect("pre-heat contract prefix")
        .0;
    sqlx::raw_sql(sqlx::AssertSqlSafe(contract))
        .execute(&mut *tx)
        .await?;
    let historical: bool = sqlx::query_scalar(
        "SELECT (SELECT count(*) = 115 AND max(version) = 473 AND bool_and(success)
                   FROM public._sqlx_migrations)
            AND (SELECT count(*) = 20 AND max(version) = 465 AND bool_and(success)
                   FROM public._two_bot_migrations)
            AND (SELECT count(*) = 1 AND min(contract_version) = '1.0'
                   FROM public.web_contract_meta)
            AND to_regclass('public.voice_heat_hours') IS NULL
            AND to_regclass('public.event_attendance_windows') IS NULL
            AND to_regclass('public.event_voice_sessions') IS NULL
            AND to_regclass('public.event_voice_sessions_id_seq') IS NULL
            AND to_regclass('web_v1.voice_heat') IS NULL",
    )
    .fetch_one(&mut *tx)
    .await?;
    assert!(
        historical,
        "fixture must start before either expansion chain"
    );

    Ok(())
}

async fn install_roles(tx: &mut PgConnection, name: &str, phase: Phase) -> Result<(), sqlx::Error> {
    let plan = plan_for_phase(phase);
    let body = plan
        .split_once("\nBEGIN;\n")
        .and_then(|(_, body)| body.strip_suffix("\nCOMMIT;\n"))
        .expect("role plan transaction wrapper");
    sqlx::raw_sql(sqlx::AssertSqlSafe(scope_roles(body, name)))
        .execute(&mut *tx)
        .await?;
    Ok(())
}

fn scope_roles(sql: &str, name: &str) -> String {
    validate_name(name);
    sql.replace("two_bot_migrator_ro", &format!("{name}_r"))
        .replace("two_bot_migrator", &format!("{name}_m"))
        .replace("two_web_reader", &format!("{name}_w"))
        .replace("two_bot_runtime", &format!("{name}_b"))
}

async fn probe(pool: &PgPool) -> Result<(), sqlx::Error> {
    legacy_store(pool).await?;
    let mut tx = pool.begin().await?;
    install_legacy_contract(&mut tx).await?;
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *tx)
        .await?;
    install_roles(&mut tx, &name, Phase::Bootstrap).await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {name}_b")))
        .execute(&mut *tx)
        .await?;

    // Positive control: the runtime can create and read the exact row the
    // member and guild paths must erase. No probe runs as the DB owner.
    sqlx::query(
        "INSERT INTO public.voice_create_reservations (guild_id, user_id, created_at)
         VALUES ('1', '2', now())",
    )
    .execute(&mut *tx)
    .await?;
    assert_eq!(
        member_erasure::plan()
            .tables
            .iter()
            .find(|entry| entry.table == "voice_create_reservations")
            .expect("reservations are part of the reviewed erasure footprint")
            .predicate,
        "guild_id = $1 AND user_id = $2"
    );
    for statement in [
        "DELETE FROM public.voice_create_reservations WHERE guild_id = '1' AND user_id = '2'",
        "DELETE FROM public.voice_create_reservations WHERE guild_id = '1'",
        "LOCK TABLE public.member_erasure_audit IN SHARE ROW EXCLUSIVE MODE",
        "INSERT INTO public.member_erasure_audit (actor, erased_at) VALUES ('fixture', now())",
    ] {
        require_denied(&mut tx, statement).await?;
    }
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public.voice_create_reservations WHERE guild_id = '1' AND user_id = '2'",
    )
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(remaining, 1);
    tx.rollback().await
}

async fn require_denied(
    tx: &mut Transaction<'_, Postgres>,
    statement: &'static str,
) -> Result<(), sqlx::Error> {
    let mut savepoint = tx.begin().await?;
    let result = sqlx::raw_sql(statement).execute(&mut *savepoint).await;
    savepoint.rollback().await?;
    assert_sqlstate(&result.expect_err(statement), "42501");
    Ok(())
}

fn assert_sqlstate(error: &sqlx::Error, expected: &str) {
    assert_eq!(
        error
            .as_database_error()
            .and_then(|db| db.code())
            .as_deref(),
        Some(expected),
        "{error}"
    );
}

async fn count<'e>(db: impl sqlx::PgExecutor<'e>, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db).await.unwrap()
}

const GUILD: &str = "100000000000000001";
const OTHER_GUILD: &str = "100000000000000002";
const MEMBER: &str = "200000000000000001";
const RESERVATIONS: &str = "SELECT count(*) FROM voice_create_reservations";
const MEMBERS: &str = "SELECT count(*) FROM members";
const QUEUE: &str = "SELECT count(*) FROM retention_guild_purge_queue";
const SESSIONS: &str = "SELECT count(*) FROM event_voice_sessions";
const WINDOWS: &str = "SELECT count(*) FROM event_attendance_windows";
const AUDITS: &str = "SELECT count(*) FROM member_erasure_audit";
const CAPTURE: &str = "SELECT count(*) FROM event_store WHERE dedup_key = 'fixture'";
const HISTORY: &str = "SELECT count(*) FROM public.backfill_history_messages";
const WRITE_HISTORY: &str =
    "INSERT INTO public.backfill_history_messages VALUES ('10','20','30','2026-01-01T00:00:00Z')";
const RECEIPTS: &str =
    "SELECT count(*) FROM retention_deletion_requests WHERE receipt_id IS NOT NULL";

#[tokio::test]
async fn runtime_privacy_boundaries_across_0480_and_v17_restore() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    let db = TestDatabase::create(&url, &through(&CUTOVER, 473))
        .await
        .unwrap();
    let name = db.name().to_owned();
    let options = db.pool().connect_options();
    let pool = db.pool().clone();
    let fixture_name = name.clone();
    let result = tokio::spawn(async move { exercise_transition(&pool, &fixture_name).await }).await;
    db.close().await.expect("verify disposable teardown");
    let bootstrap = url.rsplit_once('/').unwrap().1;
    let mut cleanup = PgConnection::connect_with(&options.as_ref().clone().database(bootstrap))
        .await
        .unwrap();
    validate_name(&name);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP ROLE IF EXISTS {name}_b, {name}_m, {name}_w, {name}_r"
    )))
    .execute(&mut cleanup)
    .await
    .unwrap();
    result.unwrap();
}

fn validate_name(name: &str) {
    assert!(name.starts_with("two_bot_test_") && name.len() <= 61);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
}

async fn role_pool(pool: &PgPool, role: String) -> PgPool {
    let setup = format!("SET ROLE {role}; SET search_path = public");
    PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _| {
            let setup = setup.clone();
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(setup))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap()
}

async fn exercise_transition(pool: &PgPool, name: &str) {
    legacy_store(pool).await.unwrap();
    let mut setup = pool.begin().await.unwrap();
    install_legacy_contract(&mut setup).await.unwrap();
    install_roles(&mut setup, name, Phase::Bootstrap)
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let runtime = role_pool(pool, format!("{name}_b")).await;
    let migrator = role_pool(pool, format!("{name}_m")).await;
    let audit = role_pool(pool, format!("{name}_r")).await;
    sqlx::raw_sql(
        "INSERT INTO members (guild_id, member_id) VALUES
             ('100000000000000001','200000000000000001'),
             ('100000000000000001','200000000000000002'),
             ('100000000000000002','200000000000000001');
         INSERT INTO voice_create_reservations (guild_id, user_id, created_at)
             SELECT guild_id, member_id, clock_timestamp() FROM members;
         INSERT INTO event_instance_windows
             (guild_id, event_id, instance_starts_at, starts_at, status, observed_at)
         VALUES ('100000000000000001', '300000000000000001',
                 '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'active', '2026-01-01T00:00:00.000Z');
         WITH intent AS (
             INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state)
             VALUES (repeat('a', 64), repeat('b', 64), 'event.upsert', repeat('c', 64), 'in_flight')
             RETURNING intent_id, caller_hash, action
         )
         INSERT INTO internal_action_log (intent_id, caller_hash, action, phase)
             SELECT intent_id, caller_hash, action, 'intent' FROM intent;",
    )
    .execute(&runtime)
    .await
    .unwrap();
    assert!(handle_guild_delete(&runtime, OTHER_GUILD, None)
        .await
        .unwrap());
    let request = request_user_deletion(&runtime, GUILD, MEMBER)
        .await
        .unwrap();
    let legacy_ledger = candidate_readout(&audit, name).await;
    let legacy_objects = rollback_catalog(&migrator).await;
    let legacy_rows = rollback_rows(&runtime).await;
    assert_runtime_refusal(&runtime, &request, "42P01").await;
    let result = run_retention(&runtime, RetentionMode::Execute).await;
    let err = result.unwrap_err();
    assert_eq!((err.step().as_str(), err.cause()), ("expiry", "database"));
    assert_preserved(&runtime).await;
    // Cutover commits before Store, web contract and grants.
    through(&CUTOVER, 480).run(&migrator).await.unwrap();
    assert_ledgers(&migrator, 480, 465).await;
    assert_counts(&migrator, &[(SESSIONS, 0), (WINDOWS, 0)]).await;
    for query in [SESSIONS, WINDOWS] {
        let error = sqlx::query(query).execute(&runtime).await.unwrap_err();
        assert_sqlstate(&error, "42501");
    }
    assert_runtime_refusal(&runtime, &request, "42501").await;
    let result = run_retention(&runtime, RetentionMode::Execute).await;
    let err = result.unwrap_err();
    assert_eq!((err.step().as_str(), err.cause()), ("expiry", "database"));
    assert_preserved(&runtime).await;
    CUTOVER.run(&migrator).await.unwrap();
    two_bot_store::migrate(&migrator).await.unwrap();
    two_bot_core::apply_web_contract(&migrator).await.unwrap();
    let before_grants = candidate_readout(&audit, name).await;
    let mut grants = pool.begin().await.unwrap();
    install_roles(&mut grants, name, Phase::Full).await.unwrap();
    grants.commit().await.unwrap();
    let applied = candidate_readout(&audit, name).await;
    assert!(applied.roles_match);
    let fence = recovery_fence(&runtime).await;
    let migration_fence: String = sqlx::query_scalar(
        "SELECT encode(sha256(timestamptz_send(installed_on)), 'hex')
         FROM public._sqlx_migrations WHERE version = 475 AND success",
    )
    .fetch_one(&audit)
    .await
    .unwrap();
    assert_eq!(fence, migration_fence);
    assert_eq!(rollback_rows(&runtime).await, legacy_rows);
    assert_undo_refused(&runtime, "SELECT 1", "invalid rehearsal binding").await;
    exercise_schema_undo(&migrator).await;
    let restored = candidate_readout(&audit, name).await;
    let boundary = applied
        .cutover
        .iter()
        .find(|row| row.version == 475)
        .unwrap();
    let mut expected = legacy_ledger.cutover;
    expected.push(boundary.clone());
    assert_eq!(restored.cutover, expected);
    assert_eq!(restored.store, legacy_ledger.store);
    assert_eq!(rollback_catalog(&migrator).await, legacy_objects);
    assert_eq!(rollback_rows(&runtime).await, legacy_rows);
    assert_eq!(recovery_fence(&runtime).await, fence);
    CUTOVER.run(&migrator).await.unwrap();
    two_bot_store::migrate(&migrator).await.unwrap();
    two_bot_core::apply_web_contract(&migrator).await.unwrap();
    let mut grants = pool.begin().await.unwrap();
    install_roles(&mut grants, name, Phase::Full).await.unwrap();
    grants.commit().await.unwrap();
    assert_eq!(recovery_fence(&runtime).await, fence);
    // Sequence advances survive rollback. Exercise this condition after the
    // positive undo, before the existing fixture inserts attendance rows.
    assert_undo_refused(
        &migrator,
        "SELECT nextval('public.event_voice_sessions_id_seq')",
        "new schema contains evidence",
    )
    .await;
    sqlx::raw_sql(
        "INSERT INTO event_attendance_windows
             (guild_id, event_key, event_id, instance_starts_at, starts_at, ends_at)
         SELECT DISTINCT guild_id, 'fixture', guild_id, now(), now(), now() + interval '1 hour'
         FROM members;
         INSERT INTO event_voice_sessions (guild_id, member_id, channel_id, joined_at, event_keys)
         SELECT guild_id, member_id, guild_id, now(), ARRAY['fixture'] FROM members;",
    )
    .execute(&runtime)
    .await
    .unwrap();
    restore_v17(&migrator, name).await;
    assert_eq!(count(&migrator, HISTORY).await, 0);
    assert!(!erase_member(&migrator, GUILD, MEMBER, ErasureMode::DryRun)
        .await
        .unwrap()
        .is_empty());
    let preview = run_retention(&migrator, RetentionMode::DryRun)
        .await
        .unwrap();
    assert_eq!(preview.deletions_completed, 1);
    assert!(preview.guild_rows_purged > 0);
    assert_preserved(&migrator).await;
    assert_eq!(count(&migrator, AUDITS).await, 0);
    assert_unverifiable_history(pool, &migrator, name, &request).await;
    verify_rollback_candidate(&audit, &migrator, name, &before_grants).await;
    restore_v17(&migrator, name).await;
    assert_counts(&migrator, &[(HISTORY, 1), (AUDITS, 0)]).await;
    assert_history_defers(&migrator, &request).await;
    for mode in [
        ErasureMode::DryRun,
        ErasureMode::Execute { actor: "fixture" },
    ] {
        assert_coverage_refusal(
            erase_member(&migrator, GUILD, MEMBER, mode)
                .await
                .unwrap_err(),
        );
    }
    sqlx::query("DELETE FROM public.backfill_history_messages")
        .execute(&migrator)
        .await
        .unwrap();
    assert_counts(&runtime, &[(SESSIONS, 3), (WINDOWS, 2)]).await;
    assert_eq!(count(&migrator, AUDITS).await, 0);
    assert_hidden_schema_and_replay_refusals(&runtime, &migrator, &request).await;
    assert_runtime_refusal(&runtime, &request, "42501").await;
    for mode in [RetentionMode::DryRun, RetentionMode::Execute] {
        let report = run_retention(&runtime, mode).await.unwrap();
        assert_eq!(report.schema_gap_deferrals, 1);
        assert_eq!(report.guild_purges_failed, 1);
        let expiry = (report.presence_raw_deleted, count(&runtime, CAPTURE).await);
        assert_eq!(expiry, (1, i64::from(mode == RetentionMode::DryRun)));
    }
    assert_preserved(&runtime).await;
    sqlx::raw_sql(include_str!("../fixtures/history_lock.sql"))
        .execute(&migrator)
        .await
        .unwrap();
    let receipt = fulfill_user_deletion(&migrator, &request).await.unwrap();
    assert_eq!(receipt.receipt_id, format!("rcpt-{request}"));
    assert!(fulfill_user_deletion(&migrator, &request).await.is_err());
    assert_counts(&runtime, &[(RECEIPTS, 1), (SESSIONS, 2), (RESERVATIONS, 2)]).await;
    assert_eq!(count(&runtime, "SELECT count(*) FROM members WHERE guild_id = '100000000000000002' AND member_id = '200000000000000001'").await, 1);
    request_user_deletion(&runtime, GUILD, "200000000000000003")
        .await
        .unwrap();
    let drained = run_retention(&migrator, RetentionMode::Execute)
        .await
        .unwrap();
    assert_eq!(drained.deletions_completed, 1);
    assert!(drained.guild_rows_purged > 0);
    let survivors: Vec<(String, String)> =
        sqlx::query_as("SELECT guild_id, member_id FROM event_voice_sessions")
            .fetch_all(&runtime)
            .await
            .unwrap();
    assert_eq!(
        survivors,
        [(GUILD.to_owned(), "200000000000000002".to_owned())]
    );
    assert_counts(&runtime, &[(WINDOWS, 1), (RESERVATIONS, 1), (QUEUE, 0)]).await;
    assert_eq!(count(&runtime, MEMBERS).await, 1);
    assert_eq!(count(&runtime, "SELECT count(*) FROM moderation_idempotency WHERE state = 'done' AND outcome = 'operator_released'").await, 2);
    let repeat = run_retention(&migrator, RetentionMode::Execute)
        .await
        .unwrap();
    assert_eq!(
        (repeat.deletions_completed, repeat.guild_rows_purged),
        (0, 0)
    );
    assert_eq!(count(&runtime, RECEIPTS).await, 2);
    erase_member(
        &migrator,
        GUILD,
        "200000000000000002",
        ErasureMode::Execute { actor: "fixture" },
    )
    .await
    .unwrap();
    assert_counts(&migrator, &[(MEMBERS, 0), (AUDITS, 1), (HISTORY, 0)]).await;
    assert!(candidate_readout(&audit, name).await.roles_match);
}

fn assert_drain_defers(report: &RetentionReport) {
    assert_eq!(
        (report.schema_gap_deferrals, report.guild_purges_failed),
        (1, 1)
    );
    assert_eq!(
        (report.deletions_completed, report.guild_rows_purged),
        (0, 0)
    );
}

fn assert_coverage_refusal(error: sqlx::Error) {
    assert_eq!(error.to_string(), "erasure schema coverage incomplete");
}

async fn assert_history_defers(operator: &PgPool, request: &str) {
    assert_coverage_refusal(fulfill_user_deletion(operator, request).await.unwrap_err());
    for mode in [RetentionMode::DryRun, RetentionMode::Execute] {
        assert_drain_defers(&run_retention(operator, mode).await.unwrap());
    }
    assert_preserved(operator).await;
}

async fn assert_unverifiable_history(admin: &PgPool, operator: &PgPool, name: &str, request: &str) {
    validate_name(name);
    for (change, undo) in [
        (
            "ALTER TABLE public.backfill_history_messages ENABLE ROW LEVEL SECURITY".to_owned(),
            "ALTER TABLE public.backfill_history_messages DISABLE ROW LEVEL SECURITY".to_owned(),
        ),
        (
            "ALTER TABLE public.backfill_history_messages ALTER COLUMN author_id DROP NOT NULL"
                .to_owned(),
            "ALTER TABLE public.backfill_history_messages ALTER COLUMN author_id SET NOT NULL"
                .to_owned(),
        ),
        (
            "ALTER TABLE public.backfill_history_messages RENAME TO history_fixture".to_owned(),
            "ALTER TABLE public.history_fixture RENAME TO backfill_history_messages".to_owned(),
        ),
        (
            format!("REVOKE SELECT ON public.backfill_history_messages FROM {name}_m"),
            format!("GRANT SELECT ON public.backfill_history_messages TO {name}_m"),
        ),
        (
            format!("ALTER TABLE public.backfill_history_messages OWNER TO {name}_w"),
            format!("ALTER TABLE public.backfill_history_messages OWNER TO {name}_m"),
        ),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(change))
            .execute(admin)
            .await
            .unwrap();
        assert_history_defers(operator, request).await;
        sqlx::query(sqlx::AssertSqlSafe(undo))
            .execute(admin)
            .await
            .unwrap();
    }
    sqlx::query("SET default_transaction_isolation = 'repeatable read'")
        .execute(operator)
        .await
        .unwrap();
    assert_coverage_refusal(fulfill_user_deletion(operator, request).await.unwrap_err());
    let report = run_retention(operator, RetentionMode::Execute)
        .await
        .unwrap();
    assert_drain_defers(&report);
    assert_preserved(operator).await;
    sqlx::query("RESET default_transaction_isolation")
        .execute(operator)
        .await
        .unwrap();

    let mut writer = admin.begin().await.unwrap();
    sqlx::query(WRITE_HISTORY)
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO event_store (event_type, guild_id, user_id, event_time, dedup_key)
         VALUES ('PRESENCE_UPDATE', $1, $2, now() - interval '40 days', 'fixture')",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .execute(operator)
    .await
    .unwrap();
    let report = run_retention(operator, RetentionMode::Execute)
        .await
        .unwrap();
    assert_drain_defers(&report);
    assert_eq!(
        (report.presence_raw_deleted, count(operator, CAPTURE).await),
        (1, 0)
    );
    assert_preserved(operator).await;

    // The next run has scanned coverage and is waiting for SHARE while the
    // writer is still invisible. Commit before SHARE is granted.
    let drain = operator.clone();
    let pending = tokio::spawn(async move { run_retention(&drain, RetentionMode::Execute).await });
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while count(
            admin,
            "SELECT count(*) FROM pg_catalog.pg_locks
            WHERE relation = 'public.backfill_history_messages'::regclass
              AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
              AND mode = 'ShareLock' AND NOT granted",
        )
        .await
            == 0
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("privacy drain did not wait on the synthetic history writer");
    writer.commit().await.unwrap();
    assert_drain_defers(&pending.await.unwrap().unwrap());
    assert_counts(operator, &[(HISTORY, 1), (AUDITS, 0)]).await;
    assert_preserved(operator).await;
    sqlx::query("DELETE FROM public.backfill_history_messages")
        .execute(operator)
        .await
        .unwrap();
}

async fn rollback_catalog(pool: &PgPool) -> String {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'relations', (SELECT jsonb_agg(jsonb_build_array(n.nspname, c.relname, c.relkind,
                                                          c.relowner, c.relacl)
                                         ORDER BY n.nspname, c.relname)
                        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                        WHERE n.nspname IN ('public', 'web_v1')),
          'schemas', (SELECT jsonb_agg(jsonb_build_array(nspname, nspowner, nspacl) ORDER BY nspname)
                      FROM pg_namespace WHERE nspname IN ('public', 'web_v1')),
          'audit_constraints', (SELECT jsonb_agg(jsonb_build_array(conname, pg_get_constraintdef(oid), convalidated)
                                                 ORDER BY conname)
                                FROM pg_constraint WHERE conrelid = 'public.internal_action_log'::regclass),
          'audit_columns', (SELECT jsonb_agg(jsonb_build_array(attname, format_type(atttypid, atttypmod), attnotnull)
                                             ORDER BY attnum)
                            FROM pg_attribute WHERE attrelid = 'public.internal_action_log'::regclass
                              AND attnum > 0 AND NOT attisdropped),
          'web_version', (SELECT contract_version FROM public.web_contract_meta WHERE singleton)
        )::text",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn rollback_rows(pool: &PgPool) -> (String, String) {
    sqlx::query_as(
        "SELECT
           (SELECT (to_jsonb(w) - 'attendance_recovered_at')::text
            FROM public.event_instance_windows w WHERE w.event_id = '300000000000000001'),
           (SELECT (to_jsonb(a) - ARRAY['delivery_hash', 'from_channel_id', 'request_id'])::text
            FROM public.internal_action_log a WHERE a.caller_hash = repeat('a', 64))",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn recovery_fence(pool: &PgPool) -> String {
    sqlx::query_scalar(
        "SELECT encode(sha256(timestamptz_send(attendance_recovered_at)), 'hex')
         FROM public.event_instance_windows WHERE event_id = '300000000000000001'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn exercise_schema_undo(migrator: &PgPool) {
    let catalog = rollback_catalog(migrator).await;
    for seed in [
        "INSERT INTO voice_heat_hours (hour_start_utc) VALUES ('2026-01-01T00:00:00Z')",
        "INSERT INTO event_attendance_windows
           (guild_id, event_key, event_id, instance_starts_at, starts_at, ends_at)
         VALUES ('1', 'fixture', '2', now(), now(), now() + interval '1 hour')",
        // An explicit ID keeps the sequence uncalled in the disposable probe.
        "INSERT INTO event_voice_sessions (id, guild_id, member_id, channel_id, joined_at)
         VALUES (1, '1', '2', '3', now())",
        "INSERT INTO live_sequence_watermarks (guild_id, seq) VALUES ('1', 1)",
    ] {
        assert_undo_refused(migrator, seed, "new schema contains evidence").await;
    }
    for assignment in [
        "intent_id = NULL",
        "action = 'voice.move_member'",
        "delivery_hash = repeat('d', 64)",
        "from_channel_id = '100000000000000001'",
        "request_id = repeat('A', 26)",
    ] {
        // Isolate each condition, including shapes normally excluded by the
        // forward receipt CHECK. The test transaction never reaches commit.
        let seed = format!(
            "ALTER TABLE internal_action_log DROP CONSTRAINT internal_action_log_receipt_check;
             UPDATE internal_action_log SET {assignment}"
        );
        assert_undo_refused(migrator, &seed, "new schema contains evidence").await;
    }
    for seed in [
        "DELETE FROM _sqlx_migrations WHERE version = 482",
        "UPDATE _sqlx_migrations SET version = 499 WHERE version = 482",
        "UPDATE _sqlx_migrations SET success = false WHERE version = 473",
        "UPDATE _sqlx_migrations SET description = 'fixture drift' WHERE version = 481",
        "UPDATE _sqlx_migrations SET checksum = decode(repeat('00', 48), 'hex') WHERE version = 481",
        "DELETE FROM _two_bot_migrations WHERE version = 478",
        "UPDATE _two_bot_migrations SET version = 499 WHERE version = 478",
        "UPDATE _two_bot_migrations SET success = false WHERE version = 465",
        "UPDATE _two_bot_migrations SET description = 'fixture drift' WHERE version = 478",
        "UPDATE _two_bot_migrations SET checksum = decode(repeat('00', 48), 'hex') WHERE version = 478",
        "UPDATE web_contract_meta SET contract_version = '1.0' WHERE singleton",
    ] {
        assert_undo_refused(migrator, seed, "unexpected rehearsal schema or ledgers").await;
    }
    assert_eq!(rollback_catalog(migrator).await, catalog);
    let mut undo_tx = migrator.begin().await.unwrap();
    sqlx::raw_sql(SCHEMA_UNDO)
        .execute(&mut *undo_tx)
        .await
        .unwrap();
    undo_tx.commit().await.unwrap();
}

const SCHEMA_UNDO: &str = include_str!("../fixtures/production_schema_undo.sql");

async fn assert_undo_refused(migrator: &PgPool, seed: &str, message: &str) {
    let mut refusal = migrator.begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(seed))
        .execute(&mut *refusal)
        .await
        .unwrap();
    let error = sqlx::raw_sql(SCHEMA_UNDO)
        .execute(&mut *refusal)
        .await
        .unwrap_err();
    assert_sqlstate(&error, "23514");
    assert_eq!(
        error.as_database_error().unwrap().message(),
        message,
        "{seed}"
    );
    refusal.rollback().await.unwrap();
}

struct CandidateReadout {
    cutover: Vec<LedgerRow>,
    store: Vec<LedgerRow>,
    roles_match: bool,
    history_rows_present: bool,
}

impl CandidateReadout {
    fn evidence<'a>(&'a self, pin: &CandidatePin<'a>, roles: &'a str) -> DatabaseEvidence<'a> {
        DatabaseEvidence {
            source_sha: pin.source_sha,
            image_digest: pin.image_digest,
            cutover: &self.cutover,
            store: &self.store,
            // The fixture substitutes only role identifiers in the canonical SQL.
            roles: Some((roles, self.roles_match)),
            history_rows_present: Some(self.history_rows_present),
        }
    }
}

async fn candidate_readout(audit: &PgPool, name: &str) -> CandidateReadout {
    let mut conn = audit.acquire().await.unwrap();
    let mut tx = read_only_transaction(&mut conn).await.unwrap();
    sqlx::raw_sql(
        "SET LOCAL statement_timeout = '5s';
         SET LOCAL search_path = pg_catalog, pg_temp;
         SET LOCAL row_security = off;",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(role, format!("{name}_r"));
    let roles_match = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(scope_roles(
        &verification_sql(),
        name,
    )))
    .fetch_optional(&mut *tx)
    .await
    .unwrap()
    .is_none();
    let cutover = candidate_ledger(&mut tx,
        "SELECT version, description, success, encode(checksum, 'hex') FROM public._sqlx_migrations ORDER BY version"
    ).await;
    let store = candidate_ledger(&mut tx,
        "SELECT version, description, success, encode(checksum, 'hex') FROM public._two_bot_migrations ORDER BY version"
    ).await;
    let history_rows_present =
        sqlx::query_scalar("SELECT EXISTS (SELECT FROM public.backfill_history_messages)")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.rollback().await.unwrap();
    CandidateReadout {
        cutover,
        store,
        roles_match,
        history_rows_present,
    }
}

async fn candidate_ledger(conn: &mut PgConnection, sql: &'static str) -> Vec<LedgerRow> {
    sqlx::query_as::<_, (i64, String, bool, String)>(sql)
        .fetch_all(conn)
        .await
        .unwrap()
        .into_iter()
        .map(|(version, description, success, checksum_hex)| LedgerRow {
            version,
            description,
            success,
            checksum_hex,
        })
        .collect()
}

async fn verify_rollback_candidate(
    audit: &PgPool,
    migrator: &PgPool,
    name: &str,
    before_grants: &CandidateReadout,
) {
    const SOURCE: &str = "0123456789abcdef0123456789abcdef01234567";
    const MANIFEST: &[u8] = b"synthetic image manifest";
    const DIGEST: &str = "sha256:9a7a62e0d09caf3bb1299712cfea3f935aa1c5f2b3046d1c4425612cfd29a38a";
    let store = Chain::Store.migrator().unwrap();
    let roles = roles_sha256();
    let mut pin = CandidatePin {
        source_sha: SOURCE,
        image_digest: DIGEST,
        cutover_sha256: &manifest_hash(SOURCE, &[], &CUTOVER),
        store_sha256: &manifest_hash(SOURCE, &[], &store),
        roles_sha256: &roles,
        privacy_access: PrivacyAccess::SeparateOperator,
    };
    let mut artifact = ArtifactEvidence {
        image_manifest: MANIFEST,
        cutover: &CUTOVER,
        store: &store,
    };
    let readout = candidate_readout(audit, name).await;
    assert_eq!(
        verify(&pin, &artifact, &before_grants.evidence(&pin, &roles)),
        Err(Refusal::Access)
    );
    let mut evidence = readout.evidence(&pin, &roles);
    assert_eq!(verify(&pin, &artifact, &evidence), Ok(()));

    artifact.image_manifest = b"different image manifest";
    assert_eq!(verify(&pin, &artifact, &evidence), Err(Refusal::Artifact));
    artifact.image_manifest = MANIFEST;
    evidence.image_digest = "";
    assert_eq!(verify(&pin, &artifact, &evidence), Err(Refusal::Artifact));
    evidence.image_digest = DIGEST;
    evidence.cutover = &[];
    assert_eq!(
        verify(&pin, &artifact, &evidence),
        Err(Refusal::CutoverLedger)
    );
    evidence.cutover = &readout.cutover;
    let mut bad_store = readout.store.clone();
    bad_store[0].checksum_hex = "00".repeat(48);
    evidence.store = &bad_store;
    assert_eq!(
        verify(&pin, &artifact, &evidence),
        Err(Refusal::StoreLedger)
    );
    evidence.store = &readout.store;
    pin.store_sha256 = "";
    assert_eq!(
        verify(&pin, &artifact, &evidence),
        Err(Refusal::StoreLedger)
    );
    let store_sha256 = manifest_hash(SOURCE, &[], &store);
    pin.store_sha256 = &store_sha256;
    evidence.roles = None;
    assert_eq!(verify(&pin, &artifact, &evidence), Err(Refusal::Access));
    evidence.roles = Some(("", true));
    assert_eq!(verify(&pin, &artifact, &evidence), Err(Refusal::Access));
    evidence.roles = Some((&roles, readout.roles_match));
    pin.privacy_access = PrivacyAccess::GatewayNeedsOperatorPrivileges;
    assert_eq!(verify(&pin, &artifact, &evidence), Err(Refusal::Access));
    pin.privacy_access = PrivacyAccess::SeparateOperator;
    evidence.history_rows_present = None;
    assert_eq!(verify(&pin, &artifact, &evidence), Err(Refusal::History));

    // These observations exercise the no-link, apparent exact match,
    // conflicting and ambiguous cases. None supplies a guild for 0409.
    sqlx::query(WRITE_HISTORY).execute(migrator).await.unwrap();
    for observations in [
        &[][..],
        &[("101", "30", "rollback-fixture-a")][..],
        &[("102", "31", "rollback-fixture-a")][..],
        &[
            ("101", "30", "rollback-fixture-a"),
            ("102", "30", "rollback-fixture-b"),
        ][..],
    ] {
        for &(guild, author, key) in observations {
            let inserted = sqlx::query(
                "INSERT INTO event_store
                    (event_type, guild_id, channel_id, message_id, user_id, event_time, dedup_key)
                 SELECT 'MESSAGE_CREATE', $1, channel_id, message_id, $2, sent_at, $3
                 FROM backfill_history_messages WHERE message_id = '10'",
            )
            .bind(guild)
            .bind(author)
            .bind(key)
            .execute(migrator)
            .await
            .unwrap();
            assert_eq!(inserted.rows_affected(), 1);
        }
        let readout = candidate_readout(audit, name).await;
        assert_eq!(
            verify(&pin, &artifact, &readout.evidence(&pin, &roles)),
            Err(Refusal::History)
        );
        assert_drain_defers(
            &run_retention(migrator, RetentionMode::Execute)
                .await
                .unwrap(),
        );
        assert_preserved(migrator).await;
        let removed = sqlx::query(
            "DELETE FROM event_store WHERE dedup_key IN ('rollback-fixture-a', 'rollback-fixture-b')",
        )
        .execute(migrator)
        .await
        .unwrap();
        assert_eq!(removed.rows_affected(), observations.len() as u64);
    }
    assert_counts(
        migrator,
        &[("SELECT count(*) FROM backfill_history_messages", 1)],
    )
    .await;
}

async fn assert_runtime_refusal(runtime: &PgPool, request: &str, state: &str) {
    let mode = ErasureMode::Execute { actor: "fixture" };
    let standalone = erase_member(runtime, GUILD, MEMBER, mode).await;
    let queued = fulfill_user_deletion(runtime, request).await;
    assert_sqlstate(&standalone.unwrap_err(), state);
    assert_sqlstate(&queued.unwrap_err(), state);
}

async fn assert_preserved(runtime: &PgPool) {
    let pending = "SELECT count(*) FROM retention_deletion_requests WHERE status = 'pending'";
    let expected = [
        (MEMBERS, 3),
        (RESERVATIONS, 3),
        (QUEUE, 1),
        (RECEIPTS, 0),
        (pending, 1),
    ];
    assert_counts(runtime, &expected).await;
}

async fn assert_counts(pool: &PgPool, expected: &[(&'static str, i64)]) {
    for &(query, rows) in expected {
        assert_eq!(count(pool, query).await, rows, "{query}");
    }
}

async fn restore_v17(migrator: &PgPool, name: &str) {
    let dir = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .or_else(|| std::env::var_os("RUNNER_TEMP"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
        .join(name);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("privacy-v17.ndjson.gz");
    let result = async {
        let manifest = dump(migrator, &path).await?;
        sqlx::raw_sql("DELETE FROM event_voice_sessions; DELETE FROM event_attendance_windows")
            .execute(migrator)
            .await?;
        let report = restore(migrator, &path).await?;
        Ok::<_, two_bot_core::backup::dump::DbDumpError>((manifest.version, report.ok))
    }
    .await;
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(result.unwrap(), (17, true));
    let tail = |chain: &Migrator| chain.iter().next_back().unwrap().version;
    assert_ledgers(migrator, tail(&CUTOVER), tail(&two_bot_store::MIGRATOR)).await;
}

async fn assert_ledgers(migrator: &PgPool, cutover: i64, store: i64) {
    let versions: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT max(version) FROM _sqlx_migrations), (SELECT max(version) FROM _two_bot_migrations)"
    ).fetch_one(migrator).await.unwrap();
    assert_eq!(versions, (cutover, store));
}

async fn assert_hidden_schema_and_replay_refusals(
    runtime: &PgPool,
    migrator: &PgPool,
    request: &str,
) {
    sqlx::raw_sql(
        "CREATE TABLE privacy_hidden_fixture (owner_id text NOT NULL);
         INSERT INTO privacy_hidden_fixture VALUES ('200000000000000001');",
    )
    .execute(migrator)
    .await
    .unwrap();
    assert_eq!(count(runtime, "SELECT count(*) FROM information_schema.columns WHERE table_name = 'privacy_hidden_fixture'").await, 0);
    let gaps = member_erasure::schema_gaps(&mut runtime.acquire().await.unwrap())
        .await
        .unwrap();
    let backfill = "uncovered identity: backfill_history_messages.author_id";
    let hidden = "uncovered identity: privacy_hidden_fixture.owner_id";
    assert_eq!(gaps, [backfill, hidden]);
    for mode in [RetentionMode::DryRun, RetentionMode::Execute] {
        let report = run_retention(runtime, mode).await.unwrap();
        assert_eq!(
            (report.schema_gap_deferrals, report.guild_purges_failed),
            (1, 1)
        );
        assert_eq!(
            (report.deletions_completed, report.guild_rows_purged),
            (0, 0)
        );
    }
    assert_preserved(runtime).await;
    // Isolate the role and replay controls after proving full-schema refusal.
    sqlx::raw_sql(
        "DROP TABLE privacy_hidden_fixture;
         INSERT INTO moderation_idempotency
            (guild_id, idempotency_key, action, request_hash, state, claimed_at)
         SELECT DISTINCT guild_id, 'guard-200000000000000001', 'warn', 'fixture', 'claimed', now()
         FROM members;",
    )
    .execute(migrator)
    .await
    .unwrap();
    let refusal = fulfill_user_deletion(runtime, request).await.unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("unresolved erasure replay guard"),
        "{refusal}"
    );
    let report = run_retention(migrator, RetentionMode::Execute)
        .await
        .unwrap();
    assert_eq!(
        (report.deletions_deferred, report.guild_purges_deferred),
        (1, 1)
    );
    sqlx::raw_sql(
        "UPDATE moderation_idempotency SET state = 'done', outcome = 'operator_released',
          completed_at = now(), request_hash = 'operator_released', claim_token = 'operator_released';
         INSERT INTO event_store (event_type, guild_id, user_id, event_time, dedup_key)
         VALUES ('PRESENCE_UPDATE', '100000000000000001', '200000000000000001', now() - interval '40 days', 'fixture')"
    ).execute(migrator).await.unwrap();
}
