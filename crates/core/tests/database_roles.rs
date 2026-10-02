//! Scratch database and cluster-unique groups; never touch existing app data.
#![cfg(feature = "db")]

#[path = "support/test_database.rs"]
mod test_database;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering};
use two_bot_core::database_roles;
use two_bot_core::member_moderation::{AuditRow, ClaimState, MemberModerationStore, StoreError};
use two_bot_core::member_moderation_store::PgMemberModerationStore;

const MEMBER_GRANTS: &str =
    include_str!("../../cutover/migrations/0114_moderation_member_runtime_grants.sql");
const MEMBER_TABLES: [&str; 5] = [
    "moderation_warnings",
    "moderation_scheduled_unbans",
    "moderation_member_bans",
    "moderation_audit",
    "moderation_idempotency",
];

// Parallel tests share one pid and can read the same clock tick.
static NEXT_DB: AtomicU64 = AtomicU64::new(0);

const TEST_URL: &str = "postgres://agent_test:@agent-testdb:5432/agent_test";
const SCHEDULED_INSERT: &str = "INSERT INTO public.scheduled_messages
    (id, guild_id, channel_id, body, next_run_at, created_by, created_at, updated_by, updated_at)
    VALUES ('scheduled-role-probe', 'g', 'c', 'hello', '2026-01-01T00:00:00.000Z',
            'actor', '2026-01-01T00:00:00.000Z', 'actor', '2026-01-01T00:00:00.000Z')";
const SCHEDULED_CLAIM: &str = "WITH due AS (
    SELECT id FROM public.scheduled_messages
     WHERE guild_id = 'g' AND enabled AND next_run_at <= '2026-01-01T00:10:00.000Z'
     ORDER BY next_run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED
    ) UPDATE public.scheduled_messages
       SET next_run_at = '2026-01-01T00:11:00.000Z', claim_token = 'role-claim',
           claimed_at = '2026-01-01T00:10:00.000Z',
           occurrence_nonce = COALESCE(occurrence_nonce, 'role-nonce')
     WHERE guild_id = 'g' AND id IN (SELECT id FROM due)
       AND next_run_at <= '2026-01-01T00:10:00.000Z'
     RETURNING id, body, claim_token, occurrence_nonce";

fn names() -> (String, Vec<String>) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = NEXT_DB.fetch_add(1, Ordering::Relaxed);
    let name = format!("dbroles10892_{}_{stamp}_{sequence}", std::process::id());
    let roles = ["m", "b", "w"].map(|suffix| format!("{name}_{suffix}"));
    (name, roles.to_vec())
}

fn isolated(sql: &str, roles: &[String]) -> String {
    sql.replace("two_bot_migrator", &roles[0])
        .replace("two_bot_runtime", &roles[1])
        .replace("two_web_reader", &roles[2])
}

async fn execute(pool: &PgPool, sql: String) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await?;
    Ok(())
}

