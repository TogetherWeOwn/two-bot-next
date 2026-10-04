//! Print-only provisioning and read-only effective privilege verification.

use sqlx::PgPool;

const MATRIX: &str = include_str!("../../../sql/database_role_matrix.sql");
const PLAN: &str = include_str!("../../../sql/database_roles.sql");
const VERIFY: &str = include_str!("../../../sql/verify_database_roles.sql");

/// Which plan to render. `Full` is the default: every allowlisted object must
/// exist. `Bootstrap` skips relations and sequences absent from the database so
/// a partially migrated database can transfer its existing objects first;
/// functions stay strict in both phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Full,
    Bootstrap,
}

impl std::str::FromStr for Phase {
    type Err = &'static str;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "full" => Ok(Phase::Full),
            "bootstrap" => Ok(Phase::Bootstrap),
            _ => Err("unknown phase: expected \"full\" or \"bootstrap\""),
        }
    }
}

/// Absent non-function object in the default phase: fail closed.
const FULL_ABSENCE: &str = "        IF obj.kind <> 'function' AND to_regclass(format('%I.%I', obj.schema_name, obj.name)) IS NULL THEN\n            RAISE EXCEPTION 'missing relation: %.%', obj.schema_name, obj.name;\n        END IF;";
/// Absent non-function object in the bootstrap phase: skip, transfer the rest.
const BOOTSTRAP_ABSENCE: &str = "        IF obj.kind <> 'function' AND to_regclass(format('%I.%I', obj.schema_name, obj.name)) IS NULL THEN\n            RAISE NOTICE 'skipping absent relation: %.%', obj.schema_name, obj.name;\n            CONTINUE;\n        END IF;";

/// No connection or execution: the operator reviews and applies this SQL.
#[must_use]
pub fn plan() -> String {
    plan_for_phase(Phase::Full)
}

/// Render the plan for one phase. No connection, execution or credentials.
#[must_use]
pub fn plan_for_phase(phase: Phase) -> String {
    let absence = match phase {
        Phase::Full => FULL_ABSENCE,
        Phase::Bootstrap => BOOTSTRAP_ABSENCE,
    };
    PLAN.replace("-- @matrix", MATRIX)
        .replace("-- @absent_relation", absence)
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
    use std::collections::HashSet;

    fn rendered_without_login(sql: &str) {
        assert!(!sql.contains("PASSWORD"));
        assert!(
            !sql.replace("NOLOGIN", "").contains("LOGIN"),
            "plan must create NOLOGIN groups only, never logins"
        );
    }

    #[test]
    fn rendered_plan_has_no_login_or_password_and_uses_one_matrix() {
        for phase in [Phase::Full, Phase::Bootstrap] {
            let rendered = plan_for_phase(phase);
            assert!(!rendered.contains("-- @matrix"));
            assert!(!rendered.contains("-- @absent_relation"));
            assert!(rendered.contains(MATRIX));
            rendered_without_login(&rendered);
            assert!(rendered.contains("CREATE ROLE two_bot_runtime NOLOGIN"));
            assert!(rendered.contains("CREATE ROLE two_bot_migrator_ro NOLOGIN"));
            assert!(rendered.starts_with("-- Print-only operator plan."));
            assert!(rendered.ends_with("COMMIT;\n"));
            // The ephemeral self-grant is present in both phases and revoked
            // before COMMIT.
            assert!(rendered.contains("WITH INHERIT TRUE, SET TRUE"));
            assert!(rendered.contains("REVOKE two_bot_migrator FROM CURRENT_USER;"));
        }
        assert!(VERIFY.replace("-- @matrix", MATRIX).contains(MATRIX));
        assert_eq!(plan(), plan_for_phase(Phase::Full));
    }

    #[test]
    fn bootstrap_phase_diffs_from_full_only_by_skip_lines() {
        let full = plan_for_phase(Phase::Full);
        let bootstrap = plan_for_phase(Phase::Bootstrap);
        let full_lines: HashSet<&str> = full.lines().collect();
        let bootstrap_lines: HashSet<&str> = bootstrap.lines().collect();
        let only_full: Vec<&str> = full
            .lines()
            .filter(|line| !bootstrap_lines.contains(line))
            .collect();
        let only_bootstrap: Vec<&str> = bootstrap
            .lines()
            .filter(|line| !full_lines.contains(line))
            .collect();
        // The default phase fails closed on an absent object; bootstrap skips
        // it and transfers the rest. Nothing else differs.
        assert_eq!(
            only_full,
            ["            RAISE EXCEPTION 'missing relation: %.%', obj.schema_name, obj.name;"]
        );
        assert_eq!(
            only_bootstrap,
            [
                "            RAISE NOTICE 'skipping absent relation: %.%', obj.schema_name, obj.name;",
                "            CONTINUE;",
            ]
        );
    }

    #[test]
    fn phase_names_parse() {
        assert_eq!("full".parse(), Ok(Phase::Full));
        assert_eq!("bootstrap".parse(), Ok(Phase::Bootstrap));
        assert!("Bootstrap".parse::<Phase>().is_err());
        assert!("".parse::<Phase>().is_err());
    }

    #[test]
    fn matrix_covers_every_migrated_object_and_contract_view() {
        // New tables, sequences and trigger functions must be deliberately
        // included rather than silently receiving wildcard permissions.
        // Enumerate the migrations directory so a new CREATE without a matrix
        // row fails CI. No database connection is needed. Our 0130 custom
        // commands migration is covered automatically: the file lives in-tree.
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
