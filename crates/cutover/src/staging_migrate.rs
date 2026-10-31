//! SQLx migration runner with an explicit staging/production target.
//!
//! Applies two independent chains, each with its own ledger: this crate's
//! embedded migrations (ledger `public._sqlx_migrations`) and the store
//! migrations from [`two_bot_store`] (ledger `public._two_bot_migrations`,
//! the chain the DML-only gateway needs at boot but nothing applied). Each
//! chain is planned, applied and verified separately with its own pending
//! list and plan-binding hash, so a drifted store ledger refuses exactly
//! like a drifted cutover ledger.
//! Every pooled connection runs `SET ROLE` in `after_connect` and verifies
//! `current_user`: plan connections assume the read-only [`READ_ONLY_ROLE`]
//! group (physically incapable of DDL), while apply connections assume the
//! [`MIGRATOR_ROLE`] group, so each connection that executes DDL is proven to
//! hold the migrator group.
//!
//! Fail-closed: every refusal happens before any DDL. The runner never resets,
//! reverts, restores, creates roles, grants privileges or reads other
//! credentials. The database URL arrives only through the target and mode's fixed
//! environment binding (staging: [`PLAN_URL_ENV`] for plan, [`URL_ENV`] for apply;
//! production: [`PROD_PLAN_URL_ENV`] for plan, [`PROD_URL_ENV`] for apply) and
//! is never printed. Plan refuses when the RO binding is absent, even when a
//! migrator URL is set elsewhere, and refuses before reading the ledger when
//! its login also holds [`MIGRATOR_ROLE`] (the plan credential is RO only).

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha384};
use sqlx::{
    migrate::Migrator,
    postgres::{PgConnectOptions, PgConnection, PgPoolOptions, PgRow},
    Executor, PgPool, Row,
};
use two_bot_core::database_tls::{self, TlsPolicy};

use crate::intake_cap::{read_bytes_capped, IntakeReadError, INTAKE_CAP_LABEL};

/// Fixed binding names; each value is a secret and is never echoed.
/// Plan reads only the RO binding; apply reads only the migrator binding.
/// Staging keeps its names; production reads only its own pair.
pub const URL_ENV: &str = "TWO_BOT_STAGING_MIGRATOR_DATABASE_URL";
pub const PLAN_URL_ENV: &str = "TWO_BOT_STAGING_PLAN_DATABASE_URL";
/// Production bindings (PlanetScale `two_bot`): plan reads only the read-only
/// binding, apply reads only the migrator binding. A mode never borrows the
/// other target's credential.
pub const PROD_URL_ENV: &str = "TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL";
pub const PROD_PLAN_URL_ENV: &str = "TWO_BOT_PRODUCTION_PLAN_DATABASE_URL";
pub const MIGRATOR_ROLE: &str = "two_bot_migrator";
/// Read-only migration-plan identity: SELECT on bot tables, the admission
/// lane and the ledger; no DML, DDL, sequence, function or web-view access
/// (rendered by `sql/database_roles.sql`, verified by
/// `crates/core/tests/database_roles.rs`).
pub const READ_ONLY_ROLE: &str = "two_bot_migrator_ro";
/// SQLx library version this runner is pinned to (asserted against Cargo.lock).
pub const SQLX_VERSION: &str = "0.9.0";
pub const RUNNER_VERSION: u32 = 1;
/// Target pool shape: max 2 connections with a 10 s acquire timeout, so a dead
/// host fails the run instead of hanging pool checkout. Unlike application
/// pools, this pool adds no statement timeout: migration DDL can exceed 15 s.
pub const TARGET_POOL_MAX: u32 = 2;
pub const TARGET_ACQUIRE_TIMEOUT_SECS: u64 = 10;

/// Read-only audit inputs: the matrix and verifier compiled from the source SHA
/// the workflow checked out and built, rendered the way
/// `two_bot_core::database_roles::verify` renders them. Their SHA-256 digests
/// are echoed in the manifest so a reviewer can compare them with the files at
/// any other SHA.
const AUDIT_MATRIX: &str = include_str!("../../../sql/database_role_matrix.sql");
const AUDIT_VERIFY: &str = include_str!("../../../sql/verify_database_roles.sql");
/// Membership entries for the four migrator logins. Driven from `VALUES` with a
/// `LEFT JOIN`, so a login that does not exist still reports an entry with
/// `exists` false instead of vanishing from the readout. Direct memberships
/// are aggregated per login; the two transitive flags mirror the plan guard's
/// `pg_has_role` check (`refuse_migrator_member_plan_login`).
const AUDIT_MEMBERSHIP_SQL: &str = "SELECT e.login, (r.oid IS NOT NULL) AS login_exists, \
     COALESCE(array_agg(m.rolname::text ORDER BY m.rolname) \
     FILTER (WHERE m.rolname IS NOT NULL), '{}') AS direct_memberships, \
     COALESCE(pg_has_role(r.oid, 'two_bot_migrator', 'MEMBER'), false) AS member_of_migrator, \
     COALESCE(pg_has_role(r.oid, 'two_bot_migrator_ro', 'MEMBER'), false) AS member_of_ro \
     FROM (VALUES ('two_bot_migrator'), ('two_bot_migrator_ro'), \
     ('two_bot_migrator_ro_plan'), ('two_bot_migrator_apply')) AS e(login) \
     LEFT JOIN pg_roles r ON r.rolname = e.login \
     LEFT JOIN pg_auth_members am ON am.member = r.oid \
     LEFT JOIN pg_roles m ON m.oid = am.roleid \
     GROUP BY e.login, r.oid ORDER BY e.login";

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Which migration chain a ledger operation targets. Each chain has its own
/// embedded source and its own bookkeeping table; the two never share rows,
/// so a same-version number in the other chain can never trip a mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    Cutover,
    Store,
}

impl Chain {
    /// Bookkeeping table for this chain. The store table name is the store
    /// crate's own constant, so the runner can never drift from the runtime.
    pub fn table(self) -> &'static str {
        match self {
            Self::Cutover => "_sqlx_migrations",
            Self::Store => two_bot_store::TABLE_NAME,
        }
    }

    /// Embedded migrations for this chain. The store set is cloned out of the
    /// store crate (the single source of truth) and pointed at its own
    /// ledger; the bot-range guard mirrors `two_bot_store::migrate` so an
    /// out-of-range store version refuses here too, before any DDL.
    pub fn migrator(self) -> Result<Migrator, String> {
        match self {
            Self::Cutover => Ok(Migrator::with_migrations(
                MIGRATOR.iter().cloned().collect(),
            )),
            Self::Store => {
                if two_bot_store::MIGRATOR
                    .iter()
                    .any(|m| !(1..=999).contains(&m.version))
                {
                    return Err("store migrations must use versions 0001-0999".to_owned());
                }
                let mut migrator =
                    Migrator::with_migrations(two_bot_store::MIGRATOR.iter().cloned().collect());
                migrator.dangerous_set_table_name(two_bot_store::TABLE_NAME);
                Ok(migrator)
            }
        }
    }
}

/// Explicit migration target. The CLI requires `--target staging|production`;
/// there is no default. Staging keeps its bindings and production-substring
/// refusal; production reads only its own bindings, allows the production
/// substring, and additionally refuses any staging host pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Staging,
    Production,
}

impl Target {
    /// Parse the CLI `--target` value. Accepts only the two lowercase names.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "staging" => Some(Self::Staging),
            "production" => Some(Self::Production),
            _ => None,
        }
    }

    /// Canonical name echoed in the manifest (`migration_target`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }
}

/// Staging host pins the production target must never touch.
///
/// Staging runs on shared Neon (plus the `agent-testdb` / loopback fixtures),
/// while production runs on PlanetScale. Any binding host that is a fixture
/// host, carries a `staging` label, or points at Neon is a staging pin: the
/// production target refuses it even when the dispatch pins match it, so a
/// mistaken production pin aimed at staging still fails closed before any DDL.
fn is_staging_host(host_lower: &str) -> bool {
    host_lower == "agent-testdb"
        || host_lower == "127.0.0.1"
        || host_lower == "localhost"
        || host_lower.contains("staging")
        || host_lower.contains("neon.tech")
}

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