async fn findings(pool: &PgPool, roles: &[String]) -> Result<Vec<String>, sqlx::Error> {
    let sql = include_str!("../../../sql/verify_database_roles.sql").replace(
        "-- @matrix",
        include_str!("../../../sql/database_role_matrix.sql"),
    );
    let mut tx = pool.begin().await?;
    sqlx::raw_sql("SET TRANSACTION READ ONLY; SET LOCAL search_path = pg_catalog, pg_temp;")
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query_scalar(sqlx::AssertSqlSafe(isolated(&sql, roles)))
        .fetch_all(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(result)
}

fn require(condition: bool, message: &str) -> Result<(), sqlx::Error> {
    if condition {
        Ok(())
    } else {
        Err(sqlx::Error::InvalidArgument(message.to_owned()))
    }
}

async fn as_role(pool: &PgPool, role: &str, sql: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {role}; {sql}")))
        .execute(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(())
}

async fn denied(pool: &PgPool, role: &str, sql: &str) -> Result<(), sqlx::Error> {
    match as_role(pool, role, sql).await {
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42501") => Ok(()),
        _ => Err(sqlx::Error::InvalidArgument(format!(
            "expected permission denial for {sql}"
        ))),
    }
}

async fn admission_as_runtime(pool: &PgPool, role: &str) -> Result<(), sqlx::Error> {
    use two_bot_core::send_admission::{
        AdmissionError, PgSendAdmission, SendAdmission, SendCooldown,
    };

    let role = role.to_owned();
    let runtime = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _| {
            let role = role.clone();
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET ROLE {role}")))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await?;
    let result = async {
        let error = |error: AdmissionError| sqlx::Error::InvalidArgument(error.to_string());
        let gate = PgSendAdmission::new(runtime.clone(), "offline-database-role-admission")
            .map_err(error)?;
        gate.admit()
            .await
            .map_err(error)?
            .complete(None)
            .await
            .map_err(error)?;
        gate.admit()
            .await
            .map_err(error)?
            .complete(Some(SendCooldown::Indefinite))
            .await
            .map_err(error)?;
        gate.extend(SendCooldown::FiniteMs(1))
            .await
            .map_err(error)?;
        require(
            matches!(gate.admit().await, Err(AdmissionError::Blocked)),
            "runtime lost indefinite admission hold",
        )
    }
    .await;
    runtime.close().await;
    result
}

async fn scheduled_runtime_probe(pool: &PgPool, role: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {role}")))
        .execute(&mut *tx)
        .await?;
    sqlx::query(SCHEDULED_INSERT).execute(&mut *tx).await?;
    let body: String = sqlx::query_scalar(
        "SELECT body FROM public.scheduled_messages WHERE id = 'scheduled-role-probe'",
    )
    .fetch_one(&mut *tx)
    .await?;
    require(body == "hello", "runtime scheduled SELECT failed")?;
    let updated = sqlx::query(
        "UPDATE public.scheduled_messages SET body = 'updated' WHERE id = 'scheduled-role-probe'",
    )
    .execute(&mut *tx)
    .await?;
    require(
        updated.rows_affected() == 1,
        "runtime scheduled UPDATE failed",
    )?;
    let claimed: (String, String, Option<String>, Option<String>) =
        sqlx::query_as(SCHEDULED_CLAIM).fetch_one(&mut *tx).await?;
    require(
        claimed
            == (
                "scheduled-role-probe".to_owned(),
                "updated".to_owned(),
                Some("role-claim".to_owned()),
                Some("role-nonce".to_owned()),
            ),
        "runtime did not claim the scheduled occurrence",
    )?;
    let deleted =
        sqlx::query("DELETE FROM public.scheduled_messages WHERE id = 'scheduled-role-probe'")
            .execute(&mut *tx)
            .await?;
    require(
        deleted.rows_affected() == 1,
        "runtime scheduled DELETE failed",
    )?;
    tx.rollback().await?;
    Ok(())
}

async fn exercise(pool: &PgPool, roles: &[String]) -> Result<(), sqlx::Error> {
    // Real migration files, not a reduced fixture that omits trigger/sequence paths.
    for migration in [
        include_str!("../../cutover/migrations/0001_funnel.sql"),
        include_str!("../../cutover/migrations/0002_leveling.sql"),
        include_str!("../../cutover/migrations/0110_moderation_member.sql"),
        include_str!("../../cutover/migrations/0111_moderation_ban_ownership.sql"),
        include_str!("../../cutover/migrations/0112_moderation_legacy_timestamps.sql"),
        include_str!("../../cutover/migrations/0113_moderation_unban_retry_order.sql"),
        include_str!("../../cutover/migrations/0114_moderation_member_runtime_grants.sql"),
        include_str!("../../cutover/migrations/0120_channel_moderation.sql"),
        include_str!("../../cutover/migrations/0121_channel_claim_generation.sql"),
        include_str!("../../cutover/migrations/0122_channel_lockdown_generation.sql"),
        include_str!("../../cutover/migrations/0140_scheduled_messages.sql"),
        include_str!("../../cutover/migrations/0141_scheduled_messages_legacy_upgrade.sql"),
        include_str!("../../cutover/migrations/0150_sticky_messages.sql"),
        include_str!("../../cutover/migrations/0160_rsvp.sql"),
        include_str!("../../cutover/migrations/0170_lfg.sql"),
        include_str!("../../cutover/migrations/0190_onboarding.sql"),
        include_str!("../../cutover/migrations/0200_self_roles.sql"),
        include_str!("../../cutover/migrations/0210_tickets.sql"),
        include_str!("../../cutover/migrations/0220_automod.sql"),
        include_str!("../../cutover/migrations/0221_automod_delivery_claims.sql"),
        include_str!("../../cutover/migrations/0222_automod_counted_claim.sql"),
        include_str!("../../cutover/migrations/0223_automod_preserved_match.sql"),
        include_str!("../../cutover/migrations/0300_website_contract.sql"),
        include_str!("../../cutover/migrations/0310_presence_probe.sql"),
        include_str!("../../cutover/migrations/0311_community_scorecard.sql"),
        include_str!("../../cutover/migrations/0320_gateway_sessions.sql"),
        include_str!("../../cutover/migrations/0321_gateway_boot_directives.sql"),
        include_str!("../../cutover/migrations/0330_guild_settings.sql"),
        include_str!("../../cutover/migrations/0331_guild_settings_versions.sql"),
        include_str!("../../cutover/migrations/0332_guild_settings_allocator.sql"),
        include_str!("../../cutover/migrations/0333_guild_settings_revision.sql"),
        include_str!("../../cutover/migrations/0334_guild_settings_cas.sql"),
        include_str!("../../cutover/migrations/0340_operational_audit.sql"),
        include_str!("../../cutover/migrations/0350_internal_actions.sql"),
        include_str!("../../cutover/migrations/0353_internal_clock_high_water.sql"),
        include_str!("../../cutover/migrations/0361_discord_send_admission.sql"),
    ] {
        // Never grant a shared cluster role: isolate names even in migrations.
        execute(pool, isolated(migration, roles)).await?;
    }
    sqlx::raw_sql("CREATE TABLE public._sqlx_migrations (version bigint PRIMARY KEY);")
        .execute(pool)
        .await?;
    sqlx::raw_sql(include_str!("../../../sql/web_v1.sql"))
        .execute(pool)
        .await?;
    require(
        findings(pool, roles)
            .await?
            .iter()
            .filter(|finding| finding.starts_with("missing role:"))
            .count()
            == 3,
        "missing groups were not reported",
    )?;
    execute(pool, isolated(&database_roles::plan(), roles)).await?;
    execute(pool, isolated(&database_roles::plan(), roles)).await?;
    require(
        findings(pool, roles).await?.is_empty(),
        "clean plan drifted",
    )?;

    exercise_member_grants(pool, roles).await?;
    exercise_member_store(pool, &roles[1]).await?;

    // Effective ACLs include PUBLIC/inherited access. Schema denial alone is
    // not proof of no reader sequence or function grant. The CAS sequence is
    // deliberately unowned by a column, so it needs an explicit matrix entry.
    require(
        sqlx::query_scalar::<_, bool>(
            r#"SELECT
                has_sequence_privilege($1::text, 'public.guild_settings_cas_seq', 'USAGE')
                AND has_sequence_privilege($1::text, 'public.guild_settings_cas_seq', 'SELECT')
                AND NOT has_sequence_privilege($1::text, 'public.guild_settings_cas_seq', 'UPDATE')
                AND NOT has_sequence_privilege($2::text, 'public.guild_settings_cas_seq', 'USAGE')
                AND NOT has_sequence_privilege($2::text, 'public.guild_settings_cas_seq', 'SELECT')
                AND NOT has_sequence_privilege($2::text, 'public.guild_settings_cas_seq', 'UPDATE')
                AND NOT has_function_privilege($1::text, 'public.guild_settings_assign_version()', 'EXECUTE')
                AND NOT has_function_privilege($2::text, 'public.guild_settings_assign_version()', 'EXECUTE')
                AND NOT (SELECT prosecdef FROM pg_proc
                    WHERE oid = 'public.guild_settings_assign_version()'::regprocedure)
                AND NOT EXISTS (SELECT FROM pg_depend
                    WHERE classid = 'pg_class'::regclass
                      AND refclassid = 'pg_class'::regclass
                      AND objid = 'public.guild_settings_cas_seq'::regclass
                      AND deptype IN ('a', 'i'))"#,
        )
        .bind(&roles[1])
        .bind(&roles[2])
        .fetch_one(pool)
        .await?,
        "CAS sequence or trigger function privileges differ",
    )?;
    as_role(
        pool,
        &roles[0],
        "CREATE TABLE public.migrator_probe (id int)",
    )
    .await?;
    // Invoker trigger DML must work without runtime direct function EXECUTE.
    as_role(
        pool,
        &roles[1],
        r#"SELECT * FROM public.members;
        SELECT nextval('public.guild_settings_cas_seq');
        SELECT last_value FROM public.guild_settings_cas_seq;
        DO $cas$
        DECLARE
            inserted bigint;
            updated bigint;
        BEGIN
            INSERT INTO public.guild_settings (guild_id, key, value, updated_by, cas_version)
                VALUES ('test', 'test', '1', 'test', 42) RETURNING cas_version INTO inserted;
            IF inserted IS NULL OR inserted >= 0 THEN
                RAISE EXCEPTION 'insert did not allocate a CAS token';
            END IF;
            UPDATE public.guild_settings SET value = '2', cas_version = inserted
                WHERE guild_id = 'test' AND key = 'test' RETURNING cas_version INTO updated;
            IF updated IS NULL OR updated >= inserted THEN
                RAISE EXCEPTION 'update did not advance the CAS token';
            END IF;
            DELETE FROM public.guild_settings WHERE guild_id = 'test' AND key = 'test';
        END;
        $cas$;"#,
    )
    .await?;
    // Migration 0200 relations are runtime-operated: event claims and panel
    // lane leases must work under the least-privilege login.
    as_role(pool, &roles[1], "SELECT * FROM public.self_role_audit; INSERT INTO public.self_role_audit (event_id, guild_id, panel_id, member_id, source_id, source, operation, outcome, added_role_ids, removed_role_ids, created_at) VALUES ('roles-probe', 'g', 'p', 'm', 's', 'button', 'add', 'processing', '[]', '[]', '2026-01-01T00:00:00Z'); UPDATE public.self_role_audit SET reason = 'probe' WHERE event_id = 'roles-probe'; DELETE FROM public.self_role_audit WHERE event_id = 'roles-probe'").await?;
    as_role(pool, &roles[1], "SELECT * FROM public.self_role_panel_claims; INSERT INTO public.self_role_panel_claims (guild_id, member_id, panel_id, claim_token, claim_generation, processing_expires_at) VALUES ('g', 'm', 'p', 'tok', 1, now() + interval '1 minute'); UPDATE public.self_role_panel_claims SET latest_option_key = 'probe' WHERE guild_id = 'g' AND member_id = 'm' AND panel_id = 'p'; DELETE FROM public.self_role_panel_claims WHERE guild_id = 'g' AND member_id = 'm' AND panel_id = 'p'").await?;
    scheduled_runtime_probe(pool, &roles[1]).await?;
    // Migration 0210 relations require runtime CRUD, including the transcript's
    // ticket foreign key. Delete the transcript before its parent ticket.
    as_role(pool, &roles[1], "INSERT INTO public.tickets (id, guild_id, channel_id, opener_id, status, created_at) VALUES ('ticket-probe', 'g', 'c', 'm', 'open', '2026-01-01T00:00:00Z'); SELECT * FROM public.tickets; UPDATE public.tickets SET claimed_by = 'staff' WHERE id = 'ticket-probe'; INSERT INTO public.ticket_transcripts (ticket_id, guild_id, channel_id, opener_id, claimed_by, content, message_count, created_at, purge_after) VALUES ('ticket-probe', 'g', 'c', 'm', 'staff', 'probe', 1, '2026-01-01T00:00:00Z', '2026-04-01T00:00:00Z'); SELECT * FROM public.ticket_transcripts; UPDATE public.ticket_transcripts SET content = 'updated probe' WHERE ticket_id = 'ticket-probe'; DELETE FROM public.ticket_transcripts WHERE ticket_id = 'ticket-probe'; DELETE FROM public.tickets WHERE id = 'ticket-probe'").await?;
    automod_runtime_role_regression(pool, roles).await?;
    for view in [
        "contract_meta",
        "live_counts",
        "rank_counts",
        "members",
        "member_milestones",
        "upcoming_events",
        "next_event",
        "funnel_daily",
        "funnel_by_source",
    ] {
        as_role(pool, &roles[2], &format!("SELECT * FROM web_v1.{view}")).await?;
    }
    admission_as_runtime(pool, &roles[1]).await?;
    for sql in [
        "DELETE FROM public.discord_send_admission",
        "TRUNCATE public.discord_send_admission",
        "CREATE TABLE public.runtime_probe (id int)",
        "CREATE SCHEMA runtime_probe",
        "CREATE TEMP TABLE runtime_probe (id int)",
        "ALTER TABLE public.members ADD COLUMN forbidden int",
        "TRUNCATE public.members",
        "ALTER TABLE public.tickets ADD COLUMN forbidden int",
        "TRUNCATE public.tickets",
        "ALTER TABLE public.ticket_transcripts ADD COLUMN forbidden int",
        "TRUNCATE public.ticket_transcripts",
        "SELECT * FROM public._sqlx_migrations",
        "SELECT setval('public.guild_settings_cas_seq', -1)",
        "SELECT public.guild_settings_assign_version()",
    ] {
        denied(pool, &roles[1], sql).await?;
    }
    for sql in [
        "SELECT * FROM public.discord_send_admission",
        "INSERT INTO public.discord_send_admission (token_key) VALUES ('offline')",
        "UPDATE public.discord_send_admission SET in_flight = FALSE",
        "DELETE FROM public.discord_send_admission",
        "SELECT * FROM public.members",
        "INSERT INTO public.members (member_id) VALUES ('test')",
        "SELECT * FROM public.self_role_audit",
        "SELECT * FROM public.self_role_panel_claims",
        "SELECT * FROM public.scheduled_messages",
        SCHEDULED_INSERT,
        "UPDATE public.scheduled_messages SET body = 'reader' WHERE id = 'scheduled-role-probe'",
        "DELETE FROM public.scheduled_messages WHERE id = 'scheduled-role-probe'",
        SCHEDULED_CLAIM,
        "SELECT * FROM public.tickets",
        "INSERT INTO public.tickets (id, guild_id, channel_id, opener_id, status, created_at) VALUES ('reader-probe', 'g', 'c', 'm', 'open', '2026-01-01T00:00:00Z')",
        "SELECT * FROM public.ticket_transcripts",
        "INSERT INTO public.ticket_transcripts (ticket_id, guild_id, channel_id, opener_id, content, message_count, created_at, purge_after) VALUES ('reader-probe', 'g', 'c', 'm', 'probe', 1, '2026-01-01T00:00:00Z', '2026-04-01T00:00:00Z')",
        "CREATE TABLE web_v1.reader_probe (id int)",
        "SELECT nextval('public.guild_settings_version_seq')",
        "SELECT nextval('public.guild_settings_cas_seq')",
        "SELECT last_value FROM public.guild_settings_cas_seq",
        "SELECT setval('public.guild_settings_cas_seq', -1)",
        "SELECT public.guild_settings_assign_version()",
    ] {
        denied(pool, &roles[2], sql).await?;
    }

    // Each drift starts clean, is detected, and is then restored. Effective
    // PUBLIC/column access, attributes, inheritance and future grants all count.
    let (migrator, runtime, reader) = (&roles[0], &roles[1], &roles[2]);
    for (change, restore) in [
        (format!("REVOKE SELECT ON public.discord_send_admission FROM {runtime}"),
         format!("GRANT SELECT ON public.discord_send_admission TO {runtime}")),
        (format!("REVOKE INSERT ON public.discord_send_admission FROM {runtime}"),
         format!("GRANT INSERT ON public.discord_send_admission TO {runtime}")),
        (format!("REVOKE UPDATE ON public.discord_send_admission FROM {runtime}"),
         format!("GRANT UPDATE ON public.discord_send_admission TO {runtime}")),
        (format!("GRANT DELETE ON public.discord_send_admission TO {runtime}"),
         format!("REVOKE DELETE ON public.discord_send_admission FROM {runtime}")),
        (format!("GRANT TRUNCATE ON public.discord_send_admission TO {runtime}"),
         format!("REVOKE TRUNCATE ON public.discord_send_admission FROM {runtime}")),
        ("GRANT SELECT ON public.discord_send_admission TO PUBLIC".to_owned(),
         "REVOKE SELECT ON public.discord_send_admission FROM PUBLIC".to_owned()),
        ("GRANT SELECT (member_id) ON public.members TO PUBLIC".to_owned(),
         "REVOKE SELECT (member_id) ON public.members FROM PUBLIC".to_owned()),
        (format!("GRANT CREATE ON SCHEMA public TO {runtime}"),
         format!("REVOKE CREATE ON SCHEMA public FROM {runtime}")),
        (format!("GRANT {migrator} TO {reader}"), format!("REVOKE {migrator} FROM {reader}")),
        (format!("REVOKE SELECT ON web_v1.members FROM {reader}"),
         format!("GRANT SELECT ON web_v1.members TO {reader}")),
        (format!("REVOKE UPDATE ON public.tickets FROM {runtime}"),
         format!("GRANT UPDATE ON public.tickets TO {runtime}")),
        (format!("REVOKE DELETE ON public.ticket_transcripts FROM {runtime}"),
         format!("GRANT DELETE ON public.ticket_transcripts TO {runtime}")),
        (format!("GRANT SELECT ON public.tickets TO {reader}"),
         format!("REVOKE SELECT ON public.tickets FROM {reader}")),
        (format!("GRANT SELECT ON public.ticket_transcripts TO {reader}"),
         format!("REVOKE SELECT ON public.ticket_transcripts FROM {reader}")),
        (format!("ALTER DEFAULT PRIVILEGES FOR ROLE {migrator} GRANT SELECT ON TABLES TO {reader}"),
         format!("ALTER DEFAULT PRIVILEGES FOR ROLE {migrator} REVOKE SELECT ON TABLES FROM {reader}")),
        (format!("GRANT SELECT ON web_v1.members TO {reader} WITH GRANT OPTION"),
         format!("REVOKE GRANT OPTION FOR SELECT ON web_v1.members FROM {reader}")),
        (format!("ALTER ROLE {runtime} BYPASSRLS"), format!("ALTER ROLE {runtime} NOBYPASSRLS")),
        (format!("GRANT UPDATE ON SEQUENCE public.guild_settings_version_seq TO {runtime}"),
         format!("REVOKE UPDATE ON SEQUENCE public.guild_settings_version_seq FROM {runtime}")),
        ("GRANT EXECUTE ON FUNCTION public.guild_settings_advance_revision() TO PUBLIC".to_owned(),
         "REVOKE EXECUTE ON FUNCTION public.guild_settings_advance_revision() FROM PUBLIC".to_owned()),
        (format!("GRANT SELECT (member_id) ON web_v1.members TO {reader} WITH GRANT OPTION"),
         format!("REVOKE SELECT (member_id) ON web_v1.members FROM {reader}")),
    ] {
        execute(pool, change.clone()).await?;
        require(!findings(pool, roles).await?.is_empty(), &format!("missed drift: {change}"))?;
        execute(pool, restore).await?;
        require(findings(pool, roles).await?.is_empty(), "restored drift remained")?;
    }
    verifier_gap_regressions(pool, roles).await?;
    require(
        findings(pool, roles).await?.is_empty(),
        "restored matrix drifted",
    )?;
    Ok(())
}

async fn exercise_member_grants(pool: &PgPool, roles: &[String]) -> Result<(), sqlx::Error> {
    let (migrator, runtime, reader) = (&roles[0], &roles[1], &roles[2]);
    // Bootstrap above ran 0114 without groups. Also prove an existing deployment
    // is repaired by the additive migration alone, executed by the object owner.
    for table in MEMBER_TABLES {
        execute(pool, format!("REVOKE ALL ON public.{table} FROM {runtime}")).await?;
    }
    execute(
        pool,
        format!(
            "REVOKE ALL ON SEQUENCE public.moderation_member_bans_generation_seq FROM {runtime}"
        ),
    )
    .await?;
    require(
        !findings(pool, roles).await?.is_empty(),
        "missing member grants were not reported",
    )?;
    for _ in 0..2 {
        let mut tx = pool.begin().await?;
        let sql = format!(
            "SET LOCAL ROLE {migrator}; {}",
            isolated(MEMBER_GRANTS, roles)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        require(
            findings(pool, roles).await?.is_empty(),
            "member grant migration drifted",
        )?;
    }
    as_role(pool, runtime, "SELECT nextval('public.moderation_member_bans_generation_seq'); SELECT last_value FROM public.moderation_member_bans_generation_seq").await?;
    for table in MEMBER_TABLES {
        denied(
            pool,
            runtime,
            &format!("ALTER TABLE public.{table} ADD COLUMN forbidden int"),
        )
        .await?;
        denied(pool, runtime, &format!("TRUNCATE public.{table}")).await?;
        denied(pool, runtime, &format!("DROP TABLE public.{table}")).await?;
        denied(pool, reader, &format!("SELECT * FROM public.{table}")).await?;
    }
    // Backup/restore sequence resets and owner-level DDL remain administrative;
    // runtime SELECT is intentionally allowed, not a blanket ban on data export.
    for sql in [
        "SELECT setval('public.moderation_member_bans_generation_seq', 1)",
        "ALTER SEQUENCE public.moderation_member_bans_generation_seq RESTART WITH 1",
        "SELECT * FROM public._sqlx_migrations",
    ] {
        denied(pool, runtime, sql).await?;
    }
    for sql in [
        "SELECT nextval('public.moderation_member_bans_generation_seq')",
        "SELECT last_value FROM public.moderation_member_bans_generation_seq",
        "SELECT setval('public.moderation_member_bans_generation_seq', 1)",
    ] {
        denied(pool, reader, sql).await?;
    }
    denied(pool, runtime, &format!("GRANT {migrator} TO {reader}")).await?;
    denied(
        pool,
        runtime,
        &format!(
            "ALTER DEFAULT PRIVILEGES FOR ROLE {migrator} GRANT SELECT ON TABLES TO {runtime}"
        ),
    )
    .await?;
    // PostgreSQL may warn and no-op, rather than error, when GRANT lacks a
    // grant option. Check the effective result inside the same transaction.
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET LOCAL ROLE {runtime}; GRANT SELECT ON public.moderation_warnings TO {reader}; GRANT USAGE ON SEQUENCE public.moderation_member_bans_generation_seq TO {reader}"
    ))).execute(&mut *tx).await?;
    let cannot_delegate: bool = sqlx::query_scalar(
        "SELECT NOT has_table_privilege($1::text, 'public.moderation_warnings', 'SELECT')
         AND NOT has_sequence_privilege($1::text, 'public.moderation_member_bans_generation_seq', 'USAGE')",
    ).bind(reader).fetch_one(&mut *tx).await?;
    tx.rollback().await?;
    require(cannot_delegate, "runtime delegated member privileges")?;
    // Named sequence coverage survives detachment, and effective PUBLIC/reader
    // privileges or grant options cannot hide behind schema denial.
    for (change, expected) in [
        (format!("REVOKE SELECT ON public.moderation_warnings FROM {runtime}"), "missing table privilege:"),
        (format!("GRANT SELECT ON public.moderation_member_bans TO {runtime} WITH GRANT OPTION"), "unexpected grant option:"),
        (format!("ALTER SEQUENCE public.moderation_member_bans_generation_seq OWNED BY NONE; REVOKE USAGE ON SEQUENCE public.moderation_member_bans_generation_seq FROM {runtime}"), "sequence privilege differs:"),
        (format!("GRANT UPDATE ON SEQUENCE public.moderation_member_bans_generation_seq TO {runtime}"), "sequence privilege differs:"),
        (format!("GRANT USAGE, SELECT ON SEQUENCE public.moderation_member_bans_generation_seq TO {reader}"), "sequence privilege differs:"),
        ("GRANT USAGE ON SEQUENCE public.moderation_member_bans_generation_seq TO PUBLIC".to_owned(), "sequence privilege differs:"),
    ] {
        require(transactional_drift(pool, roles, &change).await?.iter().any(|f| f.starts_with(expected)), &format!("missed member drift: {change}"))?;
    }
    require(
        findings(pool, roles).await?.is_empty(),
        "member probes left drift",
    )
}

fn store_error(_: StoreError) -> sqlx::Error {
    sqlx::Error::InvalidArgument("runtime member store operation failed".to_owned())
}

async fn exercise_member_store(pool: &PgPool, runtime: &str) -> Result<(), sqlx::Error> {
    // Every connection uses the restricted group; store-owned transactions must
    // not accidentally execute through the scratch database's admin pool.
    let setup = format!("SET ROLE {runtime}; SET search_path = public");
    let runtime_pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _| {
            let sql = setup.clone();
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await?;
    let result = async {
        let store = PgMemberModerationStore::new(runtime_pool.clone(), "roles-member");
        let now = "2026-01-01T00:00:00Z";
        let due = "2026-01-01T00:01:00Z";
        require(matches!(store.claim("roles-member", "warn", "warn", "hash", now).await.map_err(store_error)?, ClaimState::Claimed), "runtime did not claim")?;
        store.add_warning("roles-warning", "roles-member", "member", "actor", "probe", "roles-warning", now).await.map_err(store_error)?;
        store.record_audit(&AuditRow {
            request_id: "roles-warning".to_owned(),
            guild_id: "roles-member".to_owned(),
            actor_id: "actor".to_owned(),
            action: "warn",
            target_id: Some("member".to_owned()),
            reason: "probe".to_owned(),
            outcome: "accepted",
            idempotency_key: "warn".to_owned(),
            metadata_json: "{}".to_owned(),
        }).await.map_err(store_error)?;
        store.complete("roles-member", "warn", "accepted", "{}", now).await.map_err(store_error)?;
        require(matches!(store.claim("roles-member", "warn", "warn", "hash", now).await.map_err(store_error)?, ClaimState::Replayed { .. }), "runtime did not replay")?;
        store.claim("roles-member", "release", "warn", "hash", now).await.map_err(store_error)?;
        store.release("roles-member", "release").await.map_err(store_error)?;
        let attempt = store.stage_unban("roles-member", "member", due, "probe", "roles-expiry", now).await.map_err(store_error)?;
        require(attempt.generation > 0, "runtime did not allocate ban generation")?;
        store.confirm_ban_attempt("roles-member", "member", "roles-expiry", attempt, now).await.map_err(store_error)?;
        store.activate_staged_unban("roles-member", "member", "roles-expiry", now).await.map_err(store_error)?;
        let jobs = store.claim_due_unbans("roles-member", due, 1).await.map_err(store_error)?;
        require(jobs.len() == 1, "runtime did not claim expiry")?;
        let job = &jobs[0];
        require(store.owns_unban_claim(&job.request_id, &job.claim_token).await.map_err(store_error)?, "runtime lost expiry claim")?;
        store.requeue_unban(&job.request_id, &job.claim_token).await.map_err(store_error)?;
        let jobs = store.claim_due_unbans("roles-member", due, 1).await.map_err(store_error)?;
        require(jobs.len() == 1, "runtime did not reclaim expiry")?;
        store.complete_unban(&jobs[0].request_id, &jobs[0].claim_token).await.map_err(store_error)?;
        // The store exercised INSERT/SELECT/UPDATE and claim release DELETE.
        // Verify remaining CRUD on every ledger, without weakening its schema.
        sqlx::raw_sql("UPDATE public.moderation_warnings SET reason = 'updated' WHERE guild_id = 'roles-member'; UPDATE public.moderation_audit SET reason = 'updated' WHERE guild_id = 'roles-member'").execute(&runtime_pool).await?;
        for table in MEMBER_TABLES {
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM public.{table} WHERE guild_id = 'roles-member'"))).fetch_one(&runtime_pool).await?;
            require(count == 1, "runtime member ledger CRUD differed")?;
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DELETE FROM public.{table} WHERE guild_id = 'roles-member'"))).execute(&runtime_pool).await?;
        }
        Ok(())
    }.await;
    runtime_pool.close().await;
    result
}

async fn automod_runtime_role_regression(
    pool: &PgPool,
    roles: &[String],
) -> Result<(), sqlx::Error> {
    // The canonical plan is the only source of grants. Exercise claim fencing,
    // ledger upserts and retention under the non-owner runtime, not the admin.
    as_role(
        pool,
        &roles[1],
        r#"INSERT INTO public.automod_delivery_claims
            (guild_id, message_id, delivery_kind, dry_run, request_hash)
            VALUES ('g', 'automod-probe', 'create', false, 'hash')
            ON CONFLICT DO NOTHING RETURNING claim_token;
        SELECT claim_token, mutation_started, counted, released
            FROM public.automod_delivery_claims
            WHERE guild_id = 'g' AND message_id = 'automod-probe' FOR UPDATE;
        INSERT INTO public.automod_processed_messages (guild_id, message_id, user_id, processed_at)
            VALUES ('g', 'automod-probe', 'm', '2026-01-01T00:00:00Z')
            ON CONFLICT (guild_id, message_id) DO NOTHING;
        INSERT INTO public.automod_processed_messages (guild_id, message_id, user_id, processed_at)
            VALUES ('g', 'automod-probe', 'm', '2026-01-01T00:00:00Z')
            ON CONFLICT (guild_id, message_id) DO NOTHING;
        INSERT INTO public.automod_violations
            (guild_id, user_id, violation_count, last_filter, last_message_id, updated_at)
            VALUES ('g', 'm', 1, 'links', 'automod-probe', '2026-01-01T00:00:00Z');
        INSERT INTO public.automod_violations
            (guild_id, user_id, violation_count, last_filter, last_message_id, updated_at)
            VALUES ('g', 'm', 1, 'links', 'automod-probe', '2026-01-01T00:00:00Z')
            ON CONFLICT (guild_id, user_id) DO UPDATE
                SET violation_count = automod_violations.violation_count + 1
            RETURNING violation_count;
        UPDATE public.automod_processed_messages SET processed_at = '2026-01-01T00:00:01Z'
            WHERE guild_id = 'g' AND message_id = 'automod-probe';
        UPDATE public.automod_delivery_claims
            SET counted = true, matched_filter = 'links', matched_guild_id = 'g',
                matched_channel_id = 'c', matched_message_id = 'automod-probe',
                matched_author_id = 'm', released = false, mutation_started = true,
                result_json = '{"outcome":"probe"}', completed_at = CURRENT_TIMESTAMP
            WHERE guild_id = 'g' AND message_id = 'automod-probe';
        DO $automod$
        BEGIN
            IF NOT EXISTS (
                SELECT FROM public.automod_processed_messages p
                JOIN public.automod_violations v ON v.guild_id = p.guild_id AND v.user_id = p.user_id
                JOIN public.automod_delivery_claims c ON c.guild_id = p.guild_id AND c.message_id = p.message_id
                WHERE p.guild_id = 'g' AND p.message_id = 'automod-probe'
                    AND v.violation_count = 2 AND c.counted AND c.mutation_started
                    AND c.matched_author_id = 'm' AND c.result_json IS NOT NULL
            ) THEN
                RAISE EXCEPTION 'runtime claim/ledger DML did not persist';
            END IF;
        END;
        $automod$;
        DELETE FROM public.automod_delivery_claims WHERE guild_id = 'g' AND message_id = 'automod-probe';
        DELETE FROM public.automod_processed_messages WHERE guild_id = 'g' AND message_id = 'automod-probe';
        DELETE FROM public.automod_violations WHERE guild_id = 'g' AND user_id = 'm';"#,
    )
    .await?;
    for table in [
        "automod_violations",
        "automod_processed_messages",
        "automod_delivery_claims",
    ] {
        let relation = format!("public.{table}");
        // Schema denial alone could hide accidental reader grants. Check the
        // effective relation ACL too, including inherited/PUBLIC privileges.
        require(
            sqlx::query_scalar::<_, bool>(
                r#"SELECT has_table_privilege($1::text, $3::text, 'SELECT')
                    AND has_table_privilege($1::text, $3::text, 'INSERT')
                    AND has_table_privilege($1::text, $3::text, 'UPDATE')
                    AND has_table_privilege($1::text, $3::text, 'DELETE')
                    AND NOT has_table_privilege($1::text, $3::text, 'TRUNCATE, REFERENCES, TRIGGER')
                    AND NOT has_table_privilege($2::text, $3::text, 'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')"#,
            )
            .bind(&roles[1])
            .bind(&roles[2])
            .bind(&relation)
            .fetch_one(pool)
            .await?,
            &format!("automod least-privilege ACL differs: {relation}"),
        )?;
        denied(
            pool,
            &roles[1],
            &format!("ALTER TABLE {relation} ADD COLUMN forbidden int"),
        )
        .await?;
        denied(pool, &roles[1], &format!("TRUNCATE {relation}")).await?;
        for sql in [
            format!("SELECT * FROM {relation}"),
            format!("INSERT INTO {relation} DEFAULT VALUES"),
            format!("UPDATE {relation} SET guild_id = 'forbidden'"),
            format!("DELETE FROM {relation}"),
        ] {
            denied(pool, &roles[2], &sql).await?;
        }
    }
    Ok(())
}

async fn verifier_gap_regressions(pool: &PgPool, roles: &[String]) -> Result<(), sqlx::Error> {
    let (migrator, runtime, reader) = (&roles[0], &roles[1], &roles[2]);
    // Catalog probes inspect privileges only; never read password verifiers or files.
    as_role(pool, reader, "SELECT count(*) FROM pg_catalog.pg_class; SELECT count(*) FROM information_schema.columns; SELECT pg_catalog.lower('OK')").await?;
    denied(pool, reader, "SELECT rolname FROM pg_catalog.pg_authid").await?;
    denied(
        pool,
        runtime,
        "SET LOCAL session_replication_role = replica",
    )
    .await?;
    for (change, expected) in [
        (format!("GRANT SET ON PARAMETER session_replication_role TO {runtime}"), "unexpected parameter privilege:"),
        (format!("GRANT ALTER SYSTEM ON PARAMETER session_replication_role TO {migrator}"), "unexpected parameter privilege:"),
        (format!("GRANT SET ON PARAMETER session_replication_role TO {reader} WITH GRANT OPTION"), "unexpected parameter privilege:"),
        (format!("GRANT SET, ALTER SYSTEM ON PARAMETER \"{runtime}.probe\" TO PUBLIC"), "unexpected parameter privilege:"),
        (format!("GRANT SELECT ON pg_catalog.pg_authid TO {reader}"), "unexpected system privilege:"),
        (format!("GRANT SELECT (rolpassword) ON pg_catalog.pg_authid TO {reader}"), "unexpected system privilege:"),
        ("GRANT SELECT ON pg_catalog.pg_authid TO PUBLIC".to_owned(), "unexpected system privilege:"),
        (format!("GRANT EXECUTE ON FUNCTION pg_catalog.pg_read_file(text) TO {reader}"), "unexpected system privilege:"),
        ("GRANT EXECUTE ON FUNCTION pg_catalog.pg_read_file(text) TO PUBLIC".to_owned(), "unexpected system privilege:"),
        (format!("GRANT SELECT ON pg_catalog.pg_class TO {reader} WITH GRANT OPTION"), "unexpected system privilege:"),
        (format!("ALTER FUNCTION pg_catalog.lower(text) OWNER TO {reader}"), "unexpected system owner:"),
        (format!("ALTER SCHEMA information_schema OWNER TO {runtime}"), "unexpected system owner:"),
        ("ALTER SEQUENCE public.events_id_seq OWNED BY NONE; ALTER SEQUENCE public.events_id_seq OWNER TO agent_test".to_owned(), "object kind/owner differs:"),
        (format!("REVOKE SELECT ON public.members FROM {migrator}"), "missing table privilege:"),
        (format!("REVOKE EXECUTE ON FUNCTION web_v1._iso(timestamptz) FROM {migrator}"), "missing function EXECUTE:"),
        (format!("ALTER SEQUENCE public.events_id_seq OWNED BY NONE; REVOKE USAGE, SELECT ON SEQUENCE public.events_id_seq FROM {runtime}"), "sequence privilege differs:"),
        (format!("REVOKE USAGE ON SEQUENCE public.guild_settings_cas_seq FROM {runtime}"), "sequence privilege differs:"),
        (format!("REVOKE SELECT ON SEQUENCE public.guild_settings_cas_seq FROM {runtime}"), "sequence privilege differs:"),
        (format!("GRANT UPDATE ON SEQUENCE public.guild_settings_cas_seq TO {runtime}"), "sequence privilege differs:"),
        (format!("GRANT USAGE, SELECT, UPDATE ON SEQUENCE public.guild_settings_cas_seq TO {reader}"), "sequence privilege differs:"),
        ("GRANT USAGE, SELECT ON SEQUENCE public.guild_settings_cas_seq TO PUBLIC".to_owned(), "sequence privilege differs:"),
        (format!("GRANT EXECUTE ON FUNCTION public.guild_settings_assign_version() TO {runtime}"), "function privilege differs:"),
        ("GRANT EXECUTE ON FUNCTION public.guild_settings_assign_version() TO PUBLIC".to_owned(), "function privilege differs:"),
        ("ALTER FUNCTION public.guild_settings_assign_version() SECURITY DEFINER".to_owned(), "function owner/security differs:"),
    ] {
        let detected = transactional_drift(pool, roles, &change).await?;
        require(detected.iter().any(|f| f.starts_with(expected)), &format!("missed drift: {change}"))?;
        require(findings(pool, roles).await?.is_empty(), "rollback drifted")?;
    }
    for spelling in ["true", "on", "yes", "1", "t", "y", "TRUE", "ON"] {
        let change = format!("ALTER VIEW web_v1.members SET (security_invoker = '{spelling}')");
        let detected = transactional_drift(pool, roles, &change).await?;
        require(
            detected
                .iter()
                .any(|f| f.starts_with("security_invoker view:")),
            &format!("missed boolean: {spelling}"),
        )?;
    }
    for spelling in ["false", "off", "no", "0", "f", "n", "FALSE", "OFF"] {
        let change = format!("ALTER VIEW web_v1.members SET (security_invoker = '{spelling}')");
        require(
            transactional_drift(pool, roles, &change).await?.is_empty(),
            &format!("false boolean drifted: {spelling}"),
        )?;
    }
    // Reapplication must repair revoked owner rights and detached serial sequences.
    execute(pool, format!("REVOKE SELECT ON public.members FROM {migrator}; REVOKE EXECUTE ON FUNCTION web_v1._iso(timestamptz) FROM {migrator}; ALTER SEQUENCE public.events_id_seq OWNED BY NONE; REVOKE USAGE, SELECT ON SEQUENCE public.events_id_seq FROM {runtime}")).await?;
    execute(pool, isolated(&database_roles::plan(), roles)).await?;
    require(
        findings(pool, roles).await?.is_empty(),
        "reapplied plan did not repair required grants",
    )?;
    as_role(pool, reader, "SELECT * FROM web_v1.members").await?;
    as_role(pool, runtime, "INSERT INTO public.events (event_type, guild_id, occurred_at, source, idempotency_key) VALUES ('roles', 'roles', now(), 'roles', 'roles')").await?;
    execute(
        pool,
        "ALTER SEQUENCE public.events_id_seq OWNED BY public.events.id".to_owned(),
    )
    .await?;
    Ok(())
}

async fn transactional_drift(
    pool: &PgPool,
    roles: &[String],
    change: &str,
) -> Result<Vec<String>, sqlx::Error> {
    // Parameter ACLs and PUBLIC grants are transactional too: never leave a
    // cluster-wide test grant behind on an assertion or query failure.
    let mut tx = pool.begin().await?;
    let sql = include_str!("../../../sql/verify_database_roles.sql").replace(
        "-- @matrix",
        include_str!("../../../sql/database_role_matrix.sql"),
    );
    let result = async {
        sqlx::raw_sql(sqlx::AssertSqlSafe(change.to_owned()))
            .execute(&mut *tx)
            .await?;
        sqlx::raw_sql("SET LOCAL search_path = pg_catalog, pg_temp")
            .execute(&mut *tx)
            .await?;
        sqlx::query_scalar(sqlx::AssertSqlSafe(isolated(&sql, roles)))
            .fetch_all(&mut *tx)
            .await
    }
    .await;
    tx.rollback().await?;
    result
}

#[tokio::test]
async fn scratch_database_enforces_roles_and_detects_drift() {
    let url = match std::env::var("TWO_ROLES_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") => TEST_URL.to_owned(),
        Err(_) => return, // Offline suite: explicitly requested and CI tests never skip.
    };
    let options = test_database::test_options(&url).expect("refusing non-test-container target");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let (name, roles) = names();
    execute(&admin, format!("CREATE DATABASE {name}"))
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    let result = exercise(&pool, &roles).await;
    pool.close().await;
    // Clean up even if a privilege probe failed. Only generated owned names.
    execute(&admin, format!("DROP DATABASE {name}"))
        .await
        .unwrap();
    for role in &roles {
        execute(&admin, format!("DROP ROLE IF EXISTS {role}"))
            .await
            .unwrap();
    }
    admin.close().await;
    result.unwrap();
}
