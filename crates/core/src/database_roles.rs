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
        // Enumerate the migrations directory so a new CREATE without a matrix
        // row fails CI. No database connection is needed.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../cutover/migrations");
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("migrations directory")
            .map(|entry| entry.expect("migration entry").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sql"))
            .collect();
        paths.sort();
        assert!(!paths.is_empty(), "no migrations found");
        for path in paths {
            let migration = std::fs::read_to_string(&path).expect("migration file readable");
            let mut table: Option<String> = None;
            for line in migration.lines() {
                if let Some(rest) = line.strip_prefix("CREATE TABLE ") {
                    let rest = rest.strip_prefix("IF NOT EXISTS ").unwrap_or(rest);
                    let raw = rest.split_whitespace().next().unwrap();
                    let raw = raw.strip_prefix("public.").unwrap_or(raw);
                    let name = raw
                        .trim_matches(|c| c == '"' || c == '(')
                        .trim_end_matches('(')
                        .to_owned();
                    let kind = if name == "discord_send_admission" {
                        "admission"
                    } else if name == "member_erasure_audit" || name == "invite_campaigns" {
                        "migrator"
                    } else {
                        "table"
                    };
                    assert!(
                        MATRIX.contains(&format!("'public', '{name}', '{kind}'")),
                        "missing matrix row for {name} ({kind}) from {}",
                        path.display()
                    );
                    table = Some(name);
                } else if let Some(rest) = line.strip_prefix("CREATE SEQUENCE ") {
                    let rest = rest.strip_prefix("IF NOT EXISTS ").unwrap_or(rest);
                    let name = rest.split_whitespace().next().unwrap();
                    assert!(
                        MATRIX.contains(&format!("'public', '{name}', 'sequence'")),
                        "missing matrix sequence {name} from {}",
                        path.display()
                    );
                } else if let Some(rest) = line.strip_prefix("CREATE OR REPLACE FUNCTION ") {
                    let name = rest.split_whitespace().next().unwrap();
                    assert!(
                        MATRIX.contains(&format!("'public', '{name}', 'function'")),
                        "missing matrix function {name} from {}",
                        path.display()
                    );
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
                        let table = table.clone().expect("serial column outside table");
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