fn with_cutover_evidence(
    error: RunError,
    cutover_after: &[LedgerRow],
    cutover_applied: usize,
) -> RunError {
    match error {
        RunError::Failed(message, evidence) => {
            let mut evidence = evidence.as_object().cloned().unwrap_or_default();
            evidence.insert("ledger_after".to_owned(), ledger_json(cutover_after));
            evidence.insert("applied_count".to_owned(), json!(cutover_applied));
            RunError::Failed(message, Value::Object(evidence))
        }
        RunError::Refused(message) => RunError::Refused(message),
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub url: Option<String>,
    pub target: Target,
    pub source_sha: String,
    pub expected_host: String,
    pub expected_database: String,
    /// Pinned PlanetScale branch id (non-secret workflow input:
    /// `--staging-branch-id` on staging, `--production-branch-id` on
    /// production). Empty means no pin (Neon and test hosts). Required when
    /// the pinned host is a `*.psdb.cloud` endpoint: PlanetScale routes by
    /// the username's `{role}.{branch_id}` suffix while every branch shares
    /// `postgres` as the database name, so host+database alone cannot fix
    /// the branch.
    pub expected_branch_id: String,
    pub recovery_evidence_ref: String,
    pub acl_plan_ref: String,
    pub apply: bool,
    /// Raw `expected_pending` workflow input (ascending, comma-separated
    /// versions). Required for apply; ignored for plan.
    pub expected_pending: Option<String>,
    /// Raw `expected_store_pending` workflow input for the store chain
    /// (own ledger, own pending list). Required for apply; ignored for plan.
    pub expected_store_pending: Option<String>,
    /// Plan-bound apply identity: the SHA-256 of the plan job's uploaded
    /// `staging-migrate-manifest.json` and the run that produced it. Both are
    /// required for apply; both are ignored for plan.
    pub plan_manifest_sha256: Option<String>,
    /// The store chain's own binding hash, computed over the same source SHA
    /// plus the store pending list and store migration table. Required for
    /// apply; ignored for plan. The cutover hash above is unchanged, so
    /// existing claim-script projections keep matching byte-for-byte.
    pub store_plan_manifest_sha256: Option<String>,
    pub plan_run_id: Option<String>,
    /// Provenance anchor (TOG-15157): path to the producing plan run's
    /// downloaded `staging-migrate-manifest.json` (the workflow fetches it
    /// from `plan_run_id` before the runner starts). Apply hashes nothing
    /// itself here; it parses the artifact and requires its embedded
    /// `plan_manifest_sha256` to equal the recomputed manifest hash, so the
    /// bound hash is proven to come from the named run. Required for apply;
    /// ignored for plan.
    pub plan_manifest_path: Option<String>,
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

/// True for PlanetScale Postgres endpoints (`*.pg.psdb.cloud`,
/// `*.horizon.psdb.cloud`). Both serve direct Postgres on 5432 and
/// transaction-mode PgBouncer on 6432 of the same host.
fn is_planetscale_host(host: &str) -> bool {
    host.to_ascii_lowercase().ends_with(".psdb.cloud")
}

/// Bare branch-id shape: PlanetScale branch ids are short alphanumeric
/// strings (e.g. `cnlmx96ec5kw`). Allow alphanumerics, dash and underscore
/// up to 63 chars; anything else is not a bare pin.
fn branch_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Binding-shape refusal shared by `validate_request` (pre-connect) and
/// `verify_target` (binding match): pooled ports, pooler-style usernames and
/// branch mismatches all refuse before any DDL.
fn check_binding_shape(
    req: &Request,
    options: &sqlx::postgres::PgConnectOptions,
) -> Result<(), RunError> {
    // Transaction pooling (PgBouncer, transaction mode) is unsound for the
    // per-connection SET ROLE and the SQLx advisory lock: only direct 5432.
    if options.get_port() != 5432 {
        return refuse("pooled ports are refused; use the direct 5432 endpoint");
    }
    let username = options.get_username();
    // Dedicated and replica bouncers append `|name` to the username and
    // share the pinned host: the host pin alone cannot exclude them.
    if username.contains('|') {
        return refuse("pooler-style usernames are refused; use the direct role login");
    }
    let want = req.expected_branch_id.trim();
    if !want.is_empty() {
        // PlanetScale routes by `{role}.{branch_id}`: the username's final
        // dot-suffix must equal the pinned branch id.
        let matches = username
            .rsplit('.')
            .next()
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(want));
        if !matches {
            return refuse(format!(
                "binding branch does not match the pinned {} branch",
                req.target.as_str()
            ));
        }
    }
    Ok(())
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

/// Store chain plan binding, checked before any DDL.
///
/// Mirrors [`check_plan_binding`] over the store pending list and store
/// migration table, bound to `store_plan_manifest_sha256`. The cutover hash
/// is untouched, so existing claim-script projections keep matching.
pub fn check_store_binding(
    req: &Request,
    pending: &[i64],
    migrator: &Migrator,
) -> Result<String, RunError> {
    let computed = manifest_hash(&req.source_sha, pending, migrator);
    if req.apply {
        let want = req
            .store_plan_manifest_sha256
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if computed != want {
            return refuse(
                "store_plan_manifest_sha256 does not match the computed store plan manifest",
            );
        }
    }
    Ok(computed)
}

/// Provenance of the bound hash, checked after manifest computation but
/// before any DDL (TOG-15157 gap 2).
///
/// Exactness (`check_plan_binding`) proves the hash matches this run's own
/// source SHA, pending list and migration table, but that recomputation
/// alone cannot prove the hash came from the named producing run: its
/// inputs are public. So apply additionally parses the producing run's
/// downloaded manifest (fetched by the workflow from `plan_run_id` before
/// the runner starts) and requires its embedded `plan_manifest_sha256` to
/// equal the just-computed hash. A wrong run id, an expired or missing
/// artifact, an unreadable or unparseable file, or a field mismatch all
/// refuse: verification that is impossible is verification that failed.
/// For plan the check is a no-op (plan produces the manifest; it binds
/// nothing).
fn produced_hash(artifact: &Value, run_id: &str, field: &str) -> Result<String, RunError> {
    let produced = artifact
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if produced.is_empty() {
        return refuse(format!(
            "the manifest fetched from producing plan_run_id {run_id} carries no \
             {field}; the bound hash is unproven, refusing"
        ));
    }
    Ok(produced)
}

pub fn check_plan_provenance(
    req: &Request,
    computed_hash: &str,
    computed_store_hash: &str,
) -> Result<(), RunError> {
    if !req.apply {
        return Ok(());
    }
    let run_id = req.plan_run_id.as_deref().unwrap_or_default().trim();
    let path = req.plan_manifest_path.as_deref().unwrap_or_default().trim();
    if path.is_empty() {
        return refuse(
            "apply requires plan_manifest_path: the producing plan_run_id run's \
             downloaded staging-migrate-manifest.json",
        );
    }
    let bytes = match read_bytes_capped(path) {
        Ok(bytes) => bytes,
        Err(IntakeReadError::OverCap) => {
            return refuse(format!(
                "the manifest the workflow fetched from producing plan_run_id {run_id} \
                 at {path} exceeds {INTAKE_CAP_LABEL}; plan_manifest_sha256 provenance is \
                 unprovable, refusing"
            ));
        }
        Err(IntakeReadError::Unreadable(_)) => {
            return refuse(format!(
                "cannot read the manifest the workflow fetched from producing plan_run_id {run_id} \
                 at {path}; plan_run_id verification is impossible, refusing"
            ));
        }
    };
    let artifact: Value = serde_json::from_slice(&bytes).map_err(|_| {
        RunError::Refused(format!(
            "the manifest fetched from producing plan_run_id {run_id} at {path} is not valid \
             JSON; plan_manifest_sha256 provenance is unprovable, refusing"
        ))
    })?;
    let produced = produced_hash(&artifact, run_id, "plan_manifest_sha256")?;
    if produced != computed_hash.trim().to_ascii_lowercase() {
        return refuse(format!(
            "the manifest fetched from producing plan_run_id {run_id} does not match \
             plan_manifest_sha256; apply is not bound to that plan run, refusing"
        ));
    }
    // The store chain binds through the same producing manifest: its embedded
    // store hash must equal the recomputed store projection, or the apply is
    // not bound to that plan run's store chain.
    let produced_store = produced_hash(&artifact, run_id, "store_plan_manifest_sha256")?;
    if produced_store != computed_store_hash.trim().to_ascii_lowercase() {
        return refuse(format!(
            "the manifest fetched from producing plan_run_id {run_id} does not match \
             store_plan_manifest_sha256; apply is not bound to that plan run, refusing"
        ));
    }
    Ok(())
}

/// Pure prerequisite checks; nothing here touches the network.
///
/// Pinned-identity model: the workflow pins the exact non-secret endpoint host
/// and database name per dispatch, and `verify_target` refuses before any DDL
/// unless the secret binding points at exactly that pinned identity. A
/// `staging` substring in the database name remains accepted but is no longer
/// required, so the verified shared-Neon staging database (which cannot carry
/// `staging` in its name) can be targeted.
///
/// Staging keeps the production exclusion: any `prod`-like host or database pin
/// is refused. Production drops that substring refusal (the production host and
/// the `two_bot` database are production-like by construction) and instead
/// refuses any staging host pin: a fixture host, a `staging` label, or a Neon
/// endpoint. The binding-host check in `verify_target` enforces the same fence
/// on the secret URL itself.
pub fn validate_request(req: &Request) -> Result<(), RunError> {
    let sha = &req.source_sha;
    if sha.len() != 40 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return refuse("source SHA must be a full 40-char lowercase hex commit");
    }
    let host_lower = req.expected_host.to_ascii_lowercase();
    let db_lower = req.expected_database.to_ascii_lowercase();
    match req.target {
        Target::Staging => {
            for (label, lower) in [
                ("expected host", &host_lower),
                ("expected database", &db_lower),
            ] {
                if lower.is_empty()
                    || lower.contains("prod")
                    || lower.contains('@')
                    || lower.contains('/')
                {
                    return refuse(format!(
                        "{label} is empty, production-like or not a bare name"
                    ));
                }
            }
        }
        Target::Production => {
            for (label, lower) in [
                ("expected host", &host_lower),
                ("expected database", &db_lower),
            ] {
                if lower.is_empty() || lower.contains('@') || lower.contains('/') {
                    return refuse(format!("{label} is empty or not a bare name"));
                }
            }
            // Production never targets staging: the host pin itself is fenced
            // here, and the secret binding host is fenced again in
            // `verify_target`, so a mistaken pin aimed at staging still refuses.
            if is_staging_host(&host_lower) {
                return refuse(
                    "production target refuses a staging host pin; use the production host",
                );
            }
        }
    }
    // Session SET ROLE and the SQLx advisory lock are unsound behind
    // transaction pooling: only the direct endpoint may be targeted.
    // PlanetScale serves PgBouncer on port 6432 of the same host (never a
    // `-pooler` hostname), so the hostname check alone cannot exclude it:
    // the port and username checks in `check_binding_shape` own the rest.
    if req.expected_host.to_ascii_lowercase().contains("-pooler") {
        return refuse("pooler endpoints are refused; use the direct endpoint");
    }
    // PlanetScale branch pin: every branch shares `postgres` as the database
    // name and routes by the username's `{role}.{branch_id}` suffix, so the
    // host+database pin alone cannot fix the branch. A `*.psdb.cloud` pin
    // requires the non-secret branch id (`--staging-branch-id` on staging,
    // `--production-branch-id` on production); other hosts accept an empty
    // pin. The missing-pin refusal lives after the pooled-shape check above
    // so pooled bindings keep the pooled refusal in both gates, mirroring
    // `verify_target`; the production refusal must not name
    // `--staging-branch-id`, which the production target rejects.
    let branch = req.expected_branch_id.trim();
    if !branch.is_empty() && !branch_id_valid(branch) {
        return refuse(format!(
            "{} branch id is not a bare branch pin",
            req.target.as_str()
        ));
    }
    if branch.is_empty() && is_planetscale_host(&req.expected_host) {
        return match req.target {
            Target::Staging => refuse("PlanetScale hosts require --staging-branch-id"),
            Target::Production => refuse(
                "production PlanetScale dispatch needs a production branch pin \
                 (--production-branch-id)",
            ),
        };
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
    match &req.expected_store_pending {
        Some(raw) => {
            parse_expected_pending(raw).map_err(RunError::Refused)?;
        }
        None if req.apply => {
            return refuse("apply requires expected_store_pending from the reviewed plan");
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
    // The store chain binds separately: its hash covers the same source SHA
    // plus the store pending list and store migration table.
    match &req.store_plan_manifest_sha256 {
        Some(hash) => {
            let hash = hash.trim().to_ascii_lowercase();
            if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return refuse("store_plan_manifest_sha256 must be a 64-char lowercase hex digest");
            }
        }
        None if req.apply => {
            return refuse("apply requires store_plan_manifest_sha256 from the reviewed plan");
        }
        None => {}
    }
    // Provenance anchor: apply must also name the producing run's downloaded
    // manifest so `run_on_pool` can prove the bound hash came from that run.
    // Shape only here; the read and comparison happen after the manifest is
    // computed but before any DDL.
    if req.apply
        && req
            .plan_manifest_path
            .as_deref()
            .is_none_or(|p| p.trim().is_empty())
    {
        return refuse(
            "apply requires plan_manifest_path: the producing plan_run_id run's \
             downloaded staging-migrate-manifest.json",
        );
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
    // Pre-connect pooled/branch refusal: the same shape check `verify_target`
    // runs is parsed here so a pooled `:6432`, `|bouncer` or wrong-branch
    // binding refuses in `validate_request` too. An unparseable URL is left
    // for `verify_target`, which owns the invalid-URL refusal.
    if let Some(url) = req.url.as_deref().filter(|u| !u.is_empty()) {
        if let Ok(options) = two_bot_core::database_url::connect_options(url) {
            check_binding_shape(req, &options)?;
        }
    }
    Ok(())
}

/// The fixed environment binding for this target and mode: plan reads only the
/// read-only binding, apply reads only the migrator binding. The CLI sets
/// `Request.url` from exactly this binding, so a mode can never borrow the
/// other mode's credential nor the other target's credential;
/// `validate_request` refuses an absent binding before any connection is
/// attempted.
fn binding_env(req: &Request) -> &'static str {
    match (req.target, req.apply) {
        (Target::Staging, true) => URL_ENV,
        (Target::Staging, false) => PLAN_URL_ENV,
        (Target::Production, true) => PROD_URL_ENV,
        (Target::Production, false) => PROD_PLAN_URL_ENV,
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

/// Read the one TLS policy setting; an unset value is `Required`.
fn tls_policy_from_env() -> Result<TlsPolicy, RunError> {
    let value = std::env::var_os(database_tls::POLICY_SETTING);
    // A non-UTF-8 value parses as "" and is refused like any unknown value.
    TlsPolicy::from_setting(value.as_ref().map(|v| v.to_str().unwrap_or("")))
        .map_err(|m| RunError::Refused(m.to_owned()))
}

/// Verify the secret binding points at the pinned target identity and fence
/// it with the `TWO_DATABASE_TLS` policy (unset means `required`): validate
/// before SQLx can WARN-log query values, enforce the TLS policy, then apply
/// the effective mode. Preserve operator-supplied statement settings; do not
/// add an application-style deadline to migration DDL.
/// Failures are fixed strings; nothing echoes the URL, host or credential.
fn verify_target(req: &Request, tls: TlsPolicy) -> Result<PgConnectOptions, RunError> {
    let binding = binding_env(req);
    let url = req.url.as_deref().unwrap_or_default();
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return refuse(format!("{binding} value is not a Postgres URL"));
    }
    two_bot_core::database_url::validate(url).map_err(|m| RunError::Refused(m.to_owned()))?;
    // Threat-model F6: refuse plaintext/unverified modes and the wrong host
    // class before SQLx parses the URL (see `docs/database-tls.md`).
    database_tls::enforce(url, tls).map_err(|m| RunError::Refused(m.to_owned()))?;
    let options = two_bot_core::database_url::connect_options(url)
        .map_err(|_| RunError::Refused(format!("{binding} value is not a valid database URL")))?;
    let options = database_tls::apply(options, tls);
    let host = options.get_host().to_ascii_lowercase();
    let database = options.get_database().unwrap_or_default();
    if host.contains("-pooler") {
        return refuse("pooler endpoints are refused; use the direct endpoint");
    }
    // PlanetScale serves PgBouncer on the same host at port 6432 (with a
    // `|name` username suffix for dedicated poolers), so the `-pooler`
    // host check above cannot see it: a production binding copied from the
    // app's pooled connection string passes every host fence. Session
    // `SET ROLE` and the SQLx advisory lock are unsound behind transaction
    // pooling, so production targets only the direct endpoint: port 5432
    // with a bare login. SQLx percent-decodes the username, so one `|`
    // check covers both the raw and `%7C` forms (a raw `|` never parses).
    if req.target == Target::Production {
        if options.get_port() != 5432 {
            return refuse("production refuses a non-5432 binding port; use the direct endpoint");
        }
        if options.get_username().contains('|') {
            return refuse("production refuses a pooled binding login; use the direct login");
        }
    }
    // Production never touches a staging host, even when the dispatch pins
    // match it: the binding itself is fenced before the pin comparison.
    if req.target == Target::Production && is_staging_host(&host) {
        return refuse(
            "production target refuses a staging binding host; use the production binding",
        );
    }
    if host != req.expected_host.to_ascii_lowercase() || database != req.expected_database {
        return refuse(format!(
            "{binding} target does not match the verified {} identity",
            req.target.as_str(),
        ));
    }
    // Fail-closed PlanetScale pin: a `*.psdb.cloud` binding without a pinned
    // branch id refuses even when host+database match, so a dispatch that
    // forgets the branch pin cannot reach an unpinned branch. The production
    // refusal must not name `--staging-branch-id`, which the production
    // target rejects.
    if req.expected_branch_id.trim().is_empty() && is_planetscale_host(&host) {
        if req.target == Target::Production {
            return refuse(
                "production PlanetScale dispatch needs a production branch pin \
                 (--production-branch-id)",
            );
        }
        return refuse("PlanetScale hosts require --staging-branch-id");
    }
    check_binding_shape(req, &options)?;
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

/// Plan must run as a login that holds only [`READ_ONLY_ROLE`]. `session_user`
/// is unaffected by the per-connection `SET ROLE`, so this tests the login
/// itself: a login that could also `SET ROLE` to [`MIGRATOR_ROLE`] (directly,
/// by inheritance, or as a superuser) is not a read-only credential and is
/// refused. Fails closed: a failed check refuses too. Names the role, never the
/// login or the URL.
async fn refuse_migrator_member_plan_login(conn: &mut PgConnection) -> Result<(), RunError> {
    let holds_migrator: bool =
        sqlx::query_scalar("SELECT pg_has_role(session_user, $1::name, 'MEMBER')")
            .bind(MIGRATOR_ROLE)
            .fetch_one(&mut *conn)
            .await
            .map_err(|_| {
                RunError::Failed("plan login role check failed".to_owned(), Value::Null)
            })?;
    if holds_migrator {
        return refuse(format!(
            "plan login holds the {MIGRATOR_ROLE} group; the plan binding must be a login \
             that holds only {READ_ONLY_ROLE}"
        ));
    }
    Ok(())
}

fn ledger_rows(rows: &[PgRow]) -> Vec<LedgerRow> {
    rows.iter()
        .map(|r| LedgerRow {
            version: r.get("version"),
            description: r.get("description"),
            success: r.get("success"),
            checksum_hex: r.get("checksum"),
        })
        .collect()
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
    Ok(ledger_rows(&rows))
}

/// Read the store chain's own ledger. Mirrors [`read_ledger`] with the store
/// table literal; the two queries stay separate fixed strings so no table
/// name is ever interpolated from input.
async fn read_store_ledger(conn: &mut PgConnection) -> Result<Vec<LedgerRow>, sqlx::Error> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._two_bot_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !exists {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT version, description, success, encode(checksum, 'hex') AS checksum \
         FROM public._two_bot_migrations ORDER BY version",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(ledger_rows(&rows))
}

/// `Err` with the terminal failure kind when no attempt could read, so the
/// caller records [`LEDGER_UNKNOWN`] instead of an empty ledger. Reads one
/// chain's ledger; the chain selects which fixed query runs.
async fn read_ledger_after_run(
    pool: &PgPool,
    chain: Chain,
) -> Result<Vec<LedgerRow>, ReadAfterApply> {
    retry_transient(
        move || async move {
            let mut conn = pool
                .acquire()
                .await
                .map_err(|_| ReadAfterApply::PoolAcquireFailed)?;
            match chain {
                Chain::Cutover => read_ledger(&mut conn).await,
                Chain::Store => read_store_ledger(&mut conn).await,
            }
            .map_err(|_| ReadAfterApply::LedgerQueryFailed)
        },
        POST_APPLY_READ_ATTEMPTS,
        Duration::from_secs(POST_APPLY_READ_RETRY_DELAY_SECS),
    )
    .await
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

/// Recorded in place of a ledger that could not be read after a run. Never an
/// empty list: `[]` reads as "nothing applied".
const LEDGER_UNKNOWN: &str = "unknown: ledger unreadable";

/// Post-apply ledger read attempts: the pre-apply read uses the identical
/// query as the identical role, so a post-apply read failure is transient
/// connection state (a pool checkout timing out against a saturated
/// database), not grants or migration content. Retry briefly before reporting
/// the ledger unreadable; a permanently unreadable ledger still fails closed.
pub const POST_APPLY_READ_ATTEMPTS: u32 = 5;
pub const POST_APPLY_READ_RETRY_DELAY_SECS: u64 = 2;

/// Why the post-apply ledger read failed. Fixed strings only; never a URL,
/// host, login or error body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAfterApply {
    PoolAcquireFailed,
    LedgerQueryFailed,
}

impl ReadAfterApply {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PoolAcquireFailed => "pool_acquire_failed",
            Self::LedgerQueryFailed => "ledger_query_failed",
        }
    }
}

/// Run a fallible post-apply read `attempts` times (at least once), sleeping
/// `delay` between attempts. Returns the first success or the terminal
/// failure kind.
async fn retry_transient<F, Fut, T>(
    mut op: F,
    attempts: u32,
    delay: Duration,
) -> Result<T, ReadAfterApply>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ReadAfterApply>>,
{
    let attempts = attempts.max(1);
    for attempt in 0..attempts {
        match op().await {
            Ok(value) => return Ok(value),
            Err(kind) => {
                if attempt + 1 == attempts {
                    return Err(kind);
                }
                tokio::time::sleep(delay).await;
            }
        }
    }
    unreachable!("attempts is at least 1");
}

fn ledger_or_unknown(rows: Option<&[LedgerRow]>) -> Value {
    rows.map_or_else(|| json!(LEDGER_UNKNOWN), ledger_json)
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

/// Bounded read-only readout for the plan manifest, plan mode only.
///
/// Reports the ledger owner, the ledger successful/max/failed counts, one
/// membership entry per migrator login (existence, direct memberships and the
/// two transitive flags) and the repo verifier findings (rendered with the
/// compiled matrix). Names, counts and findings only: never passwords, URLs or
/// key material. A failed readout is reported as a fixed `error` string and
/// never fails the plan, because the plan manifest is also the apply binding.
async fn read_audit(pool: &PgPool) -> Value {
    match audit_queries(pool).await {
        Ok(audit) => audit,
        Err(reason) => json!({"error": reason}),
    }
}

/// Runs the readout inside one explicit `READ ONLY` transaction on the plan
/// pool, whose connections already hold only [`READ_ONLY_ROLE`]. A missing
/// ledger reports null/zero, so a pre-bootstrap plan still prints a readout.
async fn audit_queries(pool: &PgPool) -> Result<Value, &'static str> {
    let mut tx = pool.begin().await.map_err(|_| "audit acquire failed")?;
    sqlx::raw_sql("SET TRANSACTION READ ONLY; SET LOCAL search_path = pg_catalog, pg_temp;")
        .execute(&mut *tx)
        .await
        .map_err(|_| "audit read-only transaction failed")?;
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT r.rolname::text FROM pg_class c JOIN pg_roles r ON r.oid = c.relowner \
         WHERE c.relnamespace = 'public'::regnamespace AND c.relname = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| "audit ledger owner read failed")?;
    let counts = if owner.is_some() {
        let row: (i64, Option<i64>, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE success), max(version) FILTER (WHERE success), \
             count(*) FILTER (WHERE NOT success) FROM public._sqlx_migrations",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| "audit ledger counts read failed")?;
        json!({"successful_rows": row.0, "max_version": row.1, "failed_rows": row.2})
    } else {
        json!({"successful_rows": 0, "max_version": Value::Null, "failed_rows": 0})
    };
    // The store chain's own ledger, same readout: a missing table reports
    // null/zero so a pre-bootstrap plan still prints.
    let store_owner: Option<String> = sqlx::query_scalar(
        "SELECT r.rolname::text FROM pg_class c JOIN pg_roles r ON r.oid = c.relowner \
         WHERE c.relnamespace = 'public'::regnamespace AND c.relname = '_two_bot_migrations'",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| "audit store ledger owner read failed")?;
    let store_counts = if store_owner.is_some() {
        let row: (i64, Option<i64>, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE success), max(version) FILTER (WHERE success), \
             count(*) FILTER (WHERE NOT success) FROM public._two_bot_migrations",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| "audit store ledger counts read failed")?;
        json!({"successful_rows": row.0, "max_version": row.1, "failed_rows": row.2})
    } else {
        json!({"successful_rows": 0, "max_version": Value::Null, "failed_rows": 0})
    };
    let memberships: Vec<(String, bool, Vec<String>, bool, bool)> =
        sqlx::query_as(AUDIT_MEMBERSHIP_SQL)
            .fetch_all(&mut *tx)
            .await
            .map_err(|_| "audit membership read failed")?;
    let rendered = AUDIT_VERIFY.replace("-- @matrix", AUDIT_MATRIX);
    let findings: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(rendered))
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| "audit verify read failed")?;
    // Connection pressure: staging once refused all logins at 25/25
    // connections with no prior signal. Both counters are readable by any
    // login and live in pg_catalog, so they survive the search_path pin.
    // Count real client backends from pg_stat_database: every login can
    // read numbackends in full, while pg_stat_activity.backend_type is NULL
    // for other roles' sessions unless the reader holds pg_read_all_stats
    // (the plan login does not), which would hide exactly the sessions that
    // saturate the database. Aux processes are not backends and never
    // appear in numbackends.
    let connections_used: i64 = sqlx::query_scalar(CONNECTION_COUNT_SQL)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| "audit connection count read failed")?;
    let max_connections: i64 =
        sqlx::query_scalar("SELECT current_setting('max_connections')::bigint")
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| "audit max_connections read failed")?;
    let reserved_connections: i64 =
        sqlx::query_scalar("SELECT current_setting('superuser_reserved_connections')::bigint")
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| "audit reserved_connections read failed")?;
    tx.rollback().await.map_err(|_| "audit rollback failed")?;
    let mut membership_entries = Vec::with_capacity(memberships.len());
    for (login, exists, member_of, of_migrator, of_ro) in &memberships {
        membership_entries.push(json!({
            "login": login,
            "exists": exists,
            "member_of": member_of,
            "member_of_migrator": of_migrator,
            "member_of_ro": of_ro,
        }));
    }
    Ok(json!({
        "ledger_owner": owner,
        "ledger_counts": counts,
        "store_ledger_owner": store_owner,
        "store_ledger_counts": store_counts,
        "memberships": membership_entries,
        "verify_findings": findings,
        "matrix_sha256": hex::encode(Sha256::digest(AUDIT_MATRIX.as_bytes())),
        "verify_sha256": hex::encode(Sha256::digest(AUDIT_VERIFY.as_bytes())),
        "connection_usage": {
            "used": connections_used,
            "max_connections": max_connections,
            "reserved_connections": reserved_connections,
            "effective_max_connections": effective_max_connections(max_connections, reserved_connections),
            "pct_full": connections_used as f64 / effective_max_connections(max_connections, reserved_connections).max(1) as f64 * 100.0,
        },
    }))
}

