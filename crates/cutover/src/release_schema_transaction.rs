//! Atomic expansion for the 0475/0479–0482 and Store 0478 bundle.

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{migrate::Migrate, Acquire, PgConnection, PgPool};

use super::{read_ledger, read_store_ledger, reconcile, Chain, LedgerRow, RunError, MIGRATOR_ROLE};

const CUTOVER: [i64; 5] = [475, 479, 480, 481, 482];
const STORE: [i64; 1] = [478];
const INVERSE: &str = include_str!("../../../sql/release_schema_inverse.sql");
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

fn legacy_verifier() -> String {
    // All existing grants stay in place. The inverse drops only these
    // release-created objects, along with their grants.
    let matrix = super::AUDIT_MATRIX.trim_end().trim_end_matches(';');
    let legacy = format!(
        "SELECT * FROM ({matrix}) AS matrix WHERE (schema_name, name) NOT IN (
            ('public', 'voice_heat_hours'), ('public', 'event_attendance_windows'),
            ('public', 'event_voice_sessions'), ('public', 'event_voice_sessions_id_seq'),
            ('public', 'live_sequence_watermarks'), ('web_v1', 'voice_heat')
        )"
    );
    super::AUDIT_VERIFY.replace("-- @matrix", &legacy)
}

async fn release_roles_clean(conn: &mut PgConnection, query: String) -> Result<bool, RunError> {
    let finding: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
        .fetch_optional(conn)
        .await
        .map_err(|_| failure("release role readback failed"))?;
    Ok(finding.is_none())
}

async fn attendance_fence(conn: &mut PgConnection) -> Result<Vec<u8>, RunError> {
    sqlx::query_scalar(
        "SELECT timestamptz_send(installed_on) FROM public._sqlx_migrations
         WHERE version = 475 AND success",
    )
    .fetch_one(conn)
    .await
    .map_err(|_| failure("release attendance fence readback failed"))
}

async fn retained_attendance_schema(conn: &mut PgConnection) -> Result<bool, RunError> {
    // Match the post-0475 catalog contract in attendance_0475_readback.sql.
    sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT FROM pg_class c
            JOIN pg_attribute a ON a.attrelid = c.oid AND NOT a.attisdropped
                               AND a.attname = 'attendance_recovered_at'
            JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum
            WHERE c.oid = to_regclass('public.event_instance_windows')
              AND c.relkind = 'r' AND NOT c.relrowsecurity AND NOT c.relhassubclass
              AND a.atttypid = 'timestamptz'::regtype AND NOT a.attnotnull
              AND pg_get_expr(d.adbin, d.adrelid) = 'CURRENT_TIMESTAMP'
        )",
    )
    .fetch_one(conn)
    .await
    .map_err(|_| failure("release attendance schema readback failed"))
}

/// Reverse the exact release bundle on the already verified target. This is
/// the database operation only: the protected caller must authenticate the
/// complete post-apply ledgers, target, and stopped writer's fence epoch.
/// The protected CLI uses `recover_release_schema` to require the stamp.
#[doc(hidden)]
pub async fn undo_release_schema(
    pool: &PgPool,
    expected_cutover: &[LedgerRow],
    expected_store: &[LedgerRow],
) -> Result<(Vec<LedgerRow>, Vec<LedgerRow>), RunError> {
    recover_release_schema(pool, expected_cutover, expected_store, None).await
}

