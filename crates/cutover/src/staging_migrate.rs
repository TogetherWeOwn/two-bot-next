//! Staging-only SQLx migration runner (TOG-11572).
//!
//! Applies this crate's embedded migrations through the same SQLx migrator the
//! gateway used before it went DML-only, with ledger `public._sqlx_migrations`.
//! Unlike `sqlx migrate run` plus a separate `psql` session, every pooled
//! connection runs `SET ROLE two_bot_migrator` in `after_connect`, so each
//! connection that executes DDL is proven to hold the migrator group.
//!
//! Fail-closed: every refusal happens before any DDL. The runner never resets,
//! reverts, restores, creates roles, grants privileges or reads other
//! credentials. The database URL arrives only through the fixed environment
//! binding [`URL_ENV`] and is never printed.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use serde_json::{json, Value};
use sha2::{Digest, Sha384};
use sqlx::{
    migrate::Migrator,
    postgres::{PgConnection, PgPoolOptions},
    Executor, PgPool, Row,
};

/// Fixed binding name; the value is a secret and is never echoed.
pub const URL_ENV: &str = "TWO_BOT_STAGING_MIGRATOR_DATABASE_URL";
pub const MIGRATOR_ROLE: &str = "two_bot_migrator";
/// SQLx library version this runner is pinned to (asserted against Cargo.lock).
pub const SQLX_VERSION: &str = "0.9.0";
pub const RUNNER_VERSION: u32 = 1;

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// A prerequisite is missing or unsafe; no DDL was attempted.
    #[error("refused: {0}")]
    Refused(String),
    /// Migration execution or verification failed; ledger evidence is attached.
    #[error("failed: {0}")]
    Failed(String, Value),
}

fn refuse<T>(message: impl Into<String>) -> Result<T, RunError> {
    Err(RunError::Refused(message.into()))
}

#[derive(Debug, Clone)]
pub struct Request {
    pub url: Option<String>,
    pub source_sha: String,
    pub expected_host: String,
    pub expected_database: String,
    pub recovery_evidence_ref: String,
    pub acl_plan_ref: String,
    pub apply: bool,
    /// Raw `expected_pending` workflow input (ascending, comma-separated
    /// versions). Required for apply; ignored for plan.
    pub expected_pending: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    pub version: i64,
    pub description: String,
    pub success: bool,
    pub checksum_hex: String,
}

fn is_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !value.contains("://")
        && !value.contains('@')
        && !value.chars().any(char::is_whitespace)
}

/// Parse the `expected_pending` workflow input: ascending, comma-separated
/// versions. Empty input means no pending work is expected.
pub fn parse_expected_pending(raw: &str) -> Result<Vec<i64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        let version: i64 = part
            .parse()
            .map_err(|_| format!("expected_pending entry {part:?} is not a version"))?;
        if out.last().is_some_and(|&prev| prev >= version) {
            return Err("expected_pending must be strictly ascending".to_owned());
        }
        out.push(version);
    }
    Ok(out)
}

