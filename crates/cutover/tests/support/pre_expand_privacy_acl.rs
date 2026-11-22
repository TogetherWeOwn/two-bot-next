//! Disposable privacy checks across the 0480 schema and grant transition.

use sqlx::{
    migrate::Migrator, postgres::PgPoolOptions, Acquire, Connection, PgConnection, PgPool,
    Postgres, Transaction,
};
use two_bot_core::backup::dump::{dump, restore};
use two_bot_core::database_roles::{plan_for_phase, Phase};
use two_bot_cutover::member_erasure::{erase_member, ErasureMode};
use two_bot_cutover::retention::{
    fulfill_user_deletion, handle_guild_delete, request_user_deletion, run_retention, RetentionMode,
};
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
    validate_name(name);
    let plan = plan_for_phase(phase);
    let body = plan
        .split_once("\nBEGIN;\n")
        .and_then(|(_, body)| body.strip_suffix("\nCOMMIT;\n"))
        .expect("role plan transaction wrapper");
    let scoped = body
        .replace("two_bot_migrator_ro", &format!("{name}_r"))
        .replace("two_bot_migrator", &format!("{name}_m"))
        .replace("two_web_reader", &format!("{name}_w"))
        .replace("two_bot_runtime", &format!("{name}_b"));
    sqlx::raw_sql(sqlx::AssertSqlSafe(scoped))
        .execute(&mut *tx)
        .await?;
    Ok(())
}

async fn probe(pool: &PgPool) -> Result<(), sqlx::Error> {
    legacy_store(pool).await?;
    let mut tx = pool.begin().await?;
    install_legacy_contract(&mut tx).await?;
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *tx)
        .await?;
    // Only the outer test transaction controls whether these roles survive.
    install_roles(&mut tx, &name, Phase::Bootstrap).await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {name}_b")))
        .execute(&mut *tx)
        .await?;

    // Positive control: runtime can create and read a scoped reservation.
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
    assert_eq!(count(&mut *tx, RESERVATIONS).await, 1);
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

    // The committed role plan creates cluster roles. After dropping their
    // disposable database, remove those names from the guarded bootstrap DB.
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
    sqlx::raw_sql(
        "INSERT INTO members (guild_id, member_id) VALUES
             ('100000000000000001','200000000000000001'),
             ('100000000000000001','200000000000000002'),
             ('100000000000000002','200000000000000001');
         INSERT INTO voice_create_reservations (guild_id, user_id, created_at)
             SELECT guild_id, member_id, clock_timestamp() FROM members;",
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
    assert_runtime_refusal(&runtime, &request, "42P01").await;
    assert!(run_retention(&runtime, RetentionMode::Execute)
        .await
        .is_err());
    assert_preserved(&runtime).await;

    // The production order commits Cutover before Store. No new web contract
    // or ACL runs inside the window being probed here.
    through(&CUTOVER, 480).run(&migrator).await.unwrap();
    assert_ledgers(&migrator, 480, 465).await;
    for query in [SESSIONS, WINDOWS] {
        assert_eq!(count(&migrator, query).await, 0);
        assert_sqlstate(
            &sqlx::query_scalar::<_, i64>(query)
                .fetch_one(&runtime)
                .await
                .unwrap_err(),
            "42501",
        );
    }
    assert_runtime_refusal(&runtime, &request, "42501").await;
    assert!(run_retention(&runtime, RetentionMode::Execute)
        .await
        .is_err());
    assert_preserved(&runtime).await;

    CUTOVER.run(&migrator).await.unwrap();
    two_bot_store::migrate(&migrator).await.unwrap();
    two_bot_core::apply_web_contract(&migrator).await.unwrap();
    let mut grants = pool.begin().await.unwrap();
    install_roles(&mut grants, name, Phase::Full).await.unwrap();
    grants.commit().await.unwrap();
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
    assert_eq!(count(&runtime, SESSIONS).await, 3);
    assert_eq!(count(&runtime, WINDOWS).await, 2);
    assert_runtime_refusal(&runtime, &request, "42501").await;
    assert_preserved(&runtime).await;
    assert_eq!(
        count(&migrator, "SELECT count(*) FROM member_erasure_audit").await,
        0
    );

    assert_hidden_schema_and_replay_refusals(&runtime, &migrator, &request).await;
    // With the request still pending, settle it using the existing operator
    // role to isolate the queued guild failure under runtime privileges.
    let receipt = fulfill_user_deletion(&migrator, &request).await.unwrap();
    assert_eq!(receipt.receipt_id, format!("rcpt-{request}"));
    assert!(fulfill_user_deletion(&migrator, &request).await.is_err());
    assert_eq!(count(&runtime, RECEIPTS).await, 1);
    assert_eq!(count(&runtime, MEMBERS).await, 2);
    assert_eq!(count(&runtime, SESSIONS).await, 2);
    assert_eq!(count(&runtime, WINDOWS).await, 2);
    let failed = run_retention(&runtime, RetentionMode::Execute)
        .await
        .unwrap();
    assert_eq!(
        (failed.guild_rows_purged, failed.guild_purges_failed),
        (0, 1)
    );
    assert_eq!(count(&runtime, QUEUE).await, 1);
    assert_eq!(count(&runtime, RESERVATIONS).await, 2);
    assert_eq!(count(&runtime, SESSIONS).await, 2);

    let drained = run_retention(&migrator, RetentionMode::Execute)
        .await
        .unwrap();
    assert!(drained.guild_rows_purged > 0);
    assert_eq!(count(&runtime, QUEUE).await, 0);
    let survivors: Vec<(String, String)> =
        sqlx::query_as("SELECT guild_id, member_id FROM event_voice_sessions")
            .fetch_all(&runtime)
            .await
            .unwrap();
    assert_eq!(
        survivors,
        [(GUILD.to_owned(), "200000000000000002".to_owned())]
    );
    assert_eq!(count(&runtime, WINDOWS).await, 1);
    assert_eq!(count(&runtime, RESERVATIONS).await, 1);
    assert_eq!(count(&runtime, "SELECT count(*) FROM moderation_idempotency WHERE state = 'done' AND outcome = 'operator_released'").await, 2);
}