/// Verify the 0475 timestamp before undo, or reconcile a completed inverse.
#[doc(hidden)]
pub async fn recover_release_schema(
    pool: &PgPool,
    expected_cutover: &[LedgerRow],
    expected_store: &[LedgerRow],
    expected_fence: Option<&str>,
) -> Result<(Vec<LedgerRow>, Vec<LedgerRow>), RunError> {
    let cutover = Chain::Cutover.migrator().map_err(RunError::Refused)?;
    let store = Chain::Store.migrator().map_err(RunError::Refused)?;
    let mut conn = pool
        .acquire()
        .await
        .map_err(|_| failure("release connection unavailable"))?;
    conn.close_on_drop();
    let mut tx = conn
        .begin()
        .await
        .map_err(|_| failure("release inverse transaction unavailable"))?;
    sqlx::raw_sql(
        "SET TRANSACTION ISOLATION LEVEL READ COMMITTED;
         SET LOCAL search_path = pg_catalog, pg_temp;
         SET LOCAL lock_timeout = '2s';
         SET LOCAL statement_timeout = '15s';",
    )
    .execute(&mut *tx)
    .await
    .map_err(|_| failure("release inverse transaction setup failed"))?;
    tx.lock()
        .await
        .map_err(|_| failure("release migration lock unavailable"))?;
    sqlx::raw_sql(
        "LOCK TABLE public._sqlx_migrations, public._two_bot_migrations, public.web_contract_meta
         IN SHARE ROW EXCLUSIVE MODE NOWAIT",
    )
    .execute(&mut *tx)
    .await
    .map_err(|_| failure("release inverse ledgers busy or unavailable"))?;

    let (before_cutover, before_store) = ledgers(&mut tx).await?;
    let migrator: bool = sqlx::query_scalar("SELECT current_user::text = $1")
        .bind(MIGRATOR_ROLE)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| failure("release inverse role read failed"))?;
    let fence = attendance_fence(&mut tx).await?;
    let retained = |rows: &[LedgerRow], removed: &[i64]| -> Vec<LedgerRow> {
        rows.iter()
            .filter(|row| !removed.contains(&row.version))
            .cloned()
            .collect()
    };
    let expected_restored = (
        retained(expected_cutover, &CUTOVER[1..]),
        retained(expected_store, &STORE),
    );
    if !migrator
        || expected_fence.is_some_and(|expected| expected != hex::encode(Sha256::digest(&fence)))
        || !reconcile(expected_cutover, &cutover).is_ok_and(|pending| pending.is_empty())
        || !reconcile(expected_store, &store).is_ok_and(|pending| pending.is_empty())
    {
        return Err(RunError::Refused(
            "release inverse post-apply evidence changed".to_owned(),
        ));
    }
    if expected_fence.is_some()
        && before_cutover == expected_restored.0
        && before_store == expected_restored.1
    {
        sqlx::raw_sql("LOCK TABLE ONLY public.event_instance_windows IN ACCESS SHARE MODE NOWAIT")
            .execute(&mut *tx)
            .await
            .map_err(|_| failure("release restored attendance table busy or unavailable"))?;
        let restored: bool = sqlx::query_scalar(
            "SELECT COALESCE((SELECT contract_version FROM public.web_contract_meta WHERE singleton) = '1.0', false)
                AND to_regclass('web_v1.voice_heat') IS NULL
                AND to_regclass('public.voice_heat_hours') IS NULL
                AND to_regclass('public.event_attendance_windows') IS NULL
                AND to_regclass('public.event_voice_sessions') IS NULL
                AND to_regclass('public.event_voice_sessions_id_seq') IS NULL
                AND to_regclass('public.live_sequence_watermarks') IS NULL
                AND EXISTS (SELECT FROM pg_attribute
                    WHERE attrelid = 'public.internal_action_log'::regclass AND attname = 'intent_id' AND attnotnull AND NOT attisdropped)
                AND NOT EXISTS (SELECT FROM pg_attribute WHERE attrelid = 'public.internal_action_log'::regclass
                    AND attname IN ('delivery_hash', 'from_channel_id', 'request_id') AND NOT attisdropped)
                AND EXISTS (SELECT FROM pg_constraint WHERE conrelid = 'public.internal_action_log'::regclass
                    AND conname = 'internal_action_log_outcome_check' AND contype = 'c' AND convalidated
                    AND pg_get_expr(conbin, conrelid) = '(outcome = ANY (ARRAY[''created''::text, ''updated''::text, ''cancelled''::text]))')
                AND NOT EXISTS (SELECT FROM pg_constraint WHERE conrelid = 'public.internal_action_log'::regclass
                    AND conname = 'internal_action_log_receipt_check')",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| failure("release restored schema read failed"))?;
        if restored
            && retained_attendance_schema(&mut tx).await?
            && release_roles_clean(&mut tx, legacy_verifier()).await?
        {
            tx.rollback()
                .await
                .map_err(|_| failure("release restored readback failed"))?;
            return Ok(expected_restored);
        }
        return Err(failure("release restored state refused"));
    }
    if before_cutover != expected_cutover
        || before_store != expected_store
        || !release_roles_clean(&mut tx, two_bot_core::database_roles::verification_sql()).await?
    {
        return Err(RunError::Refused(
            "release inverse post-apply evidence changed".to_owned(),
        ));
    }

    sqlx::raw_sql(INVERSE)
        .execute(&mut *tx)
        .await
        .map_err(|_| failure("guarded release inverse refused or failed"))?;
    let after = ledgers(&mut tx).await?;
    if after != expected_restored
        || attendance_fence(&mut tx).await? != fence
        || !retained_attendance_schema(&mut tx).await?
        || !release_roles_clean(&mut tx, legacy_verifier()).await?
    {
        return Err(failure("release inverse readback refused"));
    }
    tx.commit()
        .await
        .map_err(|_| failure("release inverse commit outcome unknown; reconcile both ledgers"))?;
    Ok(after)
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
    if !release_roles_clean(&mut tx, two_bot_core::database_roles::verification_sql()).await?
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
