//! Staging-only SQLx migration runner (TOG-11572).
//!
//! Applies this crate's embedded migrations through the same SQLx migrator the
//! gateway used before it went DML-only, with ledger `public._sqlx_migrations`.
//! Every pooled connection runs `SET ROLE` in `after_connect` and verifies
//! `current_user`: plan connections assume the read-only [`READ_ONLY_ROLE`]
//! group (physically incapable of DDL), while apply connections assume the
//! [`MIGRATOR_ROLE`] group, so each connection that executes DDL is proven to
//! hold the migrator group.
//!
//! Fail-closed: every refusal happens before any DDL. The runner never resets,
//! reverts, restores, creates roles, grants privileges or reads other
//! credentials. The database URL arrives only through the mode's fixed
//! environment binding ([`PLAN_URL_ENV`] for plan, [`URL_ENV`] for apply) and
//! is never printed. Plan refuses when the RO binding is absent, even when a
//! migrator URL is set elsewhere.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha384};
use sqlx::{
    migrate::Migrator,
    postgres::{PgConnection, PgPoolOptions},
    Executor, PgPool, Row,
};

/// Fixed binding names; each value is a secret and is never echoed.
/// Plan reads only the RO binding; apply reads only the migrator binding.
pub const URL_ENV: &str = "TWO_BOT_STAGING_MIGRATOR_DATABASE_URL";
pub const PLAN_URL_ENV: &str = "TWO_BOT_STAGING_PLAN_DATABASE_URL";
pub const MIGRATOR_ROLE: &str = "two_bot_migrator";
/// Read-only migration-plan identity: SELECT on bot tables, the admission
/// lane and the ledger; no DML, DDL, sequence, function or web-view access
/// (rendered by `sql/database_roles.sql`, verified by
/// `crates/core/tests/database_roles.rs`).
pub const READ_ONLY_ROLE: &str = "two_bot_migrator_ro";
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
    /// Plan-bound apply identity: the SHA-256 of the plan job's uploaded
    /// `staging-migrate-manifest.json` and the run that produced it. Both are
    /// required for apply; both are ignored for plan.
    pub plan_manifest_sha256: Option<String>,
    pub plan_run_id: Option<String>,
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

/// SHA-256 of a plan manifest's canonical projection.
///
/// The projection is `source_sha`, the pending list and the full source
/// migration table, each on its own line. The workflow computes this over
/// the plan job's uploaded `staging-migrate-manifest.json` and passes it to
/// the apply dispatch as `plan_manifest_sha256`; the apply runner recomputes
/// it over the manifest it just printed and refuses on any mismatch. The
/// source table binds the hash to the exact migration SQL the plan showed,
/// so same-pending-different-SQL replays refuse.
pub fn manifest_hash(source_sha: &str, pending: &[i64], migrator: &Migrator) -> String {
    let mut projection = String::from(source_sha);
    projection.push('\n');
    for version in pending {
        projection.push_str(&version.to_string());
        projection.push('\n');
    }
    for m in migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
    {
        projection.push_str(&format!(
            "{}:{}:{}\n",
            m.version,
            m.description,
            hex::encode(&m.checksum)
        ));
    }
    hex::encode(Sha256::digest(projection.as_bytes()))
}

/// Plan-bound apply identity, checked before any DDL.
///
/// Returns the computed manifest hash. For apply, the request's
/// `plan_manifest_sha256` must equal the hash recomputed over this run's own
/// source SHA, pending list and migration table; a hash minted for a
/// different source SHA, a different pending set or different migration SQL
/// refuses. For plan the hash is computed and returned for the manifest.
pub fn check_plan_binding(
    req: &Request,
    pending: &[i64],
    migrator: &Migrator,
) -> Result<String, RunError> {
    let computed = manifest_hash(&req.source_sha, pending, migrator);
    if req.apply {
        let want = req
            .plan_manifest_sha256
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if computed != want {
            return refuse("plan_manifest_sha256 does not match the computed plan manifest");
        }
    }
    Ok(computed)
}

/// Pure prerequisite checks; nothing here touches the network.
///
/// Pinned-identity model: the workflow pins the exact non-secret staging
/// endpoint host and database name per dispatch, and `verify_target`
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
    // Plan-bound apply identity: apply runs only the exact manifest the
    // reviewed plan produced. Both the hash and the producing run id are
    // required for apply; the hash check itself happens in `run_on_pool`,
    // after the manifest is computed but before any DDL.
    match (&req.plan_manifest_sha256, &req.plan_run_id) {
        (Some(hash), Some(run_id)) => {
            let hash = hash.trim().to_ascii_lowercase();
            let run_id = run_id.trim();
            if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return refuse("plan_manifest_sha256 must be a 64-char lowercase hex digest");
            }
            if run_id.is_empty() || run_id.len() > 20 || !run_id.bytes().all(|b| b.is_ascii_digit())
            {
                return refuse("plan_run_id must be the numeric run id that produced the plan");
            }
        }
        _ if req.apply => {
            return refuse(
                "apply requires plan_manifest_sha256 and plan_run_id from the reviewed plan",
            );
        }
        _ => {}
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
        return refuse(format!(
            "{} binding {} is not provided",
            if req.apply { "migrator" } else { "plan" },
            binding_env(req),
        ));
    }
    Ok(())
}

