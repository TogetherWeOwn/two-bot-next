//! Print-only provisioning and read-only effective privilege verification.

use sqlx::PgPool;

const MATRIX: &str = include_str!("../../../sql/database_role_matrix.sql");
const PLAN: &str = include_str!("../../../sql/database_roles.sql");
const VERIFY: &str = include_str!("../../../sql/verify_database_roles.sql");

/// No connection or execution: the operator reviews and applies this SQL.
#[must_use]
pub fn plan() -> String {
    PLAN.replace("-- @matrix", MATRIX)
}

/// Returns every drift finding. No migrations, repairs or role changes.
/// Transaction-level read-only and fixed search_path prevent accidental writes
/// or resolution through an audit login's custom search path.
pub async fn verify(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql("SET TRANSACTION READ ONLY; SET LOCAL search_path = pg_catalog, pg_temp;")
        .execute(&mut *tx)
        .await?;
    let sql = VERIFY.replace("-- @matrix", MATRIX);
    let findings = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_plan_has_no_login_or_password_and_uses_one_matrix() {
        let plan = plan();
        assert!(!plan.contains("-- @matrix"));
        assert!(plan.contains(MATRIX));
        assert!(VERIFY.replace("-- @matrix", MATRIX).contains(MATRIX));
        assert!(!plan.contains("PASSWORD"));
        assert!(plan.contains("CREATE ROLE two_bot_runtime NOLOGIN"));
        assert!(plan.starts_with("-- Print-only operator plan."));
        assert!(plan.ends_with("COMMIT;\n"));
    }

    #[test]
    fn matrix_covers_every_migrated_object_and_contract_view() {
        // New tables, sequences and trigger functions must be deliberately
        // included rather than silently receiving wildcard permissions.
        // No database connection is needed.
        for migration in [
            include_str!("../../cutover/migrations/0001_funnel.sql"),
            include_str!("../../cutover/migrations/0002_leveling.sql"),
            include_str!("../../cutover/migrations/0110_moderation_member.sql"),
            include_str!("../../cutover/migrations/0111_moderation_ban_ownership.sql"),
            include_str!("../../cutover/migrations/0112_moderation_legacy_timestamps.sql"),
            include_str!("../../cutover/migrations/0113_moderation_unban_retry_order.sql"),
            include_str!("../../cutover/migrations/0114_moderation_member_runtime_grants.sql"),
            include_str!("../../cutover/migrations/0120_channel_moderation.sql"),
            include_str!("../../cutover/migrations/0140_scheduled_messages.sql"),
            include_str!("../../cutover/migrations/0141_scheduled_messages_legacy_upgrade.sql"),
            include_str!("../../cutover/migrations/0150_sticky_messages.sql"),
            include_str!("../../cutover/migrations/0160_rsvp.sql"),
            include_str!("../../cutover/migrations/0170_lfg.sql"),
            include_str!("../../cutover/migrations/0200_self_roles.sql"),
            include_str!("../../cutover/migrations/0205_self_role_exchange_receipts.sql"),
            include_str!("../../cutover/migrations/0206_self_role_exchange_baselines.sql"),
            include_str!("../../cutover/migrations/0210_tickets.sql"),
            include_str!("../../cutover/migrations/0220_automod.sql"),
            include_str!("../../cutover/migrations/0221_automod_delivery_claims.sql"),
            include_str!("../../cutover/migrations/0222_automod_counted_claim.sql"),
            include_str!("../../cutover/migrations/0223_automod_preserved_match.sql"),
            include_str!("../../cutover/migrations/0224_voice_rooms.sql"),
            include_str!("../../cutover/migrations/0225_voice_inherit_limit.sql"),
            include_str!("../../cutover/migrations/0226_voice_text_channels.sql"),
            include_str!("../../cutover/migrations/0227_voice_access_controls.sql"),
            include_str!("../../cutover/migrations/0300_website_contract.sql"),
            include_str!("../../cutover/migrations/0310_presence_probe.sql"),
            include_str!("../../cutover/migrations/0311_community_scorecard.sql"),
            include_str!("../../cutover/migrations/0312_community_scorecard_attempts.sql"),
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
            include_str!("../../cutover/migrations/0362_gateway_onboarding_jobs.sql"),
        ] {
            let mut table = None;
            for line in migration.lines() {
                if let Some(rest) = line.strip_prefix("CREATE TABLE ") {
                    let rest = rest.strip_prefix("IF NOT EXISTS ").unwrap_or(rest);
                    let name = rest.split_whitespace().next().unwrap();
                    let name = name.strip_prefix("public.").unwrap_or(name);
                    let kind = if name == "discord_send_admission" {
                        "admission"
                    } else {
                        "table"
                    };
                    assert!(MATRIX.contains(&format!("'public', '{name}', '{kind}'")));
                    table = Some(name);
                } else if let Some(rest) = line.strip_prefix("CREATE SEQUENCE ") {
                    let rest = rest.strip_prefix("IF NOT EXISTS ").unwrap_or(rest);
                    let name = rest.split_whitespace().next().unwrap();
                    assert!(MATRIX.contains(&format!("'public', '{name}', 'sequence'")));
                } else if let Some(rest) = line.strip_prefix("CREATE OR REPLACE FUNCTION ") {
                    let name = rest.split_whitespace().next().unwrap();
                    assert!(MATRIX.contains(&format!("'public', '{name}', 'function'")));
                } else {
                    let mut words = line.split_whitespace();
                    if let (Some(column), Some(_)) = (
                        words.next(),
                        words.next().filter(|kind| {
                            ["BIGSERIAL", "SERIAL", "SMALLSERIAL"]
                                .iter()
                                .any(|serial| kind.eq_ignore_ascii_case(serial))
                        }),
                    ) {
                        let table = table.expect("serial column outside table");
                        assert!(MATRIX
                            .contains(&format!("'public', '{table}_{column}_seq', 'sequence'")));
                    }
                }
            }
        }
        for line in include_str!("../../../sql/web_v1.sql").lines() {
            if let Some(rest) = line.strip_prefix("CREATE OR REPLACE VIEW web_v1.") {
                let name = rest.split_whitespace().next().unwrap();
                assert!(MATRIX.contains(&format!("'web_v1', '{name}', 'view'")));
            }
        }
    }
}