/// Client-backend count from `pg_stat_database`: aux processes (walwriter,
/// checkpointer, background writer, autovacuum and replication launchers)
/// are not backends, so `numbackends` excludes them without the
/// `backend_type` filter — whose value is NULL for other roles' sessions
/// unless the reader holds `pg_read_all_stats`, which the plan login does
/// not. Every login reads this in full, so the 80% warning can fire when
/// the runtime role's sessions saturate the database.
pub const CONNECTION_COUNT_SQL: &str =
    "SELECT coalesce(sum(numbackends), 0)::bigint FROM pg_stat_database";

/// Non-superuser login ceiling: `max_connections` minus
/// `superuser_reserved_connections`. Degenerate inputs floor at 1 so the
/// warning ratio never divides by zero.
pub fn effective_max_connections(max_connections: i64, reserved_connections: i64) -> i64 {
    (max_connections.saturating_sub(reserved_connections.max(0))).max(1)
}

/// Warn threshold for staging connection pressure: staging once refused all
/// logins at 25/25 connections with no prior signal. Strictly above 80%
/// warns; degenerate inputs never warn. The line is a GitHub workflow
/// annotation, so the plan run surfaces it without failing the plan manifest
/// the apply binds to.
pub const CONNECTION_WARN_PCT: f64 = 80.0;

/// `Some` annotation line when `used` exceeds [`CONNECTION_WARN_PCT`] of
/// `max`, else `None`. Counts only; never a URL, host or login.
pub fn connection_warning(used: i64, max: i64) -> Option<String> {
    if max <= 0 || used < 0 {
        return None;
    }
    let pct = used as f64 / max as f64 * 100.0;
    if pct > CONNECTION_WARN_PCT {
        Some(format!(
            "::warning::staging database connections above 80% of max_connections: \
             {used}/{max} ({pct:.1}%)"
        ))
    } else {
        None
    }
}

/// Read the plan audit's `connection_usage` back into [`connection_warning`].
/// Prefers the non-superuser `effective_max_connections` ceiling when present
/// (older manifests carry only `max_connections`). A failed or field-less
/// readout warns about nothing.
fn connection_warning_from_audit(audit: &Value) -> Option<String> {
    let usage = audit.get("connection_usage")?;
    let used = usage.get("used")?.as_i64()?;
    let max = usage.get("max_connections")?.as_i64()?;
    let reserved = usage.get("reserved_connections").and_then(Value::as_i64);
    let ceiling = reserved
        .map(|r| effective_max_connections(max, r))
        .unwrap_or(max);
    connection_warning(used, ceiling)
}

/// Shared prefix between the role-check producer (`connect`) and the
/// `Protocol` matcher below: rewording one side rewords both, so the
/// `role-verify` branch cannot silently stop matching.
const ROLE_NOT_ACTIVE_PREFIX: &str = "expected role ";

/// Bounded, non-secret cause class for a `connect` failure.
///
/// `connect` refuses before any DDL with one fixed message; without the cause
/// every failure reads identically, forcing host forensics to guess transient
/// connection loss from real membership loss. The cause carries only the error
/// kind and, for database errors, the 5-character SQLSTATE code (fixed server
/// vocabulary, e.g. `42501`): never the message, severity, statement, URL,
/// host or login. Fail-closed is unchanged: the caller still refuses, only
/// with a triageable `[cause=...]` suffix.
///
/// Reachability through the pool path: sqlx retries `after_connect` failures,
/// refused ports and transient database errors until the acquire deadline,
/// then returns only `PoolTimedOut` (sqlx-core 0.9.0 `pool/inner.rs`). `connect`
/// therefore records the last `after_connect` cause (`set-role-denied`,
/// `role-verify`) in a shared slot and prefers it over `pool-timeout`; a dead
/// host or an exhausted server still refuses as `pool-timeout`, while startup
/// auth (`28xxx`), TCP timeouts and other immediate errors classify directly.
fn connect_cause(error: &sqlx::Error) -> String {
    match error {
        sqlx::Error::Database(db) => {
            let code = db.code().map(|c| c.into_owned()).unwrap_or_default();
            match code.as_str() {
                // `insufficient_privilege` from `SET ROLE`: the login is not a
                // member of the role. Real binding/membership loss.
                "42501" => "set-role-denied:42501".to_owned(),
                // Authentication failures: wrong or rotated credential, not a
                // network or membership event.
                "28000" | "28P01" => format!("auth:{code}"),
                _ if db.is_transient_in_connect_phase() => {
                    format!("connect-transient:{code}")
                }
                _ if !code.is_empty() => format!("database:{code}"),
                _ => "database:unknown".to_owned(),
            }
        }
        sqlx::Error::Io(io) => match io.kind() {
            std::io::ErrorKind::TimedOut => "connect-timeout".to_owned(),
            std::io::ErrorKind::ConnectionRefused => "connect-refused".to_owned(),
            std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::NotConnected => "connect-reset".to_owned(),
            std::io::ErrorKind::AddrNotAvailable
            | std::io::ErrorKind::AddrInUse
            | std::io::ErrorKind::NetworkDown
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::HostUnreachable => "connect-unreachable".to_owned(),
            std::io::ErrorKind::PermissionDenied => "connect-permission".to_owned(),
            _ => "io-error".to_owned(),
        },
        // Pool checkout timed out (bounded acquire timeout): the host accepted
        // connections but none became available. Transient, and distinct from
        // a TCP connect timeout.
        sqlx::Error::PoolTimedOut => "pool-timeout".to_owned(),
        sqlx::Error::PoolClosed => "pool-closed".to_owned(),
        sqlx::Error::Tls(_) => "tls".to_owned(),
        // Our own post-connect check (`current_user != role`): `SET ROLE`
        // succeeded syntactically but the role is not active. Real, fail-closed.
        sqlx::Error::Protocol(message) if message.starts_with(ROLE_NOT_ACTIVE_PREFIX) => {
            "role-verify".to_owned()
        }
        _ => "connect-error".to_owned(),
    }
}

/// Fail-closed refusal for `connect`, with the triageable cause suffix.
/// `role` is one of the two fixed group constants, never input.
fn connect_refusal(role: &str, error: &sqlx::Error) -> RunError {
    connect_refusal_with_cause(role, &connect_cause(error))
}

/// [`connect_refusal`] for a cause captured earlier (the `after_connect` slot):
/// same fixed message, same fail-closed shape, no error text.
fn connect_refusal_with_cause(role: &str, cause: &str) -> RunError {
    RunError::Refused(format!(
        "could not connect and SET ROLE {role} (missing binding or membership) [cause={cause}]"
    ))
}