/// Pure prerequisite checks; nothing here touches the network.
///
/// Pinned-identity model: the workflow pins the exact non-secret staging
/// endpoint host and database name per dispatch, and [`verify_target`]
/// refuses before any DDL unless the secret migrator binding points at
/// exactly that pinned identity. A `staging` substring in the database name
/// remains accepted but is no longer required, so the verified shared-Neon
/// staging database (which cannot carry `staging` in its name) can be
/// targeted. Production exclusion is fail-closed: any `prod`-like host or
/// database pin is refused, and no production Neon endpoint is recorded in
/// `docs/cutover.md` or `docs/production-deploy.md` as of this change.
pub fn validate_request(req: &Request) -> Result<(), RunError> {
    let sha = &req.source_sha;
    if sha.len() != 40 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return refuse("source SHA must be a full 40-char lowercase hex commit");
    }
    for (label, value) in [
        ("expected host", &req.expected_host),
        ("expected database", &req.expected_database),
    ] {
        let lower = value.to_ascii_lowercase();
        if lower.is_empty() || lower.contains("prod") || lower.contains('@') || lower.contains('/')
        {
            return refuse(format!(
                "{label} is empty, production-like or not a bare name"
            ));
        }
    }
    // Session SET ROLE and the SQLx advisory lock are unsound behind
    // transaction pooling: only the direct endpoint may be targeted.
    if req.expected_host.to_ascii_lowercase().contains("-pooler") {
        return refuse("pooler endpoints are refused; use the direct endpoint");
    }
    match &req.expected_pending {
        Some(raw) => {
            parse_expected_pending(raw).map_err(RunError::Refused)?;
        }
        None if req.apply => {
            return refuse("apply requires expected_pending from the reviewed plan");
        }
        None => {}
    }
    // NOTE: no `staging`-substring requirement here. The pinned host plus the
    // binding-match check in `verify_target` is the staging identity; the
    // database name alone cannot prove it.
    if !is_ref(&req.recovery_evidence_ref) {
        return refuse("approved recovery evidence reference is missing or not a bare reference");
    }
    if !is_ref(&req.acl_plan_ref) {
        return refuse("reviewed ACL plan reference is missing or not a bare reference");
    }
    if req.url.as_deref().is_none_or(str::is_empty) {
        return refuse(format!("migrator binding {URL_ENV} is not provided"));
    }
    Ok(())
}

fn verify_target(req: &Request) -> Result<sqlx::postgres::PgConnectOptions, RunError> {
    let url = req.url.as_deref().unwrap_or_default();
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return refuse("migrator binding is not a Postgres URL");
    }
    two_bot_core::database_url::validate(url).map_err(|m| RunError::Refused(m.to_owned()))?;
    let options = two_bot_core::database_url::connect_options(url).map_err(|_| {
        RunError::Refused("migrator binding is not a valid database URL".to_owned())
    })?;
    let host = options.get_host().to_ascii_lowercase();
    let database = options.get_database().unwrap_or_default();
    if host.contains("-pooler") {
        return refuse("pooler endpoints are refused; use the direct endpoint");
    }
    if host != req.expected_host.to_ascii_lowercase() || database != req.expected_database {
        return refuse("migrator binding target does not match the verified staging identity");
    }
    Ok(options)
}

/// Compare the full ledger with the embedded migrations. Returns the source
/// versions absent from the ledger, in source order, or the first reason SQL
/// must not run. Set-based: a ledger may lag the source by any subset, because
/// SQLx itself applies every unapplied version regardless of the ledger max;
/// only failed rows, versions unknown to the source, and checksum drift refuse.
pub fn reconcile(ledger: &[LedgerRow], migrator: &Migrator) -> Result<Vec<i64>, String> {
    reconcile_against(ledger, &known_checksums(migrator))
}

fn known_checksums(migrator: &Migrator) -> Vec<(i64, String)> {
    migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .map(|m| (m.version, hex::encode(&m.checksum)))
        .collect()
}

fn reconcile_against(ledger: &[LedgerRow], known: &[(i64, String)]) -> Result<Vec<i64>, String> {
    use std::collections::{HashMap, HashSet};
    let checksums: HashMap<i64, &String> = known
        .iter()
        .map(|(version, checksum)| (*version, checksum))
        .collect();
    for row in ledger {
        if !row.success {
            return Err(format!(
                "ledger version {} is failed/incomplete",
                row.version
            ));
        }
        match checksums.get(&row.version) {
            None => {
                return Err(format!(
                    "ledger version {} is unknown to this source",
                    row.version
                ));
            }
            Some(checksum) if row.checksum_hex != checksum.as_str() => {
                return Err(format!("checksum drift at version {}", row.version));
            }
            Some(_) => {}
        }
    }
    let present: HashSet<i64> = ledger.iter().map(|row| row.version).collect();
    Ok(known
        .iter()
        .filter(|(version, _)| !present.contains(version))
        .map(|(version, _)| *version)
        .collect())
}