async fn assert_runtime_refusal(runtime: &PgPool, request: &str, state: &str) {
    assert_sqlstate(
        &erase_member(
            runtime,
            GUILD,
            MEMBER,
            ErasureMode::Execute { actor: "fixture" },
        )
        .await
        .unwrap_err(),
        state,
    );
    assert_sqlstate(
        &fulfill_user_deletion(runtime, request).await.unwrap_err(),
        state,
    );
}

async fn assert_preserved(runtime: &PgPool) {
    assert_eq!(count(runtime, MEMBERS).await, 3);
    assert_eq!(count(runtime, RESERVATIONS).await, 3);
    assert_eq!(count(runtime, QUEUE).await, 1);
    assert_eq!(count(runtime, RECEIPTS).await, 0);
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
    assert_ledgers(migrator, 481, 478).await;
}

async fn assert_ledgers(migrator: &PgPool, cutover: i64, store: i64) {
    assert_eq!(
        count(migrator, "SELECT max(version) FROM _sqlx_migrations").await,
        cutover
    );
    assert_eq!(
        count(migrator, "SELECT max(version) FROM _two_bot_migrations").await,
        store
    );
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
    let mut conn = runtime.acquire().await.unwrap();
    assert_eq!(
        member_erasure::schema_gaps(&mut conn).await.unwrap(),
        ["uncovered identity: privacy_hidden_fixture.owner_id"]
    );
    drop(conn);
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
    let report = run_retention(runtime, RetentionMode::Execute)
        .await
        .unwrap();
    assert_eq!(
        (report.deletions_deferred, report.guild_purges_deferred),
        (1, 1)
    );
    assert_preserved(runtime).await;
    // Synthetic settled fence with the same redacted markers used by the
    // operator release path.
    sqlx::query(
        "UPDATE moderation_idempotency SET state = 'done', outcome = 'operator_released',
          completed_at = now(), request_hash = 'operator_released', claim_token = 'operator_released'"
    ).execute(migrator).await.unwrap();
}
