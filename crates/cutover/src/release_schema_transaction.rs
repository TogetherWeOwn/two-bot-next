//! Atomic expansion for the 0475/0479–0482 and Store 0478 bundle.

use serde_json::Value;
use sqlx::{migrate::Migrate, Acquire, PgConnection, PgPool};

use super::{read_ledger, read_store_ledger, reconcile, Chain, LedgerRow, RunError, MIGRATOR_ROLE};

const CUTOVER: [i64; 5] = [475, 479, 480, 481, 482];
const STORE: [i64; 1] = [478];
const WEB_CONTRACT: &str = include_str!("../../../sql/web_v1.sql");
const GRANTS: &str = "
    GRANT SELECT, INSERT, DELETE ON public.voice_heat_hours TO two_bot_runtime;
    GRANT SELECT, INSERT, UPDATE, DELETE ON public.event_attendance_windows, public.event_voice_sessions TO two_bot_runtime;
    GRANT SELECT ON public.event_attendance_windows, public.event_voice_sessions TO two_bot_migrator_ro;
    GRANT USAGE, SELECT ON SEQUENCE public.event_voice_sessions_id_seq TO two_bot_runtime;
    GRANT SELECT, INSERT, UPDATE ON public.live_sequence_watermarks TO two_bot_runtime;
    GRANT SELECT ON public.live_sequence_watermarks TO two_bot_migrator_ro;
    GRANT SELECT ON web_v1.voice_heat TO two_web_reader;
";

fn failure(stage: &str) -> RunError {
    RunError::Failed(stage.to_owned(), Value::Null)
}

async fn ledgers(conn: &mut PgConnection) -> Result<(Vec<LedgerRow>, Vec<LedgerRow>), RunError> {
    let cutover = read_ledger(conn)
        .await
        .map_err(|_| failure("release cutover ledger read failed"))?;
    let store = read_store_ledger(conn)
        .await
        .map_err(|_| failure("release store ledger read failed"))?;
    Ok((cutover, store))
}

/// Apply only the pinned expansion, contract delta, and new-object grants.
/// The caller verifies the dispatch, target and plan. This function also
/// checks the ledgers under lock before changing them.
#[doc(hidden)]
pub async fn apply_release_schema(
    pool: &PgPool,
    expected_cutover: &[LedgerRow],
    expected_store: &[LedgerRow],
) -> Result<(Vec<LedgerRow>, Vec<LedgerRow>), RunError> {
    let cutover = Chain::Cutover.migrator().map_err(RunError::Refused)?;
    let store = Chain::Store.migrator().map_err(RunError::Refused)?;
    let mut conn = pool
        .acquire()
        .await
        .map_err(|_| failure("release connection unavailable"))?;
    // SQLx uses a session lock. Close on all return paths, including cancellation.
    conn.close_on_drop();
    let mut tx = conn
        .begin()
        .await
        .map_err(|_| failure("release transaction unavailable"))?;
    sqlx::raw_sql(
        "SET LOCAL search_path = public, pg_temp;
         SET LOCAL lock_timeout = '2s';
         SET LOCAL statement_timeout = '15s';",
    )
    .execute(&mut *tx)
    .await
    .map_err(|_| failure("release transaction setup failed"))?;
    tx.lock()
        .await
        .map_err(|_| failure("release migration lock unavailable"))?;
    sqlx::raw_sql(
        "LOCK TABLE public._sqlx_migrations, public._two_bot_migrations,
           public.web_contract_meta IN SHARE ROW EXCLUSIVE MODE NOWAIT",
    )
    .execute(&mut *tx)
    .await
    .map_err(|_| failure("release schema tables busy or unavailable"))?;

    let (before_cutover, before_store) = ledgers(&mut tx).await?;
    let eligible: bool = sqlx::query_scalar(
        "SELECT current_user::text = $1
           AND (SELECT pg_get_userbyid(relowner)::text = $1 FROM pg_class
                WHERE oid = 'public._sqlx_migrations'::regclass)
           AND (SELECT pg_get_userbyid(relowner)::text = $1 FROM pg_class
                WHERE oid = 'public._two_bot_migrations'::regclass)
           AND COALESCE((SELECT contract_version FROM public.web_contract_meta WHERE singleton) = '1.0', false)
           AND to_regclass('public.voice_heat_hours') IS NULL
           AND to_regclass('web_v1.voice_heat') IS NULL",
    )
    .bind(MIGRATOR_ROLE)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| failure("release precondition read failed"))?;
    if !eligible
        || before_cutover != expected_cutover
        || before_store != expected_store
        || reconcile(&before_cutover, &cutover).ok().as_deref() != Some(CUTOVER.as_slice())
        || reconcile(&before_store, &store).ok().as_deref() != Some(STORE.as_slice())
    {
        return Err(RunError::Refused(
            "release schema baseline changed".to_owned(),
        ));
    }

    for (migrator, table, versions) in [
        (&cutover, "public._sqlx_migrations", CUTOVER.as_slice()),
        (&store, "public._two_bot_migrations", STORE.as_slice()),
    ] {
        for migration in migrator
            .iter()
            .filter(|migration| versions.contains(&migration.version))
        {
            if migration.no_tx || migration.migration_type.is_down_migration() {
                return Err(RunError::Refused(
                    "release requires transactional up migrations".to_owned(),
                ));
            }
            // SQLx uses a savepoint on this connection. The outer transaction
            // owns the commit across both chains and the grants.
            tx.apply(table, migration)
                .await
                .map_err(|_| failure("release migration failed"))?;
        }
    }
    let (_, voice_heat_contract) = WEB_CONTRACT
        .split_once("-- Voice heat contract 1.1: trailing 28 complete UTC days; all 168 cells.\n")
        .ok_or_else(|| failure("release contract delta unavailable"))?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(voice_heat_contract))
        .execute(&mut *tx)
        .await
        .map_err(|_| failure("release contract delta failed"))?;
    sqlx::raw_sql(GRANTS)
        .execute(&mut *tx)
        .await
        .map_err(|_| failure("release grants failed"))?;
    let after = ledgers(&mut tx).await?;
    let findings: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(
        two_bot_core::database_roles::verification_sql(),
    ))
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| failure("release role readback failed"))?;
    if findings.is_some()
        || !reconcile(&after.0, &cutover).is_ok_and(|pending| pending.is_empty())
        || !reconcile(&after.1, &store).is_ok_and(|pending| pending.is_empty())
    {
        return Err(failure("release schema or role readback refused"));
    }
    tx.commit()
        .await
        .map_err(|_| failure("release commit outcome unknown; reconcile both ledgers"))?;
    Ok(after)
}