/// The fixed environment binding for this mode: plan reads only the
/// read-only binding, apply reads only the migrator binding. The CLI sets
/// `Request.url` from exactly this binding, so a mode can never borrow the
/// other mode's credential; `validate_request` refuses an absent binding
/// before any connection is attempted.
fn binding_env(req: &Request) -> &'static str {
    if req.apply {
        URL_ENV
    } else {
        PLAN_URL_ENV
    }
}

/// The database group this mode assumes per connection: plan is physically
/// read-only, apply holds the migrator group that owns DDL.
fn expected_role(req: &Request) -> &'static str {
    if req.apply {
        MIGRATOR_ROLE
    } else {
        READ_ONLY_ROLE
    }
}

fn verify_target(req: &Request) -> Result<sqlx::postgres::PgConnectOptions, RunError> {
    let binding = binding_env(req);
    let url = req.url.as_deref().unwrap_or_default();
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return refuse(format!("{binding} value is not a Postgres URL"));
    }
    two_bot_core::database_url::validate(url).map_err(|m| RunError::Refused(m.to_owned()))?;
    let options = two_bot_core::database_url::connect_options(url)
        .map_err(|_| RunError::Refused(format!("{binding} value is not a valid database URL")))?;
    let host = options.get_host().to_ascii_lowercase();
    let database = options.get_database().unwrap_or_default();
    if host.contains("-pooler") {
        return refuse("pooler endpoints are refused; use the direct endpoint");
    }
    if host != req.expected_host.to_ascii_lowercase() || database != req.expected_database {
        return refuse(format!(
            "{binding} target does not match the verified staging identity"
        ));
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
    role: &'static str,
    verified: Arc<AtomicUsize>,
) -> Result<PgPool, RunError> {
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            let verified = Arc::clone(&verified);
            Box::pin(async move {
                // Per-connection: a separate psql SET ROLE cannot activate this.
                // `role` is one of the two fixed group constants, never input.
                sqlx::query(sqlx::AssertSqlSafe(format!("SET ROLE {role}")))
                    .execute(&mut *conn)
                    .await?;
                conn.execute("SET search_path = public").await?;
                let current: String = sqlx::query_scalar("SELECT current_user::text")
                    .fetch_one(&mut *conn)
                    .await?;
                if current != role {
                    return Err(sqlx::Error::Protocol(format!(
                        "expected role {role} is not active"
                    )));
                }
                verified.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .map_err(|_| {
            RunError::Refused(format!(
                "could not connect and SET ROLE {role} (missing binding or membership)"
            ))
        })
}

/// Run the plan (read-only, RO binding and role) or apply (migrator binding
/// and role). Returns the sanitized manifest.
pub async fn run(req: &Request) -> Result<Value, RunError> {
    validate_request(req)?;
    let options = verify_target(req)?;
    let verified = Arc::new(AtomicUsize::new(0));
    let role = expected_role(req);
    let pool = connect(options, role, Arc::clone(&verified)).await?;
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
    let computed_hash = check_plan_binding(req, &pending, &MIGRATOR)?;

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
        "role": expected_role(req),
        "role_verified_connections": verified.load(Ordering::SeqCst),
        "source_migrations": source_manifest(&MIGRATOR),
        "ledger_before": ledger_json(&before),
        "ledger_after": ledger_json(&after),
        "pending_before": pending,
        "expected_pending": expected,
        "plan_manifest_sha256": computed_hash,
        "plan_run_id": req.plan_run_id.clone().unwrap_or_default(),
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

    /// Plan-bound apply identity against the live migration table, so the
    /// three hash cases below need no database.
    fn pending_versions() -> Vec<i64> {
        reconcile(&[], &MIGRATOR).expect("empty ledger leaves every version pending")
    }

    /// An apply request carrying the exact hash `check_plan_binding`
    /// computes for its own source SHA, pending list and migration table.
    fn bound_apply(pending: &[i64], source_sha: &str) -> Request {
        let hash = manifest_hash(source_sha, pending, &MIGRATOR);
        Request {
            url: Some("postgres://u@agent-testdb:5432/two_staging".to_owned()),
            source_sha: source_sha.to_owned(),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_staging".to_owned(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: true,
            expected_pending: Some(
                pending
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            plan_manifest_sha256: Some(hash),
            plan_run_id: Some("123456789".to_owned()),
        }
    }

    #[test]
    fn plan_binding_accepts_the_matching_hash() {
        let sha = "a".repeat(40);
        let pending = pending_versions();
        let req = bound_apply(&pending, &sha);
        assert!(validate_request(&req).is_ok());
        assert_eq!(
            check_plan_binding(&req, &pending, &MIGRATOR).unwrap(),
            req.plan_manifest_sha256.unwrap()
        );
    }

    #[test]
    fn plan_binding_refuses_a_mismatched_hash() {
        let sha = "a".repeat(40);
        let pending = pending_versions();
        let mut req = bound_apply(&pending, &sha);
        let tampered = format!(
            "00{}",
            req.plan_manifest_sha256.as_deref().unwrap_or_default()[2..].to_owned()
        );
        req.plan_manifest_sha256 = Some(tampered);
        assert!(validate_request(&req).is_ok());
        assert!(matches!(
            check_plan_binding(&req, &pending, &MIGRATOR),
            Err(RunError::Refused(_))
        ));
    }

    #[test]
    fn plan_binding_refuses_the_same_hash_on_a_different_source_sha() {
        let pending = pending_versions();
        let req = bound_apply(&pending, &"a".repeat(40));
        // The same hash presented for another source SHA must refuse: the
        // CEO approval names one plan for one reviewed commit.
        let replay = Request {
            source_sha: "b".repeat(40),
            ..req
        };
        assert!(validate_request(&replay).is_ok());
        assert!(matches!(
            check_plan_binding(&replay, &pending, &MIGRATOR),
            Err(RunError::Refused(_))
        ));
    }

    #[test]
    fn plan_binding_refuses_missing_or_malformed_binding_inputs() {
        let sha = "a".repeat(40);
        let pending = pending_versions();
        let good = bound_apply(&pending, &sha);
        for mutate in [
            |r: &mut Request| r.plan_manifest_sha256 = None,
            |r: &mut Request| r.plan_run_id = None,
            |r: &mut Request| {
                r.plan_manifest_sha256 = Some("not-a-digest".to_owned());
            },
            |r: &mut Request| {
                r.plan_manifest_sha256 = Some("00".repeat(32)[1..].to_owned());
            },
            |r: &mut Request| r.plan_run_id = Some(String::new()),
            |r: &mut Request| r.plan_run_id = Some("plan-42".to_owned()),
        ] {
            let mut req = good.clone();
            mutate(&mut req);
            assert!(matches!(validate_request(&req), Err(RunError::Refused(_))));
        }
        // Plan ignores the binding inputs entirely.
        let plan = Request {
            apply: false,
            expected_pending: None,
            plan_manifest_sha256: None,
            plan_run_id: None,
            ..good
        };
        assert!(validate_request(&plan).is_ok());
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
            plan_manifest_sha256: None,
            plan_run_id: None,
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
        // Apply refuses without the plan-bound identity.
        assert!(matches!(
            validate_request(&Request {
                apply: true,
                expected_pending: Some(String::new()),
                ..ok.clone()
            }),
            Err(RunError::Refused(_))
        ));
        // Apply accepts a well-formed binding when given one.
        assert!(validate_request(&Request {
            apply: true,
            expected_pending: Some(String::new()),
            plan_manifest_sha256: Some("ab".repeat(32)),
            plan_run_id: Some("123456789".to_owned()),
            ..ok.clone()
        })
        .is_ok());
    }

    #[test]
    fn modes_select_role_and_binding() {
        let sha = "a".repeat(40);
        let plan = Request {
            url: Some("postgres://u@agent-testdb:5432/two_staging".to_owned()),
            source_sha: sha.clone(),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_staging".to_owned(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            plan_manifest_sha256: None,
            plan_run_id: None,
        };
        assert_eq!(binding_env(&plan), PLAN_URL_ENV);
        assert_eq!(expected_role(&plan), READ_ONLY_ROLE);
        assert!(validate_request(&plan).is_ok());
        assert_ne!(PLAN_URL_ENV, URL_ENV);
        let pending = pending_versions();
        let apply = bound_apply(&pending, &sha);
        assert_eq!(binding_env(&apply), URL_ENV);
        assert_eq!(expected_role(&apply), MIGRATOR_ROLE);
    }

    #[test]
    fn plan_refuses_a_missing_ro_binding_by_name() {
        let ok = Request {
            url: Some("postgres://u@agent-testdb:5432/two_staging".to_owned()),
            source_sha: "a".repeat(40),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_staging".to_owned(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            plan_manifest_sha256: None,
            plan_run_id: None,
        };
        // Plan refuses on the absent RO binding by name: the migrator URL is a
        // different binding and can never satisfy the plan path.
        for missing in [
            Request {
                url: None,
                ..ok.clone()
            },
            Request {
                url: Some(String::new()),
                ..ok.clone()
            },
        ] {
            let err = validate_request(&missing).unwrap_err().to_string();
            assert!(
                err.contains(PLAN_URL_ENV),
                "plan refusal must name the RO binding, got: {err}"
            );
        }
        // Apply still names its own binding, unchanged.
        let pending = pending_versions();
        let missing_apply = Request {
            url: None,
            ..bound_apply(&pending, &"a".repeat(40))
        };
        let err = validate_request(&missing_apply).unwrap_err().to_string();
        assert!(
            err.contains(URL_ENV),
            "apply refusal must name the migrator binding, got: {err}"
        );
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
            plan_manifest_sha256: None,
            plan_run_id: None,
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