async fn connect(
    options: PgConnectOptions,
    role: &'static str,
    verified: Arc<AtomicUsize>,
) -> Result<PgPool, RunError> {
    // sqlx retries `after_connect` failures until the acquire deadline, then
    // surfaces only `PoolTimedOut`: without this slot a real `SET ROLE`
    // denial (42501) and a dead host refuse identically. The slot keeps the
    // last `after_connect` cause so the refusal below reports what failed.
    let after_connect_cause: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&after_connect_cause);
    PgPoolOptions::new()
        .max_connections(TARGET_POOL_MAX)
        // Bound pool checkout, not the duration of migration statements.
        .acquire_timeout(Duration::from_secs(TARGET_ACQUIRE_TIMEOUT_SECS))
        .after_connect(move |conn, _meta| {
            let verified = Arc::clone(&verified);
            let slot = Arc::clone(&slot);
            Box::pin(async move {
                let outcome: Result<(), sqlx::Error> = async {
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
                            "{ROLE_NOT_ACTIVE_PREFIX}{role} is not active"
                        )));
                    }
                    verified.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
                .await;
                if let Err(error) = &outcome {
                    if let Ok(mut guard) = slot.lock() {
                        *guard = Some(connect_cause(error));
                    }
                }
                outcome
            })
        })
        .connect_with(options)
        .await
        .map_err(|error| {
            // Prefer the recorded `after_connect` cause: `PoolTimedOut` after
            // a refused `SET ROLE` means membership loss, not a slow host.
            if matches!(error, sqlx::Error::PoolTimedOut) {
                if let Ok(guard) = after_connect_cause.lock() {
                    if let Some(recorded) = guard.as_deref() {
                        return connect_refusal_with_cause(role, recorded);
                    }
                }
            }
            connect_refusal(role, &error)
        })
}

/// Run the plan (read-only, RO binding and role) or apply (migrator binding
/// and role) under the `TWO_DATABASE_TLS` policy (unset means `required`).
/// Returns the sanitized manifest.
pub async fn run(req: &Request) -> Result<Value, RunError> {
    validate_request(req)?;
    let options = verify_target(req, tls_policy_from_env()?)?;
    let verified = Arc::new(AtomicUsize::new(0));
    let role = expected_role(req);
    let pool = connect(options, role, Arc::clone(&verified)).await?;
    let result = run_on_pool(req, &pool, &verified).await;
    pool.close().await;
    result
}

/// Apply one chain's pending migrations and verify its ledger. Runs only
/// when `pending` is non-empty; returns the post-apply ledger and the
/// applied count. Every failure carries that chain's ledger evidence under
/// that chain's keys, so cutover and store evidence never mix.
async fn apply_chain(
    pool: &PgPool,
    chain: Chain,
    migrator: &Migrator,
    before: &[LedgerRow],
) -> Result<(Vec<LedgerRow>, usize), RunError> {
    let failed = |m: &str| RunError::Failed(m.to_owned(), Value::Null);
    // Evidence key per chain; the owner queries below stay separate fixed
    // literals (one per chain) so no table name is interpolated from input.
    let ledger_key = match chain {
        Chain::Cutover => "ledger_after",
        Chain::Store => "ledger_store_after",
    };
    let outcome = migrator.run(pool).await;
    let read_back = read_ledger_after_run(pool, chain).await;
    if let Err(e) = outcome {
        let mut evidence = serde_json::Map::with_capacity(2);
        evidence.insert("error".to_owned(), json!(e.to_string()));
        evidence.insert(
            format!("{ledger_key}_failure"),
            ledger_or_unknown(read_back.as_ref().ok().map(Vec::as_slice)),
        );
        return Err(RunError::Failed(
            "migration execution failed".to_owned(),
            Value::Object(evidence),
        ));
    }
    let rows = match read_back {
        Ok(rows) => rows,
        Err(kind) => {
            let mut evidence = serde_json::Map::with_capacity(2);
            evidence.insert(ledger_key.to_owned(), json!(LEDGER_UNKNOWN));
            evidence.insert(format!("{ledger_key}_error"), json!(kind.as_str()));
            return Err(RunError::Failed(
                "ledger unreadable after apply".to_owned(),
                Value::Object(evidence),
            ));
        }
    };
    let applied = rows.len().saturating_sub(before.len());
    let mut leftover_evidence = serde_json::Map::with_capacity(1);
    leftover_evidence.insert(ledger_key.to_owned(), ledger_json(&rows));
    let leftover = reconcile(&rows, migrator)
        .map_err(|m| RunError::Failed(m, Value::Object(leftover_evidence.clone())))?;
    if !leftover.is_empty() {
        return Err(RunError::Failed(
            "ledger incomplete after apply".to_owned(),
            Value::Object(leftover_evidence),
        ));
    }
    let owner: String = match chain {
        Chain::Cutover => {
            sqlx::query_scalar(
                "SELECT pg_get_userbyid(relowner)::text FROM pg_class \
                 WHERE oid = to_regclass('public._sqlx_migrations')",
            )
            .fetch_one(pool)
            .await
        }
        Chain::Store => {
            sqlx::query_scalar(
                "SELECT pg_get_userbyid(relowner)::text FROM pg_class \
                 WHERE oid = to_regclass('public._two_bot_migrations')",
            )
            .fetch_one(pool)
            .await
        }
    }
    .map_err(|_| failed("ledger owner check failed"))?;
    if owner != MIGRATOR_ROLE {
        return Err(failed("ledger is not owned by two_bot_migrator"));
    }
    Ok((rows, applied))
}