async fn read_ledger(conn: &mut PgConnection) -> Result<Vec<LedgerRow>, sqlx::Error> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !exists {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT version, description, success, encode(checksum, 'hex') AS checksum \
         FROM public._sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .iter()
        .map(|r| LedgerRow {
            version: r.get("version"),
            description: r.get("description"),
            success: r.get("success"),
            checksum_hex: r.get("checksum"),
        })
        .collect())
}

fn ledger_json(rows: &[LedgerRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|r| {
                json!({"version": r.version, "description": r.description,
                       "success": r.success, "sha384": r.checksum_hex})
            })
            .collect(),
    )
}

fn source_manifest(migrator: &Migrator) -> Value {
    Value::Array(
        migrator
            .iter()
            .filter(|m| !m.migration_type.is_down_migration())
            .map(|m| {
                // Recompute from SQL so the manifest never trusts the stored value.
                let computed = hex::encode(Sha384::digest(m.sql.as_str().as_bytes()));
                json!({"version": m.version, "description": m.description,
                       "sha384": hex::encode(&m.checksum), "sha384_recomputed": computed})
            })
            .collect(),
    )
}

async fn connect(
    options: sqlx::postgres::PgConnectOptions,
    verified: Arc<AtomicUsize>,
) -> Result<PgPool, RunError> {
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            let verified = Arc::clone(&verified);
            Box::pin(async move {
                // Per-connection: a separate psql SET ROLE cannot activate this.
                conn.execute("SET ROLE two_bot_migrator").await?;
                conn.execute("SET search_path = public").await?;
                let current: String = sqlx::query_scalar("SELECT current_user::text")
                    .fetch_one(&mut *conn)
                    .await?;
                if current != MIGRATOR_ROLE {
                    return Err(sqlx::Error::Protocol("migrator role not active".to_owned()));
                }
                verified.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .map_err(|_| {
            RunError::Refused(
                "could not connect and SET ROLE two_bot_migrator (missing binding or membership)"
                    .to_owned(),
            )
        })
}

/// Run the plan (read-only) or apply. Returns the sanitized manifest.
pub async fn run(req: &Request) -> Result<Value, RunError> {
    validate_request(req)?;
    let options = verify_target(req)?;
    let verified = Arc::new(AtomicUsize::new(0));
    let pool = connect(options, Arc::clone(&verified)).await?;
    let result = run_on_pool(req, &pool, &verified).await;
    pool.close().await;
    result
}

async fn run_on_pool(
    req: &Request,
    pool: &PgPool,
    verified: &AtomicUsize,
) -> Result<Value, RunError> {
    let failed = |m: &str| RunError::Failed(m.to_owned(), Value::Null);
    let mut conn = pool
        .acquire()
        .await
        .map_err(|_| failed("pool acquire failed"))?;
    let before = read_ledger(&mut conn)
        .await
        .map_err(|_| failed("ledger read failed"))?;
    drop(conn);
    let pending = reconcile(&before, &MIGRATOR).map_err(RunError::Refused)?;
    // Plan binding: apply runs only the exact pending list the reviewed plan
    // showed. Validated in `validate_request`; compared here, before any DDL.
    let expected = match &req.expected_pending {
        Some(raw) => parse_expected_pending(raw).map_err(RunError::Refused)?,
        None => Vec::new(),
    };
    if req.apply && expected != pending {
        return refuse("expected_pending does not match the computed pending list");
    }

    let mut applied = 0usize;
    let mut after = before.clone();
    if req.apply && !pending.is_empty() {
        let outcome = MIGRATOR.run(pool).await;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|_| failed("pool acquire failed"))?;
        after = read_ledger(&mut conn).await.unwrap_or_default();
        drop(conn);
        if let Err(e) = outcome {
            let evidence = json!({"ledger_after_failure": ledger_json(&after),
                                  "error": e.to_string()});
            return Err(RunError::Failed(
                "migration execution failed".to_owned(),
                evidence,
            ));
        }
        applied = after.len().saturating_sub(before.len());
        let leftover = reconcile(&after, &MIGRATOR)
            .map_err(|m| RunError::Failed(m, json!({"ledger_after": ledger_json(&after)})))?;
        if !leftover.is_empty() {
            return Err(RunError::Failed(
                "ledger incomplete after apply".to_owned(),
                json!({"ledger_after": ledger_json(&after)}),
            ));
        }
        let owner: String = sqlx::query_scalar(
            "SELECT pg_get_userbyid(relowner)::text FROM pg_class \
             WHERE oid = to_regclass('public._sqlx_migrations')",
        )
        .fetch_one(pool)
        .await
        .map_err(|_| failed("ledger owner check failed"))?;
        if owner != MIGRATOR_ROLE {
            return Err(failed("ledger is not owned by two_bot_migrator"));
        }
    }
    Ok(json!({
        "runner_version": RUNNER_VERSION,
        "tool": {"name": "two-bot-cutover staging_migrate", "crate_version": env!("CARGO_PKG_VERSION"),
                 "sqlx": SQLX_VERSION, "invocation": "staging-migrate --plan|--apply"},
        "mode": if req.apply { "apply" } else { "plan" },
        "source_sha": req.source_sha,
        "target": {"host": req.expected_host, "database": req.expected_database},
        "recovery_evidence_ref": req.recovery_evidence_ref,
        "acl_plan_ref": req.acl_plan_ref,
        "role": MIGRATOR_ROLE,
        "role_verified_connections": verified.load(Ordering::SeqCst),
        "source_migrations": source_manifest(&MIGRATOR),
        "ledger_before": ledger_json(&before),
        "ledger_after": ledger_json(&after),
        "pending_before": pending,
        "expected_pending": expected,
        "applied_count": applied,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<LedgerRow> {
        MIGRATOR
            .iter()
            .map(|m| LedgerRow {
                version: m.version,
                description: m.description.to_string(),
                success: true,
                checksum_hex: hex::encode(&m.checksum),
            })
            .collect()
    }

    #[test]
    fn sqlx_pin_matches_lockfile() {
        let lock = include_str!("../../../Cargo.lock");
        assert!(lock.contains(&format!("name = \"sqlx\"\nversion = \"{SQLX_VERSION}\"")));
    }

    #[test]
    fn reconcile_cases() {
        let all = rows();
        assert_eq!(reconcile(&[], &MIGRATOR).unwrap().len(), all.len());
        assert!(reconcile(&all, &MIGRATOR).unwrap().is_empty());
        assert_eq!(
            reconcile(&all[..2], &MIGRATOR).unwrap().len(),
            all.len() - 2
        );
        // Set-based: a hole in the middle is filled, not refused.
        let gap = vec![all[0].clone(), all[2].clone()];
        let pending = reconcile(&gap, &MIGRATOR).unwrap();
        assert!(pending.contains(&all[1].version));
        assert_eq!(pending.len(), all.len() - 2);
        // Refusals survive anywhere in the ledger, not just at the edges.
        let mut drift = all.clone();
        drift[1].checksum_hex = "00".repeat(48);
        assert!(reconcile(&drift, &MIGRATOR).unwrap_err().contains("drift"));
        let mut bad = all.clone();
        bad[0].success = false;
        assert!(reconcile(&bad, &MIGRATOR)
            .unwrap_err()
            .contains("incomplete"));
        let mut mid_bad = vec![all[0].clone(), all[1].clone(), all[2].clone()];
        mid_bad[1].success = false;
        assert!(reconcile(&mid_bad, &MIGRATOR)
            .unwrap_err()
            .contains("incomplete"));
        let mut unknown = all.clone();
        unknown.push(LedgerRow {
            version: 99999,
            ..all[0].clone()
        });
        assert!(reconcile(&unknown, &MIGRATOR)
            .unwrap_err()
            .contains("unknown"));
        let mut mid_unknown = vec![all[0].clone(), all[1].clone()];
        mid_unknown.insert(
            1,
            LedgerRow {
                version: 99998,
                ..all[0].clone()
            },
        );
        assert!(reconcile(&mid_unknown, &MIGRATOR)
            .unwrap_err()
            .contains("unknown"));
        let known = known_checksums(&MIGRATOR);
        let mut mid_drift = vec![all[0].clone(), all[1].clone(), all[2].clone()];
        mid_drift[2].checksum_hex = "ff".repeat(48);
        assert!(reconcile_against(&mid_drift, &known)
            .unwrap_err()
            .contains("drift"));
    }

    /// Exact staging ledger at the base SHA: 29 ledger rows must yield the 24
    /// pending versions below, in source order. Later source versions stay
    /// pending too, so the live mapping is asserted as a subsequence.
    const STAGING_LEDGER: [i64; 29] = [
        1, 2, 120, 121, 122, 123, 140, 141, 150, 160, 170, 180, 190, 200, 210, 300, 310, 311, 320,
        330, 331, 332, 333, 334, 340, 350, 351, 360, 390,
    ];
    const STAGING_PENDING_AT_BASE: [i64; 24] = [
        201, 202, 203, 204, 205, 206, 220, 221, 222, 223, 224, 225, 226, 227, 228, 312, 321, 352,
        353, 361, 362, 370, 410, 411,
    ];

    #[test]
    fn staging_ledger_fixture_is_set_based() {
        use std::collections::{HashMap, HashSet};
        let checksums: HashMap<i64, String> = MIGRATOR
            .iter()
            .map(|m| (m.version, hex::encode(&m.checksum)))
            .collect();
        let ledger: Vec<LedgerRow> = STAGING_LEDGER
            .iter()
            .map(|version| LedgerRow {
                version: *version,
                description: format!("fixture {version}"),
                success: true,
                checksum_hex: checksums[version].clone(),
            })
            .collect();
        // Base source set: the 29 ledger rows plus the 24 pending versions.
        let base: HashSet<i64> = STAGING_LEDGER
            .iter()
            .chain(STAGING_PENDING_AT_BASE.iter())
            .copied()
            .collect();
        assert_eq!(base.len(), 53);
        let known_base: Vec<(i64, String)> = MIGRATOR
            .iter()
            .filter(|m| base.contains(&m.version))
            .map(|m| (m.version, hex::encode(&m.checksum)))
            .collect();
        assert_eq!(
            known_base.len(),
            53,
            "every base version must still exist at this head"
        );
        assert_eq!(
            reconcile_against(&ledger, &known_base).unwrap(),
            STAGING_PENDING_AT_BASE
        );
        // The live source may have grown past the base; the base mapping must
        // survive unchanged inside it, in source order.
        let live = reconcile(&ledger, &MIGRATOR).unwrap();
        let live_base: Vec<i64> = live
            .iter()
            .copied()
            .filter(|version| base.contains(version))
            .collect();
        assert_eq!(live_base, STAGING_PENDING_AT_BASE);
        for version in STAGING_PENDING_AT_BASE {
            assert!(live.contains(&version));
        }
    }

    #[test]
    fn expected_pending_parses_ascending() {
        assert_eq!(parse_expected_pending("").unwrap(), Vec::<i64>::new());
        assert_eq!(parse_expected_pending("  ").unwrap(), Vec::<i64>::new());
        assert_eq!(
            parse_expected_pending("201,202, 410").unwrap(),
            vec![201, 202, 410]
        );
        assert!(parse_expected_pending("201,abc")
            .unwrap_err()
            .contains("not a version"));
        assert!(parse_expected_pending("202,201")
            .unwrap_err()
            .contains("ascending"));
        assert!(parse_expected_pending("201,201")
            .unwrap_err()
            .contains("ascending"));
    }

    #[test]
    fn validation_refuses_before_connecting() {
        let ok = Request {
            url: Some("postgres://u@agent-testdb:5432/two_staging".to_owned()),
            source_sha: "a".repeat(40),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_staging".to_owned(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
        };
        assert!(validate_request(&ok).is_ok());
        // The verified shared-Neon staging identity needs no `staging` in the
        // database name: a pinned non-prod host plus an explicit database pin
        // validates, and the binding-match check owns the rest.
        let neon_pin = Request {
            url: Some(
                "postgres://u@ep-staging-example.us-east-2.aws.neon.tech:5432/two_bot?sslmode=require"
                    .to_owned(),
            ),
            expected_host: "ep-staging-example.us-east-2.aws.neon.tech".to_owned(),
            expected_database: "two_bot".to_owned(),
            ..ok.clone()
        };
        assert!(validate_request(&neon_pin).is_ok());
        let bad = [
            Request {
                url: None,
                ..ok.clone()
            },
            Request {
                source_sha: "main".to_owned(),
                ..ok.clone()
            },
            Request {
                expected_database: "two_prod".to_owned(),
                ..ok.clone()
            },
            Request {
                expected_host: "ep-prod-example.us-east-2.aws.neon.tech".to_owned(),
                ..ok.clone()
            },
            Request {
                expected_host: String::new(),
                ..ok.clone()
            },
            Request {
                expected_database: String::new(),
                ..ok.clone()
            },
            Request {
                recovery_evidence_ref: String::new(),
                ..ok.clone()
            },
            Request {
                acl_plan_ref: "postgres://x:y@h/d".to_owned(),
                ..ok.clone()
            },
            Request {
                expected_host: "ep-test-pooler.us-east-2.aws.neon.tech".to_owned(),
                ..ok.clone()
            },
            Request {
                apply: true,
                expected_pending: None,
                ..ok.clone()
            },
            Request {
                apply: true,
                expected_pending: Some("201,abc".to_owned()),
                ..ok.clone()
            },
            Request {
                apply: true,
                expected_pending: Some("202,201".to_owned()),
                ..ok.clone()
            },
        ];
        for r in bad {
            assert!(matches!(validate_request(&r), Err(RunError::Refused(_))));
        }
        // Apply accepts a well-formed binding when given one.
        assert!(validate_request(&Request {
            apply: true,
            expected_pending: Some(String::new()),
            ..ok.clone()
        })
        .is_ok());
    }

    #[test]
    fn binding_must_match_pinned_identity() {
        let base = Request {
            url: Some("postgres://u@agent-testdb:5432/two_bot?sslmode=disable".to_owned()),
            source_sha: "a".repeat(40),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_bot".to_owned(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
        };
        assert!(validate_request(&base).is_ok());
        assert!(verify_target(&base).is_ok());
        for pin in [
            Request {
                expected_host: "other-host".to_owned(),
                ..base.clone()
            },
            Request {
                expected_database: "two_bot_other".to_owned(),
                ..base.clone()
            },
        ] {
            assert!(matches!(verify_target(&pin), Err(RunError::Refused(_))));
        }
        // A pooler binding is refused even when the pins match it.
        let pooler = Request {
            url: Some(
                "postgres://u@ep-test-pooler.us-east-2.aws.neon.tech:5432/two_bot?sslmode=require"
                    .to_owned(),
            ),
            expected_host: "ep-test-pooler.us-east-2.aws.neon.tech".to_owned(),
            ..base.clone()
        };
        assert!(matches!(
            validate_request(&pooler),
            Err(RunError::Refused(_))
        ));
        assert!(matches!(verify_target(&pooler), Err(RunError::Refused(_))));
    }
}
