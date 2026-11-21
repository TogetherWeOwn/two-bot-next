//! Isolate legacy ACL prerequisites from attendance availability. These are
//! refusal probes, not a successful erasure or a pre-expand admission receipt.

use sqlx::{migrate::Migrator, Acquire, Postgres, Transaction};
use two_bot_core::database_roles::{plan_for_phase, Phase};
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
    let result = probe(&db).await;
    db.close().await.expect("verify disposable teardown");
    result.unwrap();
}

async fn probe(db: &TestDatabase) -> Result<(), sqlx::Error> {
    let pool = db.pool();
    let mut store = through(&two_bot_store::MIGRATOR, 465);
    store.dangerous_set_table_name(two_bot_store::TABLE_NAME);
    store.run(pool).await?;

    let mut tx = pool.begin().await?;
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

    // Use the checked-in ACL rules for objects present at this phase. Only the
    // test harness strips the transaction wrapper and namespaces the role
    // names: no cluster-wide role or grant survives the fixture transaction.
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *tx)
        .await?;
    assert!(name.starts_with("two_bot_test_") && name.len() <= 61);
    assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    let runtime = format!("{name}_b");
    let plan = plan_for_phase(Phase::Bootstrap);
    let body = plan
        .split_once("\nBEGIN;\n")
        .and_then(|(_, body)| body.strip_suffix("\nCOMMIT;\n"))
        .expect("role plan transaction wrapper");
    let scoped = body
        .replace("two_bot_migrator_ro", &format!("{name}_r"))
        .replace("two_bot_migrator", &format!("{name}_m"))
        .replace("two_web_reader", &format!("{name}_w"))
        .replace("two_bot_runtime", &runtime);
    sqlx::raw_sql(sqlx::AssertSqlSafe(scoped))
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {runtime}")))
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
    let error = result.expect_err("reviewed runtime role must refuse this legacy operation");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|db| db.code())
            .as_deref(),
        Some("42501"),
        "must refuse on privilege, not absent/malformed SQL: {statement}"
    );
    Ok(())
}