async fn run_on_pool(
    req: &Request,
    pool: &PgPool,
    verified: &AtomicUsize,
) -> Result<Value, RunError> {
    let failed = |m: &str| RunError::Failed(m.to_owned(), Value::Null);
    let cutover = Chain::Cutover.migrator().map_err(RunError::Refused)?;
    let store = Chain::Store.migrator().map_err(RunError::Refused)?;
    let mut conn = pool
        .acquire()
        .await
        .map_err(|_| failed("pool acquire failed"))?;
    // "RO only" is enforced, not just documented: a plan login that also holds
    // the migrator group is refused before either ledger is read.
    if !req.apply {
        refuse_migrator_member_plan_login(&mut conn).await?;
    }
    let before = read_ledger(&mut conn)
        .await
        .map_err(|_| failed("ledger read failed"))?;
    let store_before = read_store_ledger(&mut conn)
        .await
        .map_err(|_| failed("store ledger read failed"))?;
    drop(conn);
    let pending = reconcile(&before, &cutover).map_err(RunError::Refused)?;
    let store_pending = reconcile(&store_before, &store).map_err(RunError::Refused)?;
    // Plan binding: apply runs only the exact pending lists the reviewed plan
    // showed, one per chain. Validated in `validate_request`; compared here,
    // before any DDL.
    let expected = match &req.expected_pending {
        Some(raw) => parse_expected_pending(raw).map_err(RunError::Refused)?,
        None => Vec::new(),
    };
    if req.apply && expected != pending {
        return refuse("expected_pending does not match the computed pending list");
    }
    let expected_store = match &req.expected_store_pending {
        Some(raw) => parse_expected_pending(raw).map_err(RunError::Refused)?,
        None => Vec::new(),
    };
    if req.apply && expected_store != store_pending {
        return refuse("expected_store_pending does not match the computed store pending list");
    }
    let computed_hash = check_plan_binding(req, &pending, &cutover)?;
    let computed_store_hash = check_store_binding(req, &store_pending, &store)?;
    // Provenance follows exactness: each bound hash must additionally be the
    // hash the named producing plan run uploaded. All refuse before any DDL.
    check_plan_provenance(req, &computed_hash, &computed_store_hash)?;

    // Plan-only readout inside one READ ONLY transaction. Apply skips it: that
    // login is the migrator, not the read-only readout. A saturated staging
    // database warns on stderr (a workflow annotation) without failing the
    // plan manifest the apply binds to.
    let audit = if req.apply {
        Value::Null
    } else {
        let audit = read_audit(pool).await;
        if let Some(line) = connection_warning_from_audit(&audit) {
            eprintln!("{line}");
        }
        audit
    };

    let mut applied = 0usize;
    let mut after = before.clone();
    let mut store_applied = 0usize;
    let mut store_after = store_before.clone();
    if req.apply {
        if !pending.is_empty() {
            let (rows, count) = apply_chain(pool, Chain::Cutover, &cutover, &before).await?;
            after = rows;
            applied = count;
        }
        if !store_pending.is_empty() {
            let (rows, count) = apply_chain(pool, Chain::Store, &store, &store_before)
                .await
                .map_err(|error| with_cutover_evidence(error, &after, applied))?;
            store_after = rows;
            store_applied = count;
        }
    }
    Ok(json!({
        "runner_version": RUNNER_VERSION,
        "tool": {"name": "two-bot-cutover staging_migrate", "crate_version": env!("CARGO_PKG_VERSION"),
                 "sqlx": SQLX_VERSION, "invocation": "staging-migrate --target staging|production --plan|--apply"},
        "mode": if req.apply { "apply" } else { "plan" },
        "migration_target": req.target.as_str(),
        "source_sha": req.source_sha,
        "target": {"host": req.expected_host, "database": req.expected_database,
                   "branch_id": req.expected_branch_id.trim()},
        "recovery_evidence_ref": req.recovery_evidence_ref,
        "acl_plan_ref": req.acl_plan_ref,
        "role": expected_role(req),
        "role_verified_connections": verified.load(Ordering::SeqCst),
        "source_migrations": source_manifest(&cutover),
        "ledger_before": ledger_json(&before),
        "ledger_after": ledger_json(&after),
        "pending_before": pending,
        "expected_pending": expected,
        "plan_manifest_sha256": computed_hash,
        "source_store_migrations": source_manifest(&store),
        "ledger_store_before": ledger_json(&store_before),
        "ledger_store_after": ledger_json(&store_after),
        "pending_store_before": store_pending,
        "expected_store_pending": expected_store,
        "store_plan_manifest_sha256": computed_store_hash,
        "plan_run_id": req.plan_run_id.clone().unwrap_or_default(),
        // True only for apply, and only because `check_plan_provenance`
        // passed above: reaching this manifest means both bound hashes were
        // proven to come from the named producing plan run.
        "plan_provenance_verified": req.apply,
        "applied_count": applied,
        "applied_store_count": store_applied,
        // Plan-only readout (null for apply): ledger owners and counts,
        // four-login memberships and verifier findings from one READ ONLY
        // transaction. Not part of either plan hash.
        "audit": audit,
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
    fn store_failure_retains_committed_cutover_evidence() {
        let cutover = rows();
        let failure = RunError::Failed(
            "store migration failed".to_owned(),
            json!({"error": "store failure", "ledger_store_after_failure": LEDGER_UNKNOWN}),
        );

        let RunError::Failed(_, evidence) = with_cutover_evidence(failure, &cutover, cutover.len())
        else {
            panic!("failure should stay a failure");
        };
        assert_eq!(evidence["ledger_after"], ledger_json(&cutover));
        assert_eq!(evidence["applied_count"], cutover.len());
        assert_eq!(evidence["ledger_store_after_failure"], LEDGER_UNKNOWN);
    }

    #[test]
    fn claim_transport_projection_fixture_matches_rust() {
        use sqlx::{
            migrate::{Migration, MigrationType},
            SqlSafeStr,
        };
        // Synthetic SQL, no database. The shared consumer vector contains an
        // i64 above JS's safe-integer bound and a UTF-8 description.
        let migrator = Migrator::with_migrations(vec![
            Migration::new(
                1,
                "first fixture".into(),
                MigrationType::Simple,
                "SELECT 1;".into_sql_str(),
                false,
            ),
            Migration::new(
                9_007_199_254_740_993,
                "fixture résumé".into(),
                MigrationType::Simple,
                "SELECT 2;".into_sql_str(),
                false,
            ),
        ]);
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/staging-migrate-plan.json"))
                .unwrap();
        let pending = [1, 9_007_199_254_740_993];
        assert_eq!(fixture["source_migrations"], source_manifest(&migrator));
        assert_eq!(fixture["pending_before"], json!(pending));
        assert_eq!(
            fixture["plan_manifest_sha256"],
            manifest_hash(&"a".repeat(40), &pending, &migrator)
        );
        // The store half of the same fixture, pinned the same way: the
        // Python claim projection must match this Rust projection byte for
        // byte, so a drift in either breaks the test here, not in review.
        let store_fixture = Migrator::with_migrations(vec![
            Migration::new(
                400,
                "store first fixture".into(),
                MigrationType::Simple,
                "SELECT 3;".into_sql_str(),
                false,
            ),
            Migration::new(
                9_007_199_254_740_992,
                "store fixture résumé".into(),
                MigrationType::Simple,
                "SELECT 4;".into_sql_str(),
                false,
            ),
        ]);
        let store_pending = [400, 9_007_199_254_740_992];
        assert_eq!(
            fixture["source_store_migrations"],
            source_manifest(&store_fixture)
        );
        assert_eq!(fixture["pending_store_before"], json!(store_pending));
        assert_eq!(fixture["expected_store_pending"], json!(store_pending));
        assert_eq!(
            fixture["store_plan_manifest_sha256"],
            manifest_hash(&"a".repeat(40), &store_pending, &store_fixture)
        );
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
    fn chains_have_distinct_ledgers_matching_the_runtimes() {
        assert_eq!(Chain::Cutover.table(), "_sqlx_migrations");
        assert_eq!(Chain::Store.table(), two_bot_store::TABLE_NAME);
        assert_eq!(two_bot_store::TABLE_NAME, "_two_bot_migrations");
        assert_ne!(Chain::Cutover.table(), Chain::Store.table());
    }

    #[test]
    fn store_migrator_covers_the_foundation_without_leaving_bot_range() {
        let store = Chain::Store.migrator().expect("store chain builds");
        let versions: Vec<i64> = store.iter().map(|m| m.version).collect();
        assert!(!versions.is_empty(), "store chain must not be empty");
        for version in 400..=405 {
            assert!(
                versions.contains(&version),
                "store foundation {version} must be present"
            );
        }
        assert!(
            store.iter().all(|m| (1..=999).contains(&m.version)),
            "every store version stays in bot range"
        );
        assert_eq!(
            versions.len(),
            two_bot_store::MIGRATOR.iter().count(),
            "runner embeds the whole store chain"
        );
    }

    #[test]
    fn store_reconcile_mirrors_cutover_semantics() {
        let store = Chain::Store.migrator().expect("store chain builds");
        let all: Vec<LedgerRow> = store
            .iter()
            .map(|m| LedgerRow {
                version: m.version,
                description: m.description.to_string(),
                success: true,
                checksum_hex: hex::encode(&m.checksum),
            })
            .collect();
        assert_eq!(reconcile(&[], &store).unwrap().len(), all.len());
        assert!(reconcile(&all, &store).unwrap().is_empty());
        // A same-version number in the other chain's ledger is unknown here:
        // ledgers never share rows.
        let mut foreign = all.clone();
        foreign.push(LedgerRow {
            version: 1,
            ..all[0].clone()
        });
        assert!(reconcile(&foreign, &store).unwrap_err().contains("unknown"));
        let mut drift = all.clone();
        drift[0].checksum_hex = "00".repeat(48);
        assert!(reconcile(&drift, &store).unwrap_err().contains("drift"));
        let mut failed = all.clone();
        failed[0].success = false;
        assert!(reconcile(&failed, &store)
            .unwrap_err()
            .contains("incomplete"));
    }

    #[test]
    fn store_binding_accepts_its_own_hash_and_refuses_the_cutover_one() {
        let sha = "a".repeat(40);
        let store = Chain::Store.migrator().expect("store chain builds");
        let store_pending = reconcile(&[], &store).expect("empty store ledger");
        let req = bound_apply(&pending_versions(), &sha);
        // The fixture binds both chains, so the store binding accepts.
        assert!(validate_request(&req).is_ok());
        assert_eq!(
            check_store_binding(&req, &store_pending, &store).unwrap(),
            req.store_plan_manifest_sha256.clone().unwrap()
        );
        // A cutover hash presented as the store hash refuses: the chains
        // bind separately.
        let mut swapped = req.clone();
        swapped.store_plan_manifest_sha256 = swapped.plan_manifest_sha256.clone();
        assert!(validate_request(&swapped).is_ok());
        assert!(matches!(
            check_store_binding(&swapped, &store_pending, &store),
            Err(RunError::Refused(_))
        ));
        // A store pending list from another source SHA refuses as well.
        let mut replay = req.clone();
        replay.source_sha = "b".repeat(40);
        assert!(validate_request(&replay).is_ok());
        assert!(matches!(
            check_store_binding(&replay, &store_pending, &store),
            Err(RunError::Refused(_))
        ));
    }

    #[test]
    fn apply_refuses_without_the_store_binding() {
        let sha = "a".repeat(40);
        let good = bound_apply(&pending_versions(), &sha);
        for mutate in [
            |r: &mut Request| r.expected_store_pending = None,
            |r: &mut Request| r.store_plan_manifest_sha256 = None,
            |r: &mut Request| {
                r.store_plan_manifest_sha256 = Some("not-a-digest".to_owned());
            },
        ] {
            let mut req = good.clone();
            mutate(&mut req);
            assert!(matches!(validate_request(&req), Err(RunError::Refused(_))));
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

    /// An apply request carrying the exact hashes `check_plan_binding` and
    /// `check_store_binding` compute for their own source SHA, pending lists
    /// and migration tables.
    fn bound_apply(pending: &[i64], source_sha: &str) -> Request {
        let hash = manifest_hash(source_sha, pending, &MIGRATOR);
        let store = Chain::Store.migrator().expect("store chain builds");
        let store_pending =
            reconcile(&[], &store).expect("empty ledger leaves every store version pending");
        let store_hash = manifest_hash(source_sha, &store_pending, &store);
        Request {
            url: Some("postgres://u@agent-testdb:5432/two_staging".to_owned()),
            target: Target::Staging,
            source_sha: source_sha.to_owned(),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_staging".to_owned(),
            expected_branch_id: String::new(),
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
            expected_store_pending: Some(
                store_pending
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            plan_manifest_sha256: Some(hash),
            store_plan_manifest_sha256: Some(store_hash),
            plan_run_id: Some("123456789".to_owned()),
            // Shape-valid only: `validate_request` checks presence, while
            // `check_plan_provenance` reads the file. Provenance tests below
            // point this at real temp manifests.
            plan_manifest_path: Some("producing-plan/staging-migrate-manifest.json".to_owned()),
        }
    }

    /// Production apply carrying the exact hash for its own source SHA. Uses a
    /// non-staging fixture host so the staging-pin fence does not fire; the
    /// binding-host fence is covered separately.
    fn bound_production_apply(pending: &[i64], source_sha: &str) -> Request {
        let hash = manifest_hash(source_sha, pending, &MIGRATOR);
        let store = Chain::Store.migrator().expect("store chain builds");
        let store_pending =
            reconcile(&[], &store).expect("empty ledger leaves every store version pending");
        let store_hash = manifest_hash(source_sha, &store_pending, &store);
        Request {
            url: Some("postgres://u@prod-host.invalid:5432/two_bot".to_owned()),
            target: Target::Production,
            source_sha: source_sha.to_owned(),
            expected_host: "prod-host.invalid".to_owned(),
            expected_database: "two_bot".to_owned(),
            expected_branch_id: String::new(),
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
            expected_store_pending: Some(
                store_pending
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            plan_manifest_sha256: Some(hash),
            store_plan_manifest_sha256: Some(store_hash),
            plan_run_id: Some("123456789".to_owned()),
            plan_manifest_path: Some("producing-plan/production-migrate-manifest.json".to_owned()),
        }
    }

    fn staging_plan() -> Request {
        Request {
            url: Some("postgres://u@agent-testdb:5432/two_staging".to_owned()),
            target: Target::Staging,
            source_sha: "a".repeat(40),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_staging".to_owned(),
            expected_branch_id: String::new(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
        }
    }

    fn production_plan() -> Request {
        Request {
            url: Some("postgres://u@prod-host.invalid:5432/two_bot".to_owned()),
            target: Target::Production,
            source_sha: "a".repeat(40),
            expected_host: "prod-host.invalid".to_owned(),
            expected_database: "two_bot".to_owned(),
            expected_branch_id: String::new(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
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
            |r: &mut Request| r.plan_manifest_path = None,
            |r: &mut Request| r.plan_manifest_path = Some("   ".to_owned()),
        ] {
            let mut req = good.clone();
            mutate(&mut req);
            assert!(matches!(validate_request(&req), Err(RunError::Refused(_))));
        }
        // Plan ignores the binding inputs entirely.
        let plan = Request {
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
            ..good
        };
        assert!(validate_request(&plan).is_ok());
    }

    /// Write `contents` to a unique temp file and return its path. Unit tests
    /// run in parallel in one process, so the atomic counter keeps names
    /// disjoint; the caller removes the file.
    fn temp_manifest(contents: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "staging-provenance-test-{}-{id}.json",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("temp manifest must be writable");
        path
    }

    /// A producing-run manifest artifact carrying `hash` and `store_hash` in
    /// its embedded hash fields, as the plan job's `tee` would upload it.
    fn producing_manifest(hash: &str, store_hash: &str) -> String {
        json!({
            "runner_version": RUNNER_VERSION,
            "mode": "plan",
            "source_sha": "a".repeat(40),
            "plan_manifest_sha256": hash,
            "store_plan_manifest_sha256": store_hash,
            "plan_run_id": "",
        })
        .to_string()
    }

    /// The store projection hash for `sha` over an empty ledger, matching
    /// what `bound_apply` binds.
    fn store_hash_for(source_sha: &str) -> (Vec<i64>, Migrator, String) {
        let store = Chain::Store.migrator().expect("store chain builds");
        let pending =
            reconcile(&[], &store).expect("empty ledger leaves every store version pending");
        let hash = manifest_hash(source_sha, &pending, &store);
        (pending, store, hash)
    }

    #[test]
    fn plan_provenance_accepts_the_producing_run_manifest() {
        let sha = "a".repeat(40);
        let pending = pending_versions();
        let hash = manifest_hash(&sha, &pending, &MIGRATOR);
        let (_, _, store_hash) = store_hash_for(&sha);
        let path = temp_manifest(&producing_manifest(&hash, &store_hash));
        let req = Request {
            plan_manifest_path: Some(path.to_string_lossy().into_owned()),
            ..bound_apply(&pending, &sha)
        };
        assert!(validate_request(&req).is_ok());
        assert_eq!(check_plan_binding(&req, &pending, &MIGRATOR).unwrap(), hash);
        assert!(check_plan_provenance(&req, &hash, &store_hash).is_ok());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn plan_provenance_refuses_another_run_manifest() {
        let sha = "a".repeat(40);
        let pending = pending_versions();
        let hash = manifest_hash(&sha, &pending, &MIGRATOR);
        let (_, _, store_hash) = store_hash_for(&sha);
        // Same shape, different producing run: its manifest carries another
        // hash, so binding apply to this run id must refuse.
        let other = manifest_hash(&"b".repeat(40), &pending, &MIGRATOR);
        assert_ne!(other, hash);
        let path = temp_manifest(&producing_manifest(&other, &store_hash));
        let req = Request {
            plan_manifest_path: Some(path.to_string_lossy().into_owned()),
            ..bound_apply(&pending, &sha)
        };
        assert!(validate_request(&req).is_ok());
        let err = check_plan_provenance(&req, &hash, &store_hash).unwrap_err();
        assert!(matches!(err, RunError::Refused(_)));
        let message = err.to_string();
        assert!(message.contains("plan_run_id"), "{message}");
        assert!(message.contains("plan_manifest_sha256"), "{message}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn plan_provenance_refuses_a_store_hash_from_another_run() {
        let sha = "a".repeat(40);
        let pending = pending_versions();
        let hash = manifest_hash(&sha, &pending, &MIGRATOR);
        let (_, _, store_hash) = store_hash_for(&sha);
        // Cutover hash matches the producing run but the store hash is from
        // another source SHA: the apply is not bound to that run's store
        // chain and must refuse.
        let (_, _, other_store) = store_hash_for(&"b".repeat(40));
        assert_ne!(other_store, store_hash);
        let path = temp_manifest(&producing_manifest(&hash, &other_store));
        let req = Request {
            plan_manifest_path: Some(path.to_string_lossy().into_owned()),
            ..bound_apply(&pending, &sha)
        };
        assert!(validate_request(&req).is_ok());
        let err = check_plan_provenance(&req, &hash, &store_hash).unwrap_err();
        assert!(matches!(err, RunError::Refused(_)));
        assert!(err.to_string().contains("store_plan_manifest_sha256"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn plan_provenance_refuses_when_verification_is_impossible() {
        let sha = "a".repeat(40);
        let pending = pending_versions();
        let hash = manifest_hash(&sha, &pending, &MIGRATOR);
        let (_, _, store_hash) = store_hash_for(&sha);
        let good = bound_apply(&pending, &sha);
        // A wrong run id (or an expired artifact) leaves no manifest file:
        // the download step fails and, if the runner is ever reached without
        // one, the missing file refuses rather than proceeding unverified.
        let missing = Request {
            plan_manifest_path: Some(
                std::env::temp_dir()
                    .join("staging-provenance-test-no-such-file.json")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..good.clone()
        };
        assert!(validate_request(&missing).is_ok());
        assert!(matches!(
            check_plan_provenance(&missing, &hash, &store_hash),
            Err(RunError::Refused(_))
        ));
        // Corrupt or field-less artifacts are equally unprovable.
        for contents in [
            "{not json".to_owned(),
            json!({"mode": "plan"}).to_string(),
            json!({"mode": "plan", "plan_manifest_sha256": 42}).to_string(),
            json!({"mode": "plan", "plan_manifest_sha256": ""}).to_string(),
        ] {
            let path = temp_manifest(&contents);
            let req = Request {
                plan_manifest_path: Some(path.to_string_lossy().into_owned()),
                ..good.clone()
            };
            assert!(matches!(
                check_plan_provenance(&req, &hash, &store_hash),
                Err(RunError::Refused(_))
            ));
            std::fs::remove_file(&path).ok();
        }
        // Plan binds nothing, so it never reads the (absent) artifact.
        let plan = Request {
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
            ..good
        };
        assert!(check_plan_provenance(&plan, &hash, &store_hash).is_ok());
    }

    #[test]
    fn validation_refuses_before_connecting() {
        let ok = Request {
            url: Some("postgres://u@agent-testdb:5432/two_staging".to_owned()),
            target: Target::Staging,
            source_sha: "a".repeat(40),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_staging".to_owned(),
            expected_branch_id: String::new(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
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
        // PlanetScale staging uses the same pinned-host model plus a branch
        // pin: a direct `*.pg.psdb.cloud:5432` host with the matching
        // `{role}.{branch_id}` username validates, while a `-pooler` host,
        // a pooled `:6432` port, a `|bouncer` username, a wrong branch or a
        // missing branch pin refuses. PgBouncer serves pooling on 6432 of
        // the same host in transaction mode, never a `-pooler` hostname.
        let planetscale_pin = Request {
            url: Some(
                "postgresql://migrator.cnfixture01@psdb-fixture-1.pg.psdb.cloud:5432/postgres?sslmode=require"
                    .to_owned(),
            ),
            expected_host: "psdb-fixture-1.pg.psdb.cloud".to_owned(),
            expected_database: "postgres".to_owned(),
            expected_branch_id: "cnfixture01".to_owned(),
            ..ok.clone()
        };
        assert!(validate_request(&planetscale_pin).is_ok());
        assert!(verify_target(&planetscale_pin, TlsPolicy::Required).is_ok());
        // A PlanetScale pin without the branch id refuses: every branch
        // shares `postgres` as the database name, so host+database alone
        // cannot fix the branch.
        let ps_missing_branch = Request {
            url: Some(
                "postgresql://migrator.cnfixture01@psdb-fixture-1.pg.psdb.cloud:5432/postgres?sslmode=require"
                    .to_owned(),
            ),
            expected_host: "psdb-fixture-1.pg.psdb.cloud".to_owned(),
            expected_database: "postgres".to_owned(),
            expected_branch_id: String::new(),
            ..ok.clone()
        };
        assert!(matches!(
            validate_request(&ps_missing_branch),
            Err(RunError::Refused(_))
        ));
        assert!(matches!(
            verify_target(&ps_missing_branch, TlsPolicy::Required),
            Err(RunError::Refused(_))
        ));
        // Pooled shapes refuse in both gates even when the pins match them.
        for pooled in [
            // Default PgBouncer on the same host.
            "postgresql://migrator.cnfixture01@psdb-fixture-1.pg.psdb.cloud:6432/postgres?sslmode=require",
            // Dedicated/replica bouncer usernames share the pinned host.
            "postgresql://migrator.cnfixture01%7Cread-bouncer@psdb-fixture-1.pg.psdb.cloud:6432/postgres?sslmode=require",
            // Wrong branch credential on the pinned host.
            "postgresql://migrator.otherbranch@psdb-fixture-1.pg.psdb.cloud:5432/postgres?sslmode=require",
        ] {
            let req = Request {
                url: Some(pooled.to_owned()),
                expected_host: "psdb-fixture-1.pg.psdb.cloud".to_owned(),
                expected_database: "postgres".to_owned(),
                expected_branch_id: "cnfixture01".to_owned(),
                ..ok.clone()
            };
            assert!(
                matches!(validate_request(&req), Err(RunError::Refused(_))),
                "validate must refuse pooled/branch-mismatched binding {pooled}"
            );
            assert!(
                matches!(
                    verify_target(&req, TlsPolicy::Required),
                    Err(RunError::Refused(_))
                ),
                "verify must refuse pooled/branch-mismatched binding {pooled}"
            );
        }
        // A malformed branch pin refuses before any DDL. The no-binding
        // case proves the pin-format check itself refuses: without a URL the
        // username-suffix match cannot run, so only `branch_id_valid` can
        // refuse here.
        let bad_branch_no_url = Request {
            url: None,
            expected_branch_id: "not a branch!".to_owned(),
            ..planetscale_pin.clone()
        };
        assert!(matches!(
            validate_request(&bad_branch_no_url),
            Err(RunError::Refused(_))
        ));
        let bad_branch = Request {
            expected_branch_id: "not a branch!".to_owned(),
            ..planetscale_pin.clone()
        };
        assert!(matches!(
            validate_request(&bad_branch),
            Err(RunError::Refused(_))
        ));
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
                expected_host: "psdb-fixture-1-pooler.pg.psdb.cloud".to_owned(),
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
            expected_store_pending: Some(String::new()),
            plan_manifest_sha256: Some("ab".repeat(32)),
            store_plan_manifest_sha256: Some("ab".repeat(32)),
            plan_run_id: Some("123456789".to_owned()),
            plan_manifest_path: Some("producing-plan/staging-migrate-manifest.json".to_owned()),
            ..ok.clone()
        })
        .is_ok());
        // Apply refuses without the producing run's downloaded manifest: a
        // bound hash with no provenance anchor cannot be verified.
        assert!(matches!(
            validate_request(&Request {
                apply: true,
                expected_pending: Some(String::new()),
                plan_manifest_sha256: Some("ab".repeat(32)),
                plan_run_id: Some("123456789".to_owned()),
                plan_manifest_path: None,
                ..ok.clone()
            }),
            Err(RunError::Refused(_))
        ));
    }

    #[test]
    fn modes_select_role_and_binding() {
        let sha = "a".repeat(40);
        let plan = Request {
            url: Some("postgres://u@agent-testdb:5432/two_staging".to_owned()),
            target: Target::Staging,
            source_sha: sha.clone(),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_staging".to_owned(),
            expected_branch_id: String::new(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
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
            target: Target::Staging,
            source_sha: "a".repeat(40),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_staging".to_owned(),
            expected_branch_id: String::new(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
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
            target: Target::Staging,
            source_sha: "a".repeat(40),
            expected_host: "agent-testdb".to_owned(),
            expected_database: "two_bot".to_owned(),
            expected_branch_id: String::new(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
        };
        assert!(validate_request(&base).is_ok());
        assert!(verify_target(&base, TlsPolicy::LocalOnly).is_ok());
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
            assert!(matches!(
                verify_target(&pin, TlsPolicy::LocalOnly),
                Err(RunError::Refused(_))
            ));
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
        assert!(matches!(
            verify_target(&pooler, TlsPolicy::Required),
            Err(RunError::Refused(_))
        ));
        // A PlanetScale pooler binding is refused even when the pins match it.
        let ps_pooler = Request {
            url: Some(
                "postgresql://migrator.cnfixture01@psdb-fixture-1-pooler.pg.psdb.cloud:5432/postgres?sslmode=require"
                    .to_owned(),
            ),
            expected_host: "psdb-fixture-1-pooler.pg.psdb.cloud".to_owned(),
            expected_database: "postgres".to_owned(),
            expected_branch_id: "cnfixture01".to_owned(),
            ..base.clone()
        };
        assert!(matches!(
            validate_request(&ps_pooler),
            Err(RunError::Refused(_))
        ));
        assert!(matches!(
            verify_target(&ps_pooler, TlsPolicy::Required),
            Err(RunError::Refused(_))
        ));
        // A direct PlanetScale binding with the matching branch pin passes
        // both gates.
        let ps_direct = Request {
            url: Some(
                "postgresql://migrator.cnfixture01@psdb-fixture-1.pg.psdb.cloud:5432/postgres?sslmode=require"
                    .to_owned(),
            ),
            expected_host: "psdb-fixture-1.pg.psdb.cloud".to_owned(),
            expected_database: "postgres".to_owned(),
            expected_branch_id: "cnfixture01".to_owned(),
            ..base.clone()
        };
        assert!(validate_request(&ps_direct).is_ok());
        assert!(verify_target(&ps_direct, TlsPolicy::Required).is_ok());
        // Pooled and wrong-branch shapes refuse in both gates.
        for (url, branch) in [
            (
                "postgresql://migrator.cnfixture01@psdb-fixture-1.pg.psdb.cloud:6432/postgres?sslmode=require",
                "cnfixture01",
            ),
            (
                "postgresql://migrator.cnfixture01%7Cread-bouncer@psdb-fixture-1.pg.psdb.cloud:6432/postgres?sslmode=require",
                "cnfixture01",
            ),
            (
                "postgresql://migrator.otherbranch@psdb-fixture-1.pg.psdb.cloud:5432/postgres?sslmode=require",
                "cnfixture01",
            ),
        ] {
            let req = Request {
                url: Some(url.to_owned()),
                expected_host: "psdb-fixture-1.pg.psdb.cloud".to_owned(),
                expected_database: "postgres".to_owned(),
                expected_branch_id: branch.to_owned(),
                ..base.clone()
            };
            assert!(
                matches!(validate_request(&req), Err(RunError::Refused(_))),
                "validate must refuse {url}"
            );
            assert!(
                matches!(
                    verify_target(&req, TlsPolicy::Required),
                    Err(RunError::Refused(_))
                ),
                "verify must refuse {url}"
            );
        }
    }

    #[test]
    fn target_parses_only_the_two_names() {
        assert_eq!(Target::parse("staging"), Some(Target::Staging));
        assert_eq!(Target::parse("production"), Some(Target::Production));
        for raw in ["", "Staging", "STAGING", "prod", "production ", " staging"] {
            assert_eq!(Target::parse(raw), None, "must refuse {raw:?}");
        }
        assert_eq!(Target::Staging.as_str(), "staging");
        assert_eq!(Target::Production.as_str(), "production");
    }

    #[test]
    fn production_bindings_are_target_specific() {
        let staging_plan = staging_plan();
        let staging_apply = bound_apply(&pending_versions(), &"a".repeat(40));
        let prod_plan = production_plan();
        let prod_apply = bound_production_apply(&pending_versions(), &"a".repeat(40));
        assert_eq!(binding_env(&staging_plan), PLAN_URL_ENV);
        assert_eq!(binding_env(&staging_apply), URL_ENV);
        assert_eq!(binding_env(&prod_plan), PROD_PLAN_URL_ENV);
        assert_eq!(binding_env(&prod_apply), PROD_URL_ENV);
        assert_ne!(PLAN_URL_ENV, PROD_PLAN_URL_ENV);
        assert_ne!(URL_ENV, PROD_URL_ENV);
        assert_eq!(expected_role(&prod_plan), READ_ONLY_ROLE);
        assert_eq!(expected_role(&prod_apply), MIGRATOR_ROLE);
        assert!(validate_request(&staging_plan).is_ok());
        assert!(validate_request(&prod_plan).is_ok());
        assert!(validate_request(&staging_apply).is_ok());
        assert!(validate_request(&prod_apply).is_ok());
    }

    #[test]
    fn production_allows_prod_pins_but_refuses_staging_pins() {
        // Production drops the `prod` substring refusal: the production host
        // and database are production-like by construction.
        let mut prod = production_plan();
        prod.expected_host = "ep-prod-example.pscale.example".to_owned();
        prod.url = Some("postgres://u@ep-prod-example.pscale.example:5432/two_bot".to_owned());
        assert!(validate_request(&prod).is_ok());
        let mut prod_db = production_plan();
        prod_db.expected_database = "two_prod".to_owned();
        assert!(validate_request(&prod_db).is_ok());
        // Staging keeps the refusal on both pins.
        let mut staging = staging_plan();
        staging.expected_host = "ep-prod-example.us-east-2.aws.neon.tech".to_owned();
        assert!(matches!(
            validate_request(&staging),
            Err(RunError::Refused(_))
        ));
        let mut staging_db = staging_plan();
        staging_db.expected_database = "two_prod".to_owned();
        assert!(matches!(
            validate_request(&staging_db),
            Err(RunError::Refused(_))
        ));
        // Production refuses every staging host pin before any DDL.
        for host in [
            "agent-testdb",
            "127.0.0.1",
            "localhost",
            "ep-staging-example.us-east-2.aws.neon.tech",
            "ep-test-pooler.us-east-2.aws.neon.tech",
            "staging-host.invalid",
        ] {
            let mut req = production_plan();
            req.expected_host = host.to_owned();
            req.url = Some(format!("postgres://u@{host}:5432/two_bot"));
            let err = validate_request(&req).unwrap_err().to_string();
            assert!(
                err.contains("staging"),
                "production must name the staging fence, got: {err}"
            );
        }
    }

    #[test]
    fn production_refuses_a_staging_binding_host() {
        // Even when the pins match, a staging binding host refuses: the secret
        // URL itself is fenced, not just the pin text. The pins below name the
        // production host while the binding URL points at staging, so only the
        // binding fence can fire.
        let mut req = production_plan();
        req.expected_host = "prod-host.invalid".to_owned();
        req.url = Some("postgres://u@agent-testdb:5432/two_bot".to_owned());
        assert!(matches!(
            verify_target(&req, TlsPolicy::LocalOnly),
            Err(RunError::Refused(_))
        ));
        let err = verify_target(&req, TlsPolicy::LocalOnly)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("staging"),
            "binding fence must name staging: {err}"
        );
        // A Neon binding refuses the same way.
        let mut neon = production_plan();
        neon.expected_host = "prod-host.invalid".to_owned();
        neon.url = Some(
            "postgres://u@ep-staging-example.us-east-2.aws.neon.tech:5432/two_bot?sslmode=require"
                .to_owned(),
        );
        assert!(matches!(
            verify_target(&neon, TlsPolicy::Required),
            Err(RunError::Refused(_))
        ));
    }

    #[test]
    fn production_refuses_pooled_bindings() {
        // Pooled bindings refuse pre-connect in `validate_request` (shared
        // `check_binding_shape`) and again in `verify_target` before any DDL.
        // Use a non-PlanetScale host so the `*.psdb.cloud` branch-pin fence
        // stays out of this test; PlanetScale pin coverage lives in
        // `validation_refuses_before_connecting`.
        let mut pooled_port = production_plan();
        pooled_port.expected_host = "prod-host.invalid".to_owned();
        pooled_port.url =
            Some("postgres://u@prod-host.invalid:6432/two_bot?sslmode=require".to_owned());
        let err = validate_request(&pooled_port).unwrap_err().to_string();
        assert!(
            err.contains("direct"),
            "pooled port must name the direct endpoint, got: {err}"
        );
        let err = verify_target(&pooled_port, TlsPolicy::Required)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("direct endpoint"),
            "pooled port must name the direct endpoint, got: {err}"
        );
        // Dedicated-PgBouncer logins carry a `|name` suffix on the username;
        // SQLx percent-decodes it, so the encoded form refuses the same way.
        let mut pooled_login = production_plan();
        pooled_login.expected_host = "prod-host.invalid".to_owned();
        pooled_login.url = Some(
            "postgres://u%7Cpgbouncer@prod-host.invalid:5432/two_bot?sslmode=require".to_owned(),
        );
        let err = validate_request(&pooled_login).unwrap_err().to_string();
        assert!(
            err.contains("direct"),
            "pooled login must name the direct login, got: {err}"
        );
        let err = verify_target(&pooled_login, TlsPolicy::Required)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("direct login"),
            "pooled login must name the direct login, got: {err}"
        );
        // The exact probe from review: a production binding copied from the
        // app's pooled PlanetScale string (same host, port 6432, no `-pooler`
        // hostname) refuses in both gates.
        for pooled in [
            "postgresql://u@abc-useast1-1.horizon.psdb.cloud:6432/two_bot?sslmode=require",
            "postgresql://u%7Cpool@abc-useast1-1.horizon.psdb.cloud:6432/two_bot?sslmode=require",
        ] {
            let mut req = production_plan();
            req.expected_host = "abc-useast1-1.horizon.psdb.cloud".to_owned();
            req.url = Some(pooled.to_owned());
            assert!(
                matches!(validate_request(&req), Err(RunError::Refused(_))),
                "production validate must refuse pooled PlanetScale binding {pooled}"
            );
            assert!(
                matches!(
                    verify_target(&req, TlsPolicy::Required),
                    Err(RunError::Refused(_))
                ),
                "production verify must refuse pooled PlanetScale binding {pooled}"
            );
        }
        // The direct endpoint on the same host still verifies.
        let mut direct = production_plan();
        direct.expected_host = "prod-host.invalid".to_owned();
        direct.url = Some("postgres://u@prod-host.invalid:5432/two_bot?sslmode=require".to_owned());
        assert!(validate_request(&direct).is_ok());
        assert!(verify_target(&direct, TlsPolicy::Required).is_ok());
    }

    #[test]
    fn production_planetscale_branch_pin_mirrors_staging() {
        // A PlanetScale pin without the branch id refuses in both gates: the
        // refusal must name the production pin and never
        // `--staging-branch-id`, which production rejects.
        let mut req = production_plan();
        req.expected_host = "abc-useast1-1.horizon.psdb.cloud".to_owned();
        req.expected_database = "postgres".to_owned();
        req.url = Some(
            "postgresql://migrator@abc-useast1-1.horizon.psdb.cloud:5432/postgres?sslmode=require"
                .to_owned(),
        );
        for gate in [
            validate_request(&req),
            verify_target(&req, TlsPolicy::Required).map(|_| ()),
        ] {
            let err = gate.unwrap_err().to_string();
            assert!(
                err.contains("production branch pin"),
                "production PlanetScale refusal must name the production pin, got: {err}"
            );
            assert!(
                !err.contains("--staging-branch-id"),
                "production refusal must not name the staging pin, got: {err}"
            );
        }
        // A direct endpoint with the matching `{role}.{branch_id}` username
        // passes both gates.
        let pinned = Request {
            url: Some(
                "postgresql://migrator.cnfixture01@abc-useast1-1.horizon.psdb.cloud:5432/postgres?sslmode=require"
                    .to_owned(),
            ),
            expected_branch_id: "cnfixture01".to_owned(),
            ..req.clone()
        };
        assert!(validate_request(&pinned).is_ok());
        assert!(verify_target(&pinned, TlsPolicy::Required).is_ok());
        // Pooled shapes and a wrong branch refuse in both gates even when the
        // pins match them.
        for (url, branch) in [
            (
                "postgresql://migrator.cnfixture01@abc-useast1-1.horizon.psdb.cloud:6432/postgres?sslmode=require",
                "cnfixture01",
            ),
            (
                "postgresql://migrator.cnfixture01%7Cread-bouncer@abc-useast1-1.horizon.psdb.cloud:6432/postgres?sslmode=require",
                "cnfixture01",
            ),
            (
                "postgresql://migrator.otherbranch@abc-useast1-1.horizon.psdb.cloud:5432/postgres?sslmode=require",
                "cnfixture01",
            ),
        ] {
            let req = Request {
                url: Some(url.to_owned()),
                expected_branch_id: branch.to_owned(),
                ..req.clone()
            };
            assert!(
                matches!(validate_request(&req), Err(RunError::Refused(_))),
                "validate must refuse {url}"
            );
            assert!(
                matches!(
                    verify_target(&req, TlsPolicy::Required),
                    Err(RunError::Refused(_))
                ),
                "verify must refuse {url}"
            );
        }
        // A malformed pin refuses before any DDL. The message names the
        // production pin shape, never the staging flag.
        let bad = Request {
            expected_branch_id: "not a branch!".to_owned(),
            ..pinned.clone()
        };
        let err = validate_request(&bad).unwrap_err().to_string();
        assert!(err.contains("production"), "{err}");
        assert!(!err.contains("--staging-branch-id"), "{err}");
    }

    #[test]
    fn production_keeps_every_other_refusal() {
        let prod_plan = production_plan();
        // Bad source SHA refuses on both targets.
        for sha in [
            "main".to_owned(),
            "ABCDEF".repeat(10),
            "a".repeat(39),
            "g".repeat(40),
        ] {
            let mut req = prod_plan.clone();
            req.source_sha = sha.to_owned();
            assert!(matches!(validate_request(&req), Err(RunError::Refused(_))));
            let mut staging = staging_plan();
            staging.source_sha = sha.to_owned();
            assert!(matches!(
                validate_request(&staging),
                Err(RunError::Refused(_))
            ));
        }
        // Empty / non-bare pins refuse on both targets.
        for (host, db) in [
            ("", "two_bot"),
            ("prod-host.invalid", ""),
            ("a@b", "two_bot"),
            ("prod-host.invalid", "a/b"),
        ] {
            for mut req in [prod_plan.clone(), staging_plan()] {
                req.expected_host = host.to_owned();
                req.expected_database = db.to_owned();
                assert!(matches!(validate_request(&req), Err(RunError::Refused(_))));
            }
        }
        // Pooler pins refuse on both targets.
        for mut req in [prod_plan.clone(), staging_plan()] {
            req.expected_host = "ep-test-pooler.us-east-2.aws.neon.tech".to_owned();
            assert!(matches!(validate_request(&req), Err(RunError::Refused(_))));
        }
        // Missing bindings name the target's own env on both targets and modes.
        for (req, want) in [
            (
                Request {
                    url: None,
                    ..prod_plan.clone()
                },
                PROD_PLAN_URL_ENV,
            ),
            (
                Request {
                    url: None,
                    ..bound_production_apply(&pending_versions(), &"a".repeat(40))
                },
                PROD_URL_ENV,
            ),
            (
                Request {
                    url: None,
                    ..staging_plan()
                },
                PLAN_URL_ENV,
            ),
        ] {
            let err = validate_request(&req).unwrap_err().to_string();
            assert!(err.contains(want), "must name {want}, got: {err}");
        }
        // Apply without the reviewed pending list or the plan binding refuses.
        for good in [
            bound_production_apply(&pending_versions(), &"a".repeat(40)),
            bound_apply(&pending_versions(), &"a".repeat(40)),
        ] {
            for mutate in [
                |r: &mut Request| r.expected_pending = None,
                |r: &mut Request| r.plan_manifest_sha256 = None,
                |r: &mut Request| r.plan_run_id = None,
                |r: &mut Request| r.plan_manifest_path = None,
            ] {
                let mut req = good.clone();
                mutate(&mut req);
                assert!(matches!(validate_request(&req), Err(RunError::Refused(_))));
            }
        }
        // Binding mismatch refuses on both targets. The URLs carry
        // `sslmode=require` so the TLS fence passes and the pin-mismatch
        // fence is what fires.
        let mut prod_mismatch = prod_plan.clone();
        prod_mismatch.url =
            Some("postgres://u@other.invalid:5432/two_bot?sslmode=require".to_owned());
        assert!(matches!(
            verify_target(&prod_mismatch, TlsPolicy::Required),
            Err(RunError::Refused(_))
        ));
        let mut staging_mismatch = staging_plan();
        staging_mismatch.url =
            Some("postgres://u@other.invalid:5432/two_staging?sslmode=require".to_owned());
        assert!(matches!(
            verify_target(&staging_mismatch, TlsPolicy::Required),
            Err(RunError::Refused(_))
        ));
    }

    /// Threat-model F6 refusal pins for the migration target pool: a remote
    /// plaintext URL (and the other refusal cases) fails with a fixed string
    /// before SQLx parses the URL or opens a socket, with no URL part in the
    /// error. Hermetic: every case is refused, so no connection is attempted
    /// and no database is needed.
    #[test]
    fn target_tls_fence_refuses_plaintext_and_wrong_hosts() {
        let cases = [
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=disable",
                "ep-fixture-host.us-east-2.aws.neon.tech",
                "fixture-db",
                TlsPolicy::Required,
                "database sslmode does not require TLS",
            ),
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=prefer",
                "ep-fixture-host.us-east-2.aws.neon.tech",
                "fixture-db",
                TlsPolicy::Required,
                "database sslmode does not require TLS",
            ),
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db",
                "ep-fixture-host.us-east-2.aws.neon.tech",
                "fixture-db",
                TlsPolicy::Required,
                "database URL must set sslmode under the required TLS policy",
            ),
            (
                "postgres://fixture-user:fixture-db-password@agent-testdb:5432/fixture-db?sslmode=verify-full",
                "agent-testdb",
                "fixture-db",
                TlsPolicy::Required,
                "local database host is refused under the required TLS policy",
            ),
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=verify-full",
                "ep-fixture-host.us-east-2.aws.neon.tech",
                "fixture-db",
                TlsPolicy::LocalOnly,
                "remote database host is refused under the local-only TLS policy",
            ),
            (
                "postgres://fixture-user:fixture-db-password@fixture-host/fixture-db?sslmode=fixture-mode",
                "fixture-host",
                "fixture-db",
                TlsPolicy::LocalOnly,
                "unsupported database sslmode",
            ),
        ];
        for (url, host, database, policy, expected) in cases {
            let req = Request {
                url: Some(url.to_owned()),
                target: Target::Staging,
                source_sha: "a".repeat(40),
                expected_host: host.to_owned(),
                expected_database: database.to_owned(),
                expected_branch_id: String::new(),
                recovery_evidence_ref: "TOG-1#doc".to_owned(),
                acl_plan_ref: "TOG-2#doc".to_owned(),
                apply: false,
                expected_pending: None,
                expected_store_pending: None,
                plan_manifest_sha256: None,
                store_plan_manifest_sha256: None,
                plan_run_id: None,
                plan_manifest_path: None,
            };
            // The TLS fence runs before the pin comparison, so the exact
            // fixed string proves the TLS fence (not a pin fence) fired.
            let error = verify_target(&req, policy).unwrap_err().to_string();
            assert!(error.contains(expected), "{url} got: {error}");
            assert!(!error.contains("fixture"), "TLS refusal echoed the URL");
            let debug = format!("{:?}", verify_target(&req, policy).unwrap_err());
            assert!(!debug.contains("fixture"), "TLS refusal echoed the URL");
        }
    }

    /// Threat-model F6 happy path: preserve the effective TLS mode without
    /// capping migration statements in either plan or apply. A lazy pool opens
    /// no socket, so no database is needed.
    #[tokio::test]
    async fn target_fence_applies_tls_without_capping_migration_statements() {
        use sqlx::postgres::{PgPoolOptions, PgSslMode};
        // `LocalOnly` loopback keeps the URL mode as written.
        let local = Request {
            url: Some(
                "postgres://fixture:fixture-password@127.0.0.1:5432/fixture?sslmode=disable"
                    .to_owned(),
            ),
            target: Target::Staging,
            source_sha: "a".repeat(40),
            expected_host: "127.0.0.1".to_owned(),
            expected_database: "fixture".to_owned(),
            expected_branch_id: String::new(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
        };
        // `Required` upgrades a remote `require` URL to `verify-full`.
        let remote = Request {
            url: Some(
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech:5432/fixture-db?sslmode=require"
                    .to_owned(),
            ),
            expected_host: "ep-fixture-host.us-east-2.aws.neon.tech".to_owned(),
            expected_database: "fixture-db".to_owned(),
            ..local.clone()
        };
        for (request, policy, expected_mode) in [
            (&local, TlsPolicy::LocalOnly, PgSslMode::Disable),
            (&remote, TlsPolicy::Required, PgSslMode::VerifyFull),
        ] {
            for apply in [false, true] {
                let mut req = request.clone();
                req.apply = apply;
                let options = verify_target(&req, policy).unwrap();
                // `PgSslMode` has no `PartialEq`; all variants are fieldless, so
                // discriminant equality is exact equality.
                assert_eq!(
                    std::mem::discriminant(&options.get_ssl_mode()),
                    std::mem::discriminant(&expected_mode)
                );
                assert_eq!(options.get_options(), None);

                // Keep an explicit operator budget, rather than overwriting
                // it with an application-pool deadline (or disabling it).
                req.url
                    .as_mut()
                    .unwrap()
                    .push_str("&options%5Bstatement_timeout%5D=90000");
                let options = verify_target(&req, policy).unwrap();
                // `PgSslMode` has no `PartialEq`; all variants are fieldless, so
                // discriminant equality is exact equality.
                assert_eq!(
                    std::mem::discriminant(&options.get_ssl_mode()),
                    std::mem::discriminant(&expected_mode)
                );
                assert_eq!(options.get_options(), Some("-c statement_timeout=90000"));
            }
        }
        let options = verify_target(&remote, TlsPolicy::Required).unwrap();
        // A lazy pool from fenced options opens no socket.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(options);
        assert_eq!(pool.size(), 0);
        pool.close().await;
    }

    /// Target pool shape: max 2 connections and a 10 s acquire timeout.
    #[test]
    fn target_pool_shape_pins_acquire_timeout() {
        assert_eq!(TARGET_POOL_MAX, 2);
        assert_eq!(TARGET_ACQUIRE_TIMEOUT_SECS, 10);
    }

    /// Staging drift witness: run 38654195597 applied all 19 pending migrations, then the
    /// post-apply ledger read failed on a transient pool acquire while the
    /// saturated staging DB (25/25 connections) refused the new checkout. The
    /// pre-apply read had used the identical query as the identical role, so
    /// the failure was transient connection state, not grants or migration
    /// content. The post-apply read must retry transient failures before
    /// reporting the ledger unreadable.
    #[tokio::test]
    async fn post_apply_read_retries_transient_failures_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&calls);
        let result = retry_transient(
            || {
                let probe = Arc::clone(&probe);
                async move {
                    let n = probe.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(ReadAfterApply::PoolAcquireFailed)
                    } else {
                        Ok(42u32)
                    }
                }
            },
            5,
            Duration::from_millis(0),
        )
        .await;
        assert_eq!(result, Ok(42u32));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// A permanently unreadable ledger still reports unreadable (fail closed),
    /// with the terminal failure kind preserved for evidence.
    #[tokio::test]
    async fn post_apply_read_returns_the_terminal_failure_kind() {
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&calls);
        let result = retry_transient(
            || {
                let probe = Arc::clone(&probe);
                async move {
                    probe.fetch_add(1, Ordering::SeqCst);
                    Err::<u32, ReadAfterApply>(ReadAfterApply::LedgerQueryFailed)
                }
            },
            3,
            Duration::from_millis(0),
        )
        .await;
        assert_eq!(result, Err(ReadAfterApply::LedgerQueryFailed));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn post_apply_read_error_names_are_stable() {
        assert_eq!(
            ReadAfterApply::PoolAcquireFailed.as_str(),
            "pool_acquire_failed"
        );
        assert_eq!(
            ReadAfterApply::LedgerQueryFailed.as_str(),
            "ledger_query_failed"
        );
    }

    /// Staging drift witness: staging refused all logins at 25/25 connections. The plan
    /// manifest must carry connection usage and warn strictly above 80%.
    #[test]
    fn connection_warning_fires_only_above_eighty_percent() {
        assert_eq!(connection_warning(20, 25), None);
        assert_eq!(connection_warning(80, 100), None);
        assert_eq!(connection_warning(0, 0), None);
        assert_eq!(connection_warning(5, 0), None);
        for (used, max) in [(21, 25), (81, 100), (25, 25)] {
            let line =
                connection_warning(used, max).unwrap_or_else(|| panic!("{used}/{max} must warn"));
            assert!(line.contains("80%"), "{line}");
            assert!(line.contains("::warning::"), "{line}");
        }
    }

    #[test]
    fn connection_warning_from_audit_reads_usage_fields() {
        let saturated = json!({"connection_usage": {"used": 21, "max_connections": 25}});
        let line = connection_warning_from_audit(&saturated).expect("must warn");
        assert!(line.contains("21/25"), "{line}");
        assert_eq!(
            connection_warning_from_audit(
                &json!({"connection_usage": {"used": 20, "max_connections": 25}})
            ),
            None
        );
        // A failed audit readout warns about nothing.
        assert_eq!(
            connection_warning_from_audit(&json!({"error": "audit read failed"})),
            None
        );
        assert_eq!(connection_warning_from_audit(&Value::Null), None);
    }

    #[test]
    fn connection_count_uses_fully_readable_source() {
        // `backend_type` is NULL for other roles' sessions unless the reader
        // holds pg_read_all_stats, which the plan login does not; the count
        // must come from pg_stat_database, readable in full by every login.
        assert!(
            CONNECTION_COUNT_SQL.contains("pg_stat_database"),
            "{CONNECTION_COUNT_SQL}"
        );
        assert!(
            CONNECTION_COUNT_SQL.contains("numbackends"),
            "{CONNECTION_COUNT_SQL}"
        );
        assert!(
            !CONNECTION_COUNT_SQL.contains("backend_type"),
            "{CONNECTION_COUNT_SQL}"
        );
    }

    #[test]
    fn effective_ceiling_subtracts_reserved() {
        assert_eq!(effective_max_connections(25, 3), 22);
        assert_eq!(effective_max_connections(25, 0), 25);
        assert_eq!(effective_max_connections(1, 5), 1);
        // Backwards-compatible: manifests without the reserved field warn on raw max.
        assert_eq!(
            connection_warning_from_audit(
                &json!({"connection_usage": {"used": 20, "max_connections": 25}})
            ),
            None
        );
        // Same reading against the login ceiling (25 - 3 = 22) warns: 20/22 > 80%.
        let line = connection_warning_from_audit(
            &json!({"connection_usage": {"used": 20, "max_connections": 25, "reserved_connections": 3}}),
        )
        .expect("must warn on effective ceiling");
        assert!(line.contains("20/22"), "{line}");
    }

    /// The `run` entry refuses a plaintext remote binding before connecting
    /// under either TLS policy (unset means `Required`, which refuses the weak
    /// sslmode; `local-only` refuses the remote host), so no socket is opened
    /// and no database is needed regardless of the ambient setting.
    #[tokio::test]
    async fn run_refuses_plaintext_remote_binding_before_connect() {
        let req = Request {
            url: Some(
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech:5432/fixture-db?sslmode=disable"
                    .to_owned(),
            ),
            target: Target::Staging,
            source_sha: "a".repeat(40),
            expected_host: "ep-fixture-host.us-east-2.aws.neon.tech".to_owned(),
            expected_database: "fixture-db".to_owned(),
            expected_branch_id: String::new(),
            recovery_evidence_ref: "TOG-1#doc".to_owned(),
            acl_plan_ref: "TOG-2#doc".to_owned(),
            apply: false,
            expected_pending: None,
            expected_store_pending: None,
            plan_manifest_sha256: None,
            store_plan_manifest_sha256: None,
            plan_run_id: None,
            plan_manifest_path: None,
        };
        let err = run(&req).await.unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.contains("database sslmode does not require TLS")
                || shown.contains("remote database host is refused"),
            "TLS fence must fire before connect, got: {shown}"
        );
        assert!(
            !shown.contains("fixture"),
            "refusal echoed the URL: {shown}"
        );
        let debug = format!("{err:?}");
        assert!(
            !debug.contains("fixture"),
            "refusal echoed the URL: {debug}"
        );
    }

    /// Stand-in database error whose Display carries a fake secret: proves
    /// the connect refusal never surfaces it.
    #[derive(Debug)]
    struct FakeConnectDbError {
        code: Option<&'static str>,
        transient: bool,
    }

    impl std::fmt::Display for FakeConnectDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake database error with postgres://secret@localhost/db")
        }
    }

    impl std::error::Error for FakeConnectDbError {}

    impl sqlx::error::DatabaseError for FakeConnectDbError {
        fn message(&self) -> &str {
            "fake"
        }

        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            self.code.map(std::borrow::Cow::Borrowed)
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn is_transient_in_connect_phase(&self) -> bool {
            self.transient
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn connect_database(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeConnectDbError {
            code: Some(code),
            transient: false,
        }))
    }

    fn connect_io(kind: std::io::ErrorKind) -> sqlx::Error {
        sqlx::Error::Io(std::io::Error::new(kind, "simulated transport failure"))
    }

    /// A simulated connect timeout and a real `SET ROLE` denial (SQLSTATE
    /// 42501) surface distinct cause strings, so the next recurrence is
    /// triageable from the log line.
    #[test]
    fn connect_refusal_distinguishes_timeout_from_set_role_denial() {
        let timeout = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "simulated connect timeout",
        ));
        let denied = connect_database("42501");

        let timeout_cause = connect_cause(&timeout);
        let denied_cause = connect_cause(&denied);
        assert_eq!(timeout_cause, "connect-timeout", "timeout cause");
        assert_eq!(denied_cause, "set-role-denied:42501", "denial cause");
        assert_ne!(timeout_cause, denied_cause, "causes must differ");

        for (error, cause) in [
            (&timeout, timeout_cause.as_str()),
            (&denied, denied_cause.as_str()),
        ] {
            let refusal = connect_refusal(READ_ONLY_ROLE, error);
            assert!(
                matches!(refusal, RunError::Refused(_)),
                "connect failure must refuse, got cause {cause}"
            );
            let RunError::Refused(message) = refusal else {
                continue;
            };
            assert!(
                message.contains("could not connect and SET ROLE"),
                "refusal must keep the historic prefix: {message}"
            );
            assert!(
                message.contains(READ_ONLY_ROLE),
                "refusal must name the role: {message}"
            );
            assert!(
                message.contains(&format!("[cause={cause}]")),
                "refusal must carry the cause: {message}"
            );
            assert!(
                !message.contains("secret"),
                "refusal leaked error detail: {message}"
            );
        }
    }

    #[test]
    fn connect_cause_covers_auth_refused_reset_and_pool_classes() {
        assert_eq!(
            connect_cause(&connect_io(std::io::ErrorKind::ConnectionRefused)),
            "connect-refused"
        );
        assert_eq!(
            connect_cause(&connect_io(std::io::ErrorKind::ConnectionReset)),
            "connect-reset"
        );
        assert_eq!(
            connect_cause(&connect_io(std::io::ErrorKind::HostUnreachable)),
            "connect-unreachable"
        );
        assert_eq!(
            connect_cause(&connect_io(std::io::ErrorKind::PermissionDenied)),
            "connect-permission"
        );
        assert_eq!(
            connect_cause(&connect_io(std::io::ErrorKind::InvalidData)),
            "io-error"
        );
        assert_eq!(connect_cause(&connect_database("28P01")), "auth:28P01");
        assert_eq!(connect_cause(&connect_database("28000")), "auth:28000");
        let transient = sqlx::Error::Database(Box::new(FakeConnectDbError {
            code: Some("53300"),
            transient: true,
        }));
        assert_eq!(connect_cause(&transient), "connect-transient:53300");
        assert_eq!(connect_cause(&connect_database("XX000")), "database:XX000");
        let codeless = sqlx::Error::Database(Box::new(FakeConnectDbError {
            code: None,
            transient: false,
        }));
        assert_eq!(connect_cause(&codeless), "database:unknown");
        assert_eq!(connect_cause(&sqlx::Error::PoolTimedOut), "pool-timeout");
        assert_eq!(connect_cause(&sqlx::Error::PoolClosed), "pool-closed");
        let tls = sqlx::Error::Tls(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "simulated tls failure",
        )));
        assert_eq!(connect_cause(&tls), "tls");
        assert_eq!(
            connect_cause(&sqlx::Error::Protocol(
                "expected role two_bot_migrator_ro is not active".to_owned()
            )),
            "role-verify"
        );
        assert_eq!(
            connect_cause(&sqlx::Error::Protocol("garbage".to_owned())),
            "connect-error"
        );
        assert_eq!(
            connect_cause(&sqlx::Error::InvalidArgument("literal step".to_owned())),
            "connect-error"
        );
        assert_eq!(connect_cause(&sqlx::Error::WorkerCrashed), "connect-error");
    }

    /// `connect()` through a real pool against a closed loopback port refuses
    /// with the documented pool-deadline cause: sqlx retries the refused port
    /// until the acquire deadline, so the pool path can only report
    /// `pool-timeout` here. Fails if the `connect_refusal` wiring is reverted
    /// to the fixed string (no `[cause=...]` suffix). Costs one acquire
    /// deadline (~10 s): the production timeout, not test overhead.
    #[tokio::test]
    async fn connect_refuses_closed_port_with_pool_timeout_cause() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("loopback bind")
            .local_addr()
            .expect("loopback addr")
            .port();
        // Listener dropped here: the port is closed when `connect` dials it.
        let options = PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(port)
            .username("staging_migrate_cause_probe")
            .database("staging_migrate_cause_probe")
            .ssl_mode(sqlx::postgres::PgSslMode::Disable);
        let verified = Arc::new(AtomicUsize::new(0));
        let outcome = connect(options, READ_ONLY_ROLE, Arc::clone(&verified)).await;
        let error = outcome.expect_err("closed port must refuse");
        assert!(
            matches!(error, RunError::Refused(_)),
            "closed port must refuse fail-closed, got {error:?}"
        );
        let RunError::Refused(message) = error else {
            return;
        };
        assert!(
            message.contains("[cause=pool-timeout]"),
            "closed port must document the pool-deadline cause: {message}"
        );
        assert!(
            message.contains("could not connect and SET ROLE"),
            "refusal must keep the historic prefix: {message}"
        );
        assert_eq!(
            verified.load(Ordering::SeqCst),
            0,
            "refused connections must not verify"
        );
    }

    /// `connect()` as a login that is not a member of the role refuses with
    /// `set-role-denied:42501`, not the pool-deadline cause: the
    /// `after_connect` slot preserves the `SET ROLE` denial sqlx retries away.
    /// Requires `TWO_TEST_DATABASE_URL` (agent-testdb or the CI service);
    /// creates one login with a unique name and drops it before asserting, so
    /// a red test does not leak the login. Costs one acquire deadline (~10 s):
    /// the production retry budget a persistent denial always pays.
    #[tokio::test]
    async fn connect_records_set_role_denial_behind_pool_timeout() {
        use sqlx::Connection as _;
        let raw = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(raw) => raw,
            Err(error) => {
                assert!(
                    matches!(error, std::env::VarError::NotPresent),
                    "invalid test bootstrap configuration: {error}"
                );
                eprintln!("skipped: TWO_TEST_DATABASE_URL not set");
                return;
            }
        };
        two_bot_testsupport::guard_database_url(&raw).expect("test database only");
        let admin_options: PgConnectOptions = raw
            .parse()
            .expect("TWO_TEST_DATABASE_URL parses as Postgres options");
        let admin_options = admin_options.ssl_mode(sqlx::postgres::PgSslMode::Disable);
        let mut admin = PgConnection::connect_with(&admin_options)
            .await
            .expect("agent-testdb admin connect");
        // The denial must be a real 42501, so the groups exist (idempotent,
        // shared with other suites) while the probe login holds nothing.
        for group in [MIGRATOR_ROLE, READ_ONLY_ROLE] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DO $$ BEGIN CREATE ROLE {group} NOLOGIN; \
                 EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$"
            )))
            .execute(&mut admin)
            .await
            .expect("shared test group exists");
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let login = format!("sm_cause_login_{}_{nonce}", std::process::id());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE ROLE {login} LOGIN")))
            .execute(&mut admin)
            .await
            .expect("probe login created");
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&mut admin)
            .await
            .expect("current database");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT CONNECT ON DATABASE {database} TO {login}"
        )))
        .execute(&mut admin)
        .await
        .expect("probe login can connect");
        let probe_options = admin_options.username(&login);
        let verified = Arc::new(AtomicUsize::new(0));
        let outcome = connect(probe_options, READ_ONLY_ROLE, Arc::clone(&verified)).await;
        // Cleanup before asserting, so a red test does not leak the login.
        // The CONNECT grant blocks DROP ROLE (2BP01) until revoked.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "REVOKE CONNECT ON DATABASE {database} FROM {login}"
        )))
        .execute(&mut admin)
        .await
        .expect("probe login connect revoked");
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {login}")))
            .execute(&mut admin)
            .await
            .expect("probe login dropped");
        admin.close().await.expect("admin close");
        let error = outcome.expect_err("non-member SET ROLE must refuse");
        assert!(
            matches!(error, RunError::Refused(_)),
            "non-member SET ROLE must refuse fail-closed, got {error:?}"
        );
        let RunError::Refused(message) = error else {
            return;
        };
        assert!(
            message.contains("[cause=set-role-denied:42501]"),
            "non-member SET ROLE must surface the denial cause: {message}"
        );
        assert_eq!(
            verified.load(Ordering::SeqCst),
            0,
            "denied connections must not verify"
        );
    }

    #[test]
    fn audit_names_the_four_logins_and_renders_the_compiled_verifier() {
        for login in [
            "two_bot_migrator",
            "two_bot_migrator_ro",
            "two_bot_migrator_ro_plan",
            "two_bot_migrator_apply",
        ] {
            assert!(AUDIT_MEMBERSHIP_SQL.contains(&format!("'{login}'")));
        }
        assert!(AUDIT_MEMBERSHIP_SQL.contains("LEFT JOIN pg_roles"));
        assert!(AUDIT_MEMBERSHIP_SQL.contains("pg_has_role"));
        assert!(AUDIT_MEMBERSHIP_SQL.contains("ORDER BY e.login"));
        assert!(AUDIT_MATRIX.contains("'public', '_sqlx_migrations', 'ledger'"));
        assert!(AUDIT_VERIFY.contains("SELECT finding FROM findings ORDER BY finding;"));
        assert!(AUDIT_VERIFY.contains("-- @matrix"));
        let rendered = AUDIT_VERIFY.replace("-- @matrix", AUDIT_MATRIX);
        assert!(!rendered.contains("-- @matrix"));
        assert!(rendered.contains("'public', '_sqlx_migrations', 'ledger'"));
    }
}
